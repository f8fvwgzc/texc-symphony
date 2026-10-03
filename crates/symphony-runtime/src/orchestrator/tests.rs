//! Orchestrator state-machine tests (ports of `core_test.exs` / `orchestrator_status_test.exs`, B.13.2
//! and B.13.3, plus the Rust improvements). Workers are fakes from [`FnWorkerFactory`]; timing tests
//! run on tokio's paused clock, tests that run hook subprocesses use real time.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use symphony_codex::{CodexError, CodexEvent, CodexEventData, CodexEventKind, StreamMessage};
use symphony_core::Issue;
use symphony_store::{RunQuery, RunStatus, Store};
use symphony_trackers::{MemoryIssues, MemoryTracker, TrackerDeps};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::runtime::{RuntimeContext, RuntimeOptions, WorkerSet};
use crate::test_support::{TestWorkflow, issue, read};
use crate::worker::{FnWorkerFactory, RunError, WorkerContext, WorkerFactory};

/// Records `(issue id, attempt, worker host)` of every started worker.
type Started = Arc<Mutex<Vec<(String, Option<u32>, Option<String>)>>>;

// ----- harness -------------------------------------------------------------------------------------

struct Harness {
    wf: TestWorkflow,
    memory: MemoryIssues,
    ctx: Arc<RuntimeContext>,
    _cmd_tx: mpsc::Sender<RuntimeCommand>,
    cmd_rx: Option<mpsc::Receiver<RuntimeCommand>>,
    generation: watch::Receiver<u64>,
}

impl Harness {
    fn new(config: Value, factory: Arc<dyn WorkerFactory>) -> Self {
        Self::with_store(config, factory, Store::disabled())
    }

    fn with_store(config: Value, factory: Arc<dyn WorkerFactory>, store: Store) -> Self {
        Self::build(config, factory, |options| options.with_store(store))
    }

    fn build(
        config: Value,
        factory: Arc<dyn WorkerFactory>,
        customize: impl FnOnce(RuntimeOptions) -> RuntimeOptions,
    ) -> Self {
        let wf = TestWorkflow::new(config);
        let memory = MemoryIssues::new();
        let options = RuntimeOptions::new(Arc::clone(&wf.store), TrackerDeps::new().unwrap())
            .with_tracker(Arc::new(MemoryTracker::new(memory.clone())))
            .with_worker_factory(factory);
        let mut options = customize(options);
        options.worker_cancel_grace = Duration::from_millis(500);
        let (gen_tx, generation) = watch::channel(0);
        let ctx = Arc::new(RuntimeContext::from_options(&options, gen_tx));
        let (cmd_tx, cmd_rx) = mpsc::channel(8);
        Self {
            wf,
            memory,
            ctx,
            _cmd_tx: cmd_tx,
            cmd_rx: Some(cmd_rx),
            generation,
        }
    }

    fn orchestrator<'w>(&mut self, workers: &'w mut WorkerSet) -> Orchestrator<'w> {
        let rx = self.cmd_rx.take().expect("one orchestrator per harness");
        Orchestrator::new(Arc::clone(&self.ctx), rx, workers, CancellationToken::new())
    }
}

/// Steps until `done` holds (bounded number of events).
async fn run_until(orch: &mut Orchestrator<'_>, done: impl Fn(&Orchestrator<'_>) -> bool) {
    for _ in 0..500 {
        if done(orch) {
            return;
        }
        assert!(orch.step().await, "orchestrator stopped");
    }
    panic!("condition not reached");
}

/// Worker that records its start, then waits for cancellation.
fn blocking_factory(started: Started) -> Arc<dyn WorkerFactory> {
    Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        started.lock().unwrap().push((
            ctx.issue.id.clone().unwrap_or_default(),
            ctx.attempt,
            ctx.worker_host.clone(),
        ));
        async move {
            ctx.cancel.cancelled().await;
            Err(RunError::Cancelled)
        }
    }))
}

fn result_factory(result: fn() -> WorkerResult) -> Arc<dyn WorkerFactory> {
    Arc::new(FnWorkerFactory(move |_ctx: WorkerContext| async move {
        result()
    }))
}

/// Worker that emits `events` and then waits for cancellation.
fn emitting_factory(events: Vec<CodexEvent>) -> Arc<dyn WorkerFactory> {
    Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        let events = events.clone();
        async move {
            for event in events {
                ctx.events.emit(event);
            }
            ctx.cancel.cancelled().await;
            Err(RunError::Cancelled)
        }
    }))
}

fn notification(payload: Value) -> CodexEvent {
    let raw = payload.to_string();
    CodexEvent::new(
        CodexEventData::Notification(StreamMessage { payload, raw }),
        Some("4242".into()),
        None,
    )
}

fn session_started(session_id: &str) -> CodexEvent {
    CodexEvent::new(
        CodexEventData::SessionStarted {
            session_id: session_id.into(),
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        },
        Some("4242".into()),
        None,
    )
}

fn input_required() -> CodexEvent {
    let payload = json!({"method": "item/tool/requestUserInput", "id": 9, "params": {}});
    CodexEvent::new(
        CodexEventData::TurnInputRequired(StreamMessage {
            raw: payload.to_string(),
            payload,
        }),
        None,
        None,
    )
}

fn ms(duration_ms: u64) -> Duration {
    Duration::from_millis(duration_ms)
}

fn retry_due_in(orch: &Orchestrator<'_>, id: &str) -> u128 {
    orch.state.retry_attempts[id]
        .due_at
        .saturating_duration_since(Instant::now())
        .as_millis()
}

/// Puts a synthetic running entry (no real worker) into the state.
fn fake_entry(identifier: &str, issue: Issue, worker_host: Option<&str>) -> RunningEntry {
    let now = Instant::now();
    RunningEntry {
        run_id: 0,
        worker: None,
        identifier: identifier.into(),
        issue,
        worker_host: worker_host.map(Into::into),
        workspace_path: None,
        session_id: None,
        last_codex_message: None,
        last_codex_timestamp: None,
        last_codex_event: None,
        last_activity: now,
        codex_app_server_pid: None,
        tokens: Default::default(),
        turn_count: 0,
        retry_attempt: 0,
        started_at: Utc::now(),
        started: now,
        recorder: RunRecorder::default(),
    }
}

// ----- candidate selection and host choice ----------------------------------------------------------

#[tokio::test]
async fn dispatch_eligibility_follows_provider_routing_and_required_labels() {
    let mut h = Harness::new(
        json!({"agent": {"max_concurrent_agents": 3}, "tracker": {"required_labels": ["symphony", "javascript"]}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let orch = h.orchestrator(&mut workers);
    let settings = orch.settings();
    let sets = StateSets::from_settings(&settings);
    let labeled = |dispatchable: bool, labels: &[&str]| Issue {
        dispatchable,
        labels: labels.iter().map(|l| l.to_string()).collect(),
        ..issue("i-1", "MT-1", "Todo")
    };
    // provider-marked blocked / assigned elsewhere
    assert!(!orch.state.should_dispatch(
        &labeled(false, &["symphony", "javascript"]),
        &settings,
        &sets
    ));
    // missing required label
    assert!(
        !orch
            .state
            .should_dispatch(&labeled(true, &["symphony"]), &settings, &sets)
    );
    // ready
    assert!(orch.state.should_dispatch(
        &labeled(true, &["Symphony", "JavaScript"]),
        &settings,
        &sets
    ));
    // terminal or inactive states are never candidates
    let done = Issue {
        state: Some("Done".into()),
        ..labeled(true, &["symphony", "javascript"])
    };
    assert!(!orch.state.should_dispatch(&done, &settings, &sets));
    // blank title
    let blank = Issue {
        title: Some("  ".into()),
        ..labeled(true, &["symphony", "javascript"])
    };
    assert!(!orch.state.should_dispatch(&blank, &settings, &sets));
}

#[tokio::test]
async fn select_worker_host_respects_the_shared_per_host_cap() {
    let mut h = Harness::new(
        json!({"worker": {"ssh_hosts": ["worker-a", "worker-b"], "max_concurrent_agents_per_host": 1}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    let settings = orch.settings();
    orch.state.running.insert(
        "a".into(),
        fake_entry("MT-A", issue("a", "MT-A", "Todo"), Some("worker-a")),
    );
    assert_eq!(
        orch.state.select_worker_host(&settings, None),
        HostChoice::Host("worker-b".into())
    );
    orch.state.running.insert(
        "b".into(),
        fake_entry("MT-B", issue("b", "MT-B", "Todo"), Some("worker-b")),
    );
    assert_eq!(
        orch.state.select_worker_host(&settings, None),
        HostChoice::NoCapacity
    );
    assert!(!orch.state.should_dispatch(
        &issue("c", "MT-C", "Todo"),
        &settings,
        &StateSets::from_settings(&settings)
    ));
}

#[tokio::test]
async fn select_worker_host_keeps_the_preferred_host_when_it_has_capacity() {
    let mut h = Harness::new(
        json!({"worker": {"ssh_hosts": ["worker-a", "worker-b"], "max_concurrent_agents_per_host": 2}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    let settings = orch.settings();
    orch.state.running.insert(
        "a".into(),
        fake_entry("MT-A", issue("a", "MT-A", "Todo"), Some("worker-a")),
    );
    orch.state.running.insert(
        "b".into(),
        fake_entry("MT-B", issue("b", "MT-B", "Todo"), Some("worker-b")),
    );
    assert_eq!(
        orch.state.select_worker_host(&settings, Some("worker-a")),
        HostChoice::Host("worker-a".into())
    );
    assert_eq!(
        orch.state.select_worker_host(&settings, Some("unknown")),
        HostChoice::Host("worker-a".into())
    );
    let local = Harness::new(json!({}), result_factory(|| Ok(())));
    let settings = local.wf.store.settings();
    assert_eq!(
        orch.state.select_worker_host(&settings, None),
        HostChoice::Local
    );
}

#[tokio::test(start_paused = true)]
async fn per_state_and_global_limits_gate_dispatch() {
    let started = Arc::new(Mutex::new(Vec::new()));
    let mut h = Harness::new(
        json!({"agent": {"max_concurrent_agents": 3, "max_concurrent_agents_by_state": {"todo": 1}}}),
        blocking_factory(Arc::clone(&started)),
    );
    h.memory.set(vec![
        issue("t1", "MT-1", "Todo"),
        issue("t2", "MT-2", "Todo"),
        issue("p1", "MT-3", "In Progress"),
        issue("p2", "MT-4", "In Progress"),
        issue("p3", "MT-5", "In Progress"),
    ]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    let mut ids: Vec<String> = orch.state.running.keys().cloned().collect();
    ids.sort();
    assert_eq!(ids, ["p1", "p2", "t1"]);
    assert_eq!(started.lock().unwrap().len(), 3);
    assert!(orch.state.claimed.contains("t1"));
}

// ----- revalidation --------------------------------------------------------------------------------

#[tokio::test]
async fn revalidation_skips_when_routing_or_labels_change_and_reports_missing() {
    let mut h = Harness::new(
        json!({"tracker": {"required_labels": ["symphony"]}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let orch = h.orchestrator(&mut workers);
    let candidate = Issue {
        labels: vec!["symphony".into()],
        ..issue("i-1", "MT-1", "Todo")
    };
    let refreshed = Issue {
        dispatchable: false,
        blocked_by: vec![symphony_core::BlockerRef {
            id: Some("b".into()),
            identifier: Some("MT-0".into()),
            state: Some("In Progress".into()),
        }],
        ..candidate.clone()
    };
    h.memory.set(vec![refreshed.clone()]);
    match orch.revalidate_issue_for_dispatch(&candidate).await {
        Revalidation::Skip(issue) => assert_eq!(issue.blocked_by.len(), 1),
        other => panic!("unexpected {other:?}"),
    }
    let unlabeled = Issue {
        labels: vec![],
        ..candidate.clone()
    };
    h.memory.set(vec![unlabeled]);
    assert!(matches!(
        orch.revalidate_issue_for_dispatch(&candidate).await,
        Revalidation::Skip(_)
    ));
    h.memory.set(vec![]);
    assert!(matches!(
        orch.revalidate_issue_for_dispatch(&candidate).await,
        Revalidation::SkipMissing
    ));
    h.memory.set(vec![candidate.clone()]);
    assert!(matches!(
        orch.revalidate_issue_for_dispatch(&candidate).await,
        Revalidation::Ok(_)
    ));
}

// ----- worker exits and retries --------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn normal_worker_exit_schedules_active_state_continuation_retry() {
    let mut h = Harness::new(json!({}), result_factory(|| Ok(())));
    h.memory.set(vec![issue("i-1", "MT-558", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-558", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-1"));
    run_until(&mut orch, |o| o.state.retry_attempts.contains_key("i-1")).await;
    assert!(!orch.state.running.contains_key("i-1"));
    assert!(orch.state.completed.contains("i-1"));
    assert!(orch.state.claimed.contains("i-1"));
    let entry = &orch.state.retry_attempts["i-1"];
    assert_eq!(entry.attempt, 1);
    assert!((500..=1_100).contains(&retry_due_in(&orch, "i-1")));
}

#[tokio::test(start_paused = true)]
async fn abnormal_worker_exit_increments_retry_attempt_progressively() {
    let mut h = Harness::new(
        json!({}),
        result_factory(|| Err(RunError::Other("boom".into()))),
    );
    h.memory.set(vec![issue("i-1", "MT-559", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-559", "In Progress"), None, None)
        .await;

    // First abnormal exit waits 10 s.
    run_until(&mut orch, |o| o.state.retry_attempts.contains_key("i-1")).await;
    let entry = &orch.state.retry_attempts["i-1"];
    assert_eq!(entry.attempt, 1);
    assert_eq!(entry.error.as_deref(), Some("agent exited: boom"));
    assert!((9_000..=10_500).contains(&retry_due_in(&orch, "i-1")));

    // The retry dispatches attempt 1, which crashes into attempt 2 (20 s) and then 3 (40 s).
    run_until(&mut orch, |o| {
        o.state
            .retry_attempts
            .get("i-1")
            .is_some_and(|e| e.attempt == 2)
    })
    .await;
    assert!((19_000..=20_500).contains(&retry_due_in(&orch, "i-1")));
    run_until(&mut orch, |o| {
        o.state
            .retry_attempts
            .get("i-1")
            .is_some_and(|e| e.attempt == 3)
    })
    .await;
    let entry = &orch.state.retry_attempts["i-1"];
    assert_eq!(entry.identifier, "MT-559");
    assert_eq!(entry.error.as_deref(), Some("agent exited: boom"));
    assert!((39_500..=40_500).contains(&retry_due_in(&orch, "i-1")));
}

#[tokio::test(start_paused = true)]
async fn continuation_requeued_without_slots_waits_twenty_seconds() {
    let started = Arc::new(Mutex::new(Vec::new()));
    let mut h = Harness::new(
        json!({"agent": {"max_concurrent_agents": 1}}),
        blocking_factory(Arc::clone(&started)),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    // Occupy the only slot, then queue a continuation for another issue.
    h.memory.set(vec![
        issue("busy", "MT-B", "In Progress"),
        issue("i-1", "MT-1", "In Progress"),
    ]);
    orch.dispatch_issue(issue("busy", "MT-B", "In Progress"), None, None)
        .await;
    orch.schedule_issue_retry(
        "i-1",
        Some(1),
        RetryMeta::default(),
        DelayType::Continuation,
    );
    assert!((900..=1_000).contains(&retry_due_in(&orch, "i-1")));
    run_until(&mut orch, |o| {
        o.state
            .retry_attempts
            .get("i-1")
            .is_some_and(|e| e.attempt == 2)
    })
    .await;
    let entry = &orch.state.retry_attempts["i-1"];
    assert_eq!(
        entry.error.as_deref(),
        Some("no available orchestrator slots")
    );
    assert!((19_000..=20_000).contains(&retry_due_in(&orch, "i-1")));
}

#[tokio::test(start_paused = true)]
async fn panicking_workers_are_retried_like_failures() {
    let factory: Arc<dyn WorkerFactory> =
        Arc::new(FnWorkerFactory(|_ctx: WorkerContext| async move {
            panic!("worker exploded");
        }));
    let mut h = Harness::new(json!({}), factory);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-P", "In Progress"), None, None);
    run_until(&mut orch, |o| o.state.retry_attempts.contains_key("i-1")).await;
    let entry = &orch.state.retry_attempts["i-1"];
    assert_eq!(entry.attempt, 1);
    assert_eq!(
        entry.error.as_deref(),
        Some("agent exited: worker panicked: worker exploded")
    );
    assert!(orch.state.running.is_empty());
}

#[tokio::test(start_paused = true)]
async fn stale_retry_timer_messages_do_not_consume_newer_retry_entries() {
    let mut h = Harness::new(json!({}), result_factory(|| Ok(())));
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.schedule_issue_retry("i-1", Some(1), RetryMeta::default(), DelayType::Failure);
    let stale = orch.state.retry_attempts["i-1"].token;
    orch.schedule_issue_retry("i-1", Some(2), RetryMeta::default(), DelayType::Failure);
    let current = orch.state.retry_attempts["i-1"].token;
    assert_ne!(stale, current);
    orch.handle_retry_due("i-1", stale).await;
    let entry = &orch.state.retry_attempts["i-1"];
    assert_eq!(entry.attempt, 2);
    assert_eq!(entry.token, current);
}

#[tokio::test(start_paused = true)]
async fn retry_releases_its_claim_when_a_required_label_is_removed() {
    let mut h = Harness::new(
        json!({"tracker": {"required_labels": ["symphony"]}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.state.claimed.insert("i-1".into());
    orch.handle_retry_issue_lookup(
        Some(issue("i-1", "MT-1", "In Progress")),
        "i-1",
        1,
        RetryMeta::default(),
    )
    .await;
    assert!(!orch.state.claimed.contains("i-1"));
    assert!(!orch.state.retry_attempts.contains_key("i-1"));
    assert!(orch.state.running.is_empty());
}

#[tokio::test(start_paused = true)]
async fn retry_releases_its_claim_when_dispatch_revalidation_no_longer_finds_the_issue() {
    let mut h = Harness::new(
        json!({"hooks": {"before_run": "exit 1"}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.state.claimed.insert("i-1".into());
    // The tracker is empty, so the dispatch revalidation fetch returns [].
    orch.handle_retry_issue_lookup(
        Some(issue("i-1", "MT-1", "In Progress")),
        "i-1",
        1,
        RetryMeta::default(),
    )
    .await;
    assert!(!orch.state.claimed.contains("i-1"));
    assert!(orch.state.running.is_empty());
    assert!(orch.state.retry_attempts.is_empty());
}

#[tokio::test(start_paused = true)]
async fn retry_fired_for_a_missing_or_inactive_issue_releases_the_claim() {
    let mut h = Harness::new(json!({}), result_factory(|| Ok(())));
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.schedule_issue_retry("gone", Some(1), RetryMeta::default(), DelayType::Failure);
    run_until(&mut orch, |o| !o.state.retry_attempts.contains_key("gone")).await;
    assert!(!orch.state.claimed.contains("gone"));

    h.memory.set(vec![issue("backlog", "MT-2", "Backlog")]);
    orch.schedule_issue_retry("backlog", Some(1), RetryMeta::default(), DelayType::Failure);
    run_until(&mut orch, |o| {
        !o.state.retry_attempts.contains_key("backlog")
    })
    .await;
    assert!(!orch.state.claimed.contains("backlog"));
    assert!(orch.state.running.is_empty());
}

// ----- refresh and ticks ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn manual_refresh_coalesces_repeated_requests_and_ignores_superseded_ticks() {
    let mut h = Harness::new(json!({}), result_factory(|| Ok(())));
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.schedule_tick(ms(30_000));
    let stale = orch.state.tick_token.unwrap();

    let first = orch.request_refresh();
    assert!(first.queued);
    assert!(!first.coalesced);
    assert_eq!(first.operations, ["poll", "reconcile"]);
    let fresh = orch.state.tick_token.unwrap();
    assert_ne!(fresh, stale);
    assert!(orch.state.next_poll_due.unwrap() <= Instant::now());

    let second = orch.request_refresh();
    assert!(second.coalesced);
    assert_eq!(orch.state.tick_token, Some(fresh));

    orch.handle_tick(Some(stale));
    assert!(!orch.state.poll_check_in_progress);
    assert_eq!(orch.state.tick_token, Some(fresh));
}

#[tokio::test(start_paused = true)]
async fn triggers_an_immediate_poll_cycle_shortly_after_startup() {
    let mut h = Harness::new(
        json!({"polling": {"interval_ms": 5000}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    let generation = h.generation.clone();
    orch.startup().await;
    run_until(&mut orch, |o| o.state.poll_check_in_progress).await;
    let snap = orch.snapshot();
    assert!(snap.polling.checking);
    assert_eq!(snap.polling.next_poll_in_ms, None);
    run_until(&mut orch, |o| !o.state.poll_check_in_progress).await;
    let snap = orch.snapshot();
    assert_eq!(snap.polling.poll_interval_ms, 5000);
    assert!(snap.polling.next_poll_in_ms.unwrap() <= 5000);
    assert!(*generation.borrow() >= 2, "tick and cycle end notify");
}

#[tokio::test(start_paused = true)]
async fn poll_cycle_resets_the_next_refresh_countdown() {
    let mut h = Harness::new(
        json!({"polling": {"interval_ms": 50}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.state.poll_check_in_progress = true;
    orch.state.next_poll_due = None;
    orch.run_poll_cycle().await;
    let snap = orch.snapshot();
    assert!(!snap.polling.checking);
    assert!(snap.polling.next_poll_in_ms.unwrap() <= 50);
}

// ----- snapshots and token accounting --------------------------------------------------------------

async fn snapshot_after_events(events: Vec<CodexEvent>) -> (Snapshot, Snapshot) {
    let count = events.len();
    let mut h = Harness::new(json!({}), emitting_factory(events));
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    for _ in 0..count {
        assert!(orch.step().await);
    }
    let live = orch.snapshot();
    orch.terminate_running_issue("i-1", false, "test").await;
    let after = orch.snapshot();
    (live, after)
}

#[tokio::test(start_paused = true)]
async fn snapshot_reflects_last_codex_update_and_session_id() {
    let (snap, _) = snapshot_after_events(vec![
        session_started("thread-live-turn-live"),
        notification(json!({"method": "some-event"})),
    ])
    .await;
    let row = &snap.running[0];
    assert_eq!(
        row.issue_url.as_deref(),
        Some("https://example.org/issues/MT-1")
    );
    assert_eq!(row.session_id.as_deref(), Some("thread-live-turn-live"));
    assert_eq!(row.turn_count, 1);
    assert_eq!(row.last_codex_event, Some(CodexEventKind::Notification));
    let message = row.last_codex_message.as_ref().unwrap();
    assert_eq!(message.event, CodexEventKind::Notification);
    assert_eq!(message.message, Some(json!({"method": "some-event"})));
    assert_eq!(Some(message.timestamp), row.last_codex_timestamp);
}

#[tokio::test(start_paused = true)]
async fn snapshot_tracks_thread_totals_and_app_server_pid() {
    let (live, after) = snapshot_after_events(vec![
        session_started("s-1"),
        notification(json!({"method": "thread/tokenUsage/updated",
            "params": {"tokenUsage": {"total": {"inputTokens": 12, "outputTokens": 4, "totalTokens": 16}}}})),
    ])
    .await;
    let row = &live.running[0];
    assert_eq!(row.codex_app_server_pid.as_deref(), Some("4242"));
    assert_eq!(
        (
            row.codex_input_tokens,
            row.codex_output_tokens,
            row.codex_total_tokens
        ),
        (12, 4, 16)
    );
    assert_eq!(row.turn_count, 1);
    let totals = after.codex_totals;
    assert_eq!(
        (
            totals.input_tokens,
            totals.output_tokens,
            totals.total_tokens
        ),
        (12, 4, 16)
    );
    assert!(after.running.is_empty());
}

#[tokio::test(start_paused = true)]
async fn snapshot_tracks_turn_completed_usage_and_token_count_payloads() {
    let (live, _) = snapshot_after_events(vec![notification(json!({"method": "turn/completed",
        "usage": {"input_tokens": "12", "output_tokens": 4, "total_tokens": 16}}))])
    .await;
    let row = &live.running[0];
    assert_eq!(
        (
            row.codex_input_tokens,
            row.codex_output_tokens,
            row.codex_total_tokens
        ),
        (12, 4, 16)
    );

    let (live, after) = snapshot_after_events(vec![
        notification(json!({"method": "codex/event/token_count", "params": {"msg": {"type": "token_count",
            "info": {"total_token_usage": {"input_tokens": "2", "output_tokens": 2, "total_tokens": 4}}}}})),
        notification(json!({"method": "codex/event/token_count", "params": {"msg": {"type": "token_count",
            "info": {"total_token_usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}}}})),
    ])
    .await;
    let row = &live.running[0];
    assert_eq!(
        (
            row.codex_input_tokens,
            row.codex_output_tokens,
            row.codex_total_tokens
        ),
        (10, 5, 15)
    );
    assert_eq!(after.codex_totals.total_tokens, 15);
}

#[tokio::test(start_paused = true)]
async fn token_accounting_prefers_totals_accumulates_monotonically_and_ignores_last_usage() {
    let (live, _) = snapshot_after_events(vec![notification(json!({"method": "codex/event/token_count",
        "params": {"msg": {"type": "token_count", "info": {
            "last_token_usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3},
            "total_token_usage": {"input_tokens": 200, "output_tokens": 100, "total_tokens": 300}}}}}))])
    .await;
    assert_eq!(live.running[0].codex_total_tokens, 300);

    let (live, _) = snapshot_after_events(vec![
        notification(json!({"method": "thread/tokenUsage/updated",
            "params": {"tokenUsage": {"total": {"inputTokens": 8, "outputTokens": 3, "totalTokens": 11}}}})),
        notification(json!({"method": "thread/tokenUsage/updated",
            "params": {"tokenUsage": {"total": {"inputTokens": 10, "outputTokens": 4, "totalTokens": 14}}}})),
    ])
    .await;
    let row = &live.running[0];
    assert_eq!(
        (
            row.codex_input_tokens,
            row.codex_output_tokens,
            row.codex_total_tokens
        ),
        (10, 4, 14)
    );

    let (live, _) = snapshot_after_events(vec![notification(
        json!({"method": "codex/event/token_count",
        "params": {"msg": {"type": "token_count", "info": {
            "last_token_usage": {"input_tokens": 8, "output_tokens": 3, "total_tokens": 11}}}}}),
    )])
    .await;
    assert_eq!(live.running[0].codex_total_tokens, 0);
    assert_eq!(live.codex_totals.total_tokens, 0);
}

#[tokio::test(start_paused = true)]
async fn snapshot_tracks_rate_limit_payloads() {
    let rate_limits = json!({"limit_id": "codex", "primary": {"remaining": 90, "limit": 100},
        "secondary": null, "credits": {"has_credits": true, "balance": 9876.5}});
    let (live, _) = snapshot_after_events(vec![notification(
        json!({"method": "codex/event/token_count",
        "params": {"msg": {"payload": {"rate_limits": rate_limits.clone()}}}}),
    )])
    .await;
    assert_eq!(live.rate_limits, Some(rate_limits));
}

#[tokio::test(start_paused = true)]
async fn snapshot_includes_retry_backoff_entries_and_poll_countdown() {
    let mut h = Harness::new(json!({}), result_factory(|| Ok(())));
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.schedule_issue_retry(
        "mt-500",
        Some(2),
        RetryMeta {
            identifier: Some("MT-500".into()),
            issue_url: Some("https://example.org/issues/MT-500".into()),
            error: Some("boom".into()),
            ..RetryMeta::default()
        },
        DelayType::Failure,
    );
    orch.schedule_tick(ms(4_000));
    let snap = orch.snapshot();
    let row = &snap.retrying[0];
    assert_eq!(row.issue_id, "mt-500");
    assert_eq!(row.attempt, 2);
    assert!(row.due_in_ms > 0);
    assert_eq!(row.identifier, "MT-500");
    assert_eq!(
        row.issue_url.as_deref(),
        Some("https://example.org/issues/MT-500")
    );
    assert_eq!(row.error.as_deref(), Some("boom"));
    assert!(!snap.polling.checking);
    assert_eq!(snap.polling.poll_interval_ms, 30_000);
    assert!(snap.polling.next_poll_in_ms.unwrap() <= 4_000);
    assert_eq!(snap.tracker.kind.as_deref(), Some("memory"));
    assert_eq!(snap.max_concurrent_agents, 10);
    assert!(snap.issue("MT-500").is_some());
}

// ----- stalls and blocked issues -------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn restarts_stalled_workers_with_retry_backoff_and_keeps_the_host() {
    let started = Arc::new(Mutex::new(Vec::new()));
    let mut h = Harness::new(
        json!({"codex": {"stall_timeout_ms": 1000}, "worker": {"ssh_hosts": ["worker-a", "worker-b"]}}),
        blocking_factory(Arc::clone(&started)),
    );
    h.memory.set(vec![issue("i-1", "MT-STALL", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(
        issue("i-1", "MT-STALL", "In Progress"),
        None,
        Some("worker-b"),
    )
    .await;
    tokio::time::advance(ms(5_000)).await;
    orch.reconcile_stalled_running_issues().await;
    assert!(orch.state.running.is_empty());
    let entry = &orch.state.retry_attempts["i-1"];
    assert_eq!(entry.attempt, 1);
    assert_eq!(entry.identifier, "MT-STALL");
    assert_eq!(
        entry.issue_url.as_deref(),
        Some("https://example.org/issues/MT-STALL")
    );
    assert!(entry.error.as_deref().unwrap().starts_with("stalled for "));
    assert_eq!(entry.worker_host.as_deref(), Some("worker-b"));
    assert!((9_500..=10_500).contains(&retry_due_in(&orch, "i-1")));
}

#[tokio::test(start_paused = true)]
async fn blocks_stalled_workers_that_are_waiting_on_mcp_elicitation() {
    let mut h = Harness::new(
        json!({"codex": {"stall_timeout_ms": 1000}}),
        emitting_factory(vec![notification(
            json!({"method": "mcpServer/elicitation/request", "id": 3}),
        )]),
    );
    h.memory.set(vec![issue("i-1", "MT-MCP", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-MCP", "In Progress"), None, None)
        .await;
    assert!(orch.step().await); // the event
    {
        let entry = orch.state.running.get_mut("i-1").unwrap();
        entry.worker_host = Some("dm-dev2".into());
        entry.workspace_path = Some("/workspaces/MT-MCP".into());
    }
    tokio::time::advance(ms(5_000)).await;
    orch.maybe_dispatch().await;
    assert!(
        orch.state.running.is_empty(),
        "not re-dispatched in the same cycle"
    );
    assert!(orch.state.retry_attempts.is_empty());
    assert!(orch.state.claimed.contains("i-1"));
    let blocked = &orch.state.blocked["i-1"];
    assert_eq!(blocked.identifier, "MT-MCP");
    assert_eq!(
        blocked.error,
        "codex MCP elicitation requires operator input"
    );
    assert_eq!(blocked.worker_host.as_deref(), Some("dm-dev2"));
    assert_eq!(
        blocked.workspace_path.as_deref(),
        Some("/workspaces/MT-MCP")
    );
    let snap = orch.snapshot();
    assert_eq!(snap.blocked[0].identifier, "MT-MCP");
    assert_eq!(
        snap.blocked[0].issue_url.as_deref(),
        Some("https://example.org/issues/MT-MCP")
    );
    assert_eq!(
        snap.blocked[0].error,
        "codex MCP elicitation requires operator input"
    );
    assert_eq!(
        snap.issue("MT-MCP").unwrap().status,
        crate::snapshot::IssueStatus::Blocked
    );
}

#[tokio::test(start_paused = true)]
async fn blocks_failed_workers_after_app_server_reports_input_required() {
    let factory: Arc<dyn WorkerFactory> =
        Arc::new(FnWorkerFactory(|ctx: WorkerContext| async move {
            ctx.events.emit(input_required());
            Err(RunError::Codex(CodexError::TurnInputRequired(
                json!({"method": "item/tool/requestUserInput"}),
            )))
        }));
    let mut h = Harness::new(json!({}), factory);
    h.memory.set(vec![issue("i-1", "MT-IN", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-IN", "In Progress"), None, None)
        .await;
    run_until(&mut orch, |o| o.state.blocked.contains_key("i-1")).await;
    assert_eq!(
        orch.state.blocked["i-1"].error,
        "codex turn requires operator input"
    );
    assert!(orch.state.claimed.contains("i-1"));
    assert!(orch.state.retry_attempts.is_empty());
    assert_eq!(
        orch.state.blocked["i-1"].last_codex_event,
        Some(CodexEventKind::TurnInputRequired)
    );
}

#[tokio::test(start_paused = true)]
async fn blocks_on_the_typed_blocker_even_without_a_blocker_event() {
    let factory: Arc<dyn WorkerFactory> =
        Arc::new(FnWorkerFactory(|_ctx: WorkerContext| async move {
            Err(RunError::Codex(CodexError::ApprovalRequired(json!({}))))
        }));
    let mut h = Harness::new(json!({}), factory);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-AP", "In Progress"), None, None);
    run_until(&mut orch, |o| o.state.blocked.contains_key("i-1")).await;
    assert_eq!(
        orch.state.blocked["i-1"].error,
        "codex turn requires approval"
    );
}

#[tokio::test(start_paused = true)]
async fn blocks_normal_worker_exits_after_input_required() {
    let factory: Arc<dyn WorkerFactory> =
        Arc::new(FnWorkerFactory(|ctx: WorkerContext| async move {
            ctx.events.emit(input_required());
            Ok(())
        }));
    let mut h = Harness::new(json!({}), factory);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-IN", "In Progress"), None, None);
    run_until(&mut orch, |o| o.state.blocked.contains_key("i-1")).await;
    assert!(!orch.state.completed.contains("i-1"));
    assert!(orch.state.retry_attempts.is_empty());
    assert!(orch.state.claimed.contains("i-1"));
}

#[tokio::test(start_paused = true)]
async fn reconcile_releases_a_blocked_issue_when_a_required_label_is_removed() {
    let mut h = Harness::new(
        json!({"tracker": {"required_labels": ["symphony"]}}),
        result_factory(|| Ok(())),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    let entry = fake_entry("MT-1", issue("i-1", "MT-1", "In Progress"), None);
    orch.block_issue_from_entry("i-1", entry, "codex turn requires operator input".into());
    assert!(orch.state.claimed.contains("i-1"));
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]); // no labels
    orch.reconcile_blocked_issues().await;
    assert!(orch.state.blocked.is_empty());
    assert!(!orch.state.claimed.contains("i-1"));
}

#[tokio::test(start_paused = true)]
async fn blocked_issues_stay_blocked_while_active_and_are_released_when_missing() {
    let mut h = Harness::new(json!({}), result_factory(|| Ok(())));
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    let entry = fake_entry("MT-1", issue("i-1", "MT-1", "Todo"), None);
    orch.block_issue_from_entry("i-1", entry, "codex turn requires approval".into());
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    orch.maybe_dispatch().await;
    assert_eq!(
        orch.state.blocked["i-1"]
            .issue
            .as_ref()
            .unwrap()
            .state
            .as_deref(),
        Some("In Progress")
    );
    assert!(
        orch.state.running.is_empty(),
        "blocked issues are never auto-retried"
    );
    h.memory.set(vec![]);
    orch.reconcile_blocked_issues().await;
    assert!(orch.state.blocked.is_empty());
    assert!(!orch.state.claimed.contains("i-1"));
}

// ----- reconciliation of running issues ------------------------------------------------------------

async fn running_harness(config: Value) -> (Harness, Started) {
    let started = Arc::new(Mutex::new(Vec::new()));
    let h = Harness::new(config, blocking_factory(Arc::clone(&started)));
    (h, started)
}

#[tokio::test]
async fn non_active_issue_state_stops_running_agent_without_cleaning_workspace() {
    let (mut h, _) = running_harness(json!({})).await;
    let workspace = h.wf.workspace_root().join("MT-557");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-557", "Todo"), None, None);
    h.memory.set(vec![Issue {
        dispatchable: false,
        ..issue("i-1", "MT-557", "Backlog")
    }]);
    orch.reconcile_running_issues().await;
    assert!(orch.state.running.is_empty());
    assert!(!orch.state.claimed.contains("i-1"));
    assert!(workspace.exists());
    // The worker was cancelled and awaited.
    assert!(orch.workers.set.join_next().await.is_some());
}

#[tokio::test]
async fn terminal_issue_state_stops_running_agent_before_cleaning_workspace() {
    let wf_marker = Arc::new(Mutex::new(None::<std::path::PathBuf>));
    let marker = Arc::clone(&wf_marker);
    // The worker keeps an "alive" marker file until it is dropped.
    let factory: Arc<dyn WorkerFactory> = Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        let alive = marker.lock().unwrap().clone().unwrap();
        async move {
            struct Alive(std::path::PathBuf);
            impl Drop for Alive {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            std::fs::write(&alive, "alive").unwrap();
            let _guard = Alive(alive);
            ctx.cancel.cancelled().await;
            // Slow wind-down: the cleanup must still wait for it.
            tokio::time::sleep(Duration::from_millis(100)).await;
            Err(RunError::Cancelled)
        }
    }));
    let mut h = Harness::new(json!({}), factory);
    let alive = h.wf.path("alive");
    let result = h.wf.path("before_remove.result");
    *wf_marker.lock().unwrap() = Some(alive.clone());
    h.wf.rewrite(json!({"hooks": {"before_remove": format!(
        "if [ -e '{}' ]; then echo running > '{}'; else echo stopped > '{}'; fi",
        alive.display(), result.display(), result.display()
    )}}));
    let workspace = h.wf.workspace_root().join("MT-558");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-558", "In Progress"), None, None);
    tokio::task::yield_now().await;
    crate::test_support::eventually(Duration::from_secs(5), || alive.exists()).await;
    h.memory.set(vec![issue("i-1", "MT-558", "Closed")]);
    orch.reconcile_running_issues().await;
    assert_eq!(read(&result).trim(), "stopped");
    assert!(!workspace.exists());
    assert!(orch.state.running.is_empty());
    assert!(!orch.state.claimed.contains("i-1"));
}

#[tokio::test]
async fn terminal_cleanup_uses_the_workspace_recorded_for_the_running_issue() {
    let (mut h, _) = running_harness(json!({})).await;
    let old_root = h.wf.path("old-root");
    let old_workspace = old_root.join("MT-REC");
    std::fs::create_dir_all(&old_workspace).unwrap();
    let new_workspace = h.wf.workspace_root().join("MT-REC");
    std::fs::create_dir_all(&new_workspace).unwrap();
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-REC", "In Progress"), None, None);
    orch.state.running.get_mut("i-1").unwrap().workspace_path = Some(
        std::fs::canonicalize(&old_workspace)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    h.memory.set(vec![issue("i-1", "MT-REC", "Done")]);
    orch.reconcile_running_issues().await;
    assert!(!old_workspace.exists());
    assert!(new_workspace.exists());
}

#[tokio::test]
async fn missing_running_issues_stop_active_agents_without_cleaning_the_workspace() {
    let (mut h, _) = running_harness(json!({})).await;
    let workspace = h.wf.workspace_root().join("MT-GONE");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-GONE", "In Progress"), None, None);
    orch.maybe_dispatch().await; // memory tracker empty
    assert!(orch.state.running.is_empty());
    assert!(!orch.state.claimed.contains("i-1"));
    assert!(workspace.exists());
}

#[tokio::test]
async fn reconcile_updates_running_issue_state_for_active_issues() {
    let (mut h, _) = running_harness(json!({})).await;
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-1", "Todo"), None, None);
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    orch.reconcile_running_issues().await;
    assert!(orch.state.claimed.contains("i-1"));
    assert_eq!(
        orch.state.running["i-1"].issue.state.as_deref(),
        Some("In Progress")
    );
}

#[tokio::test]
async fn reconcile_stops_running_issue_when_reassigned_or_unlabeled() {
    for (config, refreshed) in [
        (
            json!({}),
            Issue {
                dispatchable: false,
                ..issue("i-1", "MT-1", "In Progress")
            },
        ),
        (
            json!({"tracker": {"required_labels": ["symphony"]}}),
            issue("i-1", "MT-1", "In Progress"),
        ),
    ] {
        let (mut h, _) = running_harness(config).await;
        let mut workers = WorkerSet::default();
        let mut orch = h.orchestrator(&mut workers);
        let labeled = Issue {
            labels: vec!["symphony".into()],
            ..issue("i-1", "MT-1", "In Progress")
        };
        orch.do_dispatch_issue(labeled, None, None);
        h.memory.set(vec![refreshed]);
        orch.reconcile_running_issues().await;
        assert!(orch.state.running.is_empty());
        assert!(!orch.state.claimed.contains("i-1"));
    }
}

#[tokio::test(start_paused = true)]
async fn stale_codex_events_never_reach_a_newer_run() {
    // The first worker ignores cancellation for a while and keeps emitting.
    let generation = Arc::new(AtomicUsize::new(0));
    let gen2 = Arc::clone(&generation);
    let factory: Arc<dyn WorkerFactory> = Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        let n = gen2.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                ctx.cancel.cancelled().await;
                ctx.events.emit(session_started("stale-session"));
            }
            ctx.cancel.cancelled().await;
            Err(RunError::Cancelled)
        }
    }));
    let mut h = Harness::new(json!({}), factory);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.do_dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None);
    orch.terminate_running_issue("i-1", false, "test").await;
    orch.do_dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None);
    for _ in 0..3 {
        tokio::select! {
            _ = orch.step() => {}
            () = tokio::time::sleep(ms(10)) => {}
        }
    }
    assert_eq!(orch.state.running["i-1"].session_id, None);
    assert_eq!(generation.load(Ordering::SeqCst), 2);
}

// ----- startup cleanup -----------------------------------------------------------------------------

#[tokio::test]
async fn startup_cleanup_removes_terminal_issue_workspaces() {
    let (mut h, _) = running_harness(json!({})).await;
    let done = h.wf.workspace_root().join("MT-DONE");
    let active = h.wf.workspace_root().join("MT-ACTIVE");
    std::fs::create_dir_all(&done).unwrap();
    std::fs::create_dir_all(&active).unwrap();
    h.memory.set(vec![
        issue("d", "MT-DONE", "Done"),
        issue("a", "MT-ACTIVE", "In Progress"),
    ]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.run_terminal_workspace_cleanup().await;
    assert!(!done.exists());
    assert!(active.exists());
}

#[tokio::test]
async fn startup_cleanup_fans_out_to_every_ssh_host() {
    let ssh_dir = tempfile::tempdir().unwrap();
    let exe = crate::test_support::executable_copy(
        &crate::test_support::fixture("ssh/fake_ssh.sh"),
        ssh_dir.path(),
        "ssh",
    );
    let mut h = Harness::build(
        json!({"workspace": {"root": "~/ws"}, "worker": {"ssh_hosts": ["worker-a", "worker-b"]}}),
        result_factory(|| Ok(())),
        |options| options.with_ssh(crate::ssh::SshConfig::default().with_executable(exe)),
    );
    h.memory.set(vec![
        issue("d1", "MT-D1", "Done"),
        issue("d2", "MT-D2", "Closed"),
    ]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.run_terminal_workspace_cleanup().await;
    let trace = read(&ssh_dir.path().join("ssh.trace"));
    for host in ["worker-a", "worker-b"] {
        for key in ["MT-D1", "MT-D2"] {
            assert!(
                trace
                    .lines()
                    .any(|l| l.contains(&format!("-T {host} bash -lc"))
                        && l.contains(&format!("~/ws/{key}"))),
                "missing removal of {key} on {host}:\n{trace}"
            );
        }
    }
}

// ----- store integration ---------------------------------------------------------------------------

#[tokio::test]
async fn runs_are_recorded_with_their_final_status() {
    let store = Store::open_in_memory().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = Arc::clone(&calls);
    let factory: Arc<dyn WorkerFactory> = Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        let n = calls2.fetch_add(1, Ordering::SeqCst);
        async move {
            ctx.reporter.runtime_info(None, "/tmp/ws/MT-1");
            ctx.events.emit(session_started("thread-1-turn-1"));
            ctx.events
                .emit(notification(json!({"method": "item/agentMessage/delta"})));
            ctx.events
                .emit(notification(json!({"method": "item/agentMessage/delta"})));
            ctx.events.emit(notification(json!({"method": "thread/tokenUsage/updated",
                "params": {"tokenUsage": {"total": {"inputTokens": 3, "outputTokens": 2, "totalTokens": 5}}}})));
            match n {
                0 => Ok(()),
                1 => Err(RunError::Other("boom".into())),
                2 => Err(RunError::Codex(CodexError::TurnInputRequired(json!({})))),
                _ => {
                    ctx.cancel.cancelled().await;
                    Err(RunError::Cancelled)
                }
            }
        }
    }));
    let mut h = Harness::with_store(json!({}), factory, store.clone());
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    for (id, ident) in [("a", "MT-A"), ("b", "MT-B"), ("c", "MT-C")] {
        orch.do_dispatch_issue(issue(id, ident, "In Progress"), None, None);
        run_until(&mut orch, |o| !o.state.running.contains_key(id)).await;
    }
    orch.do_dispatch_issue(issue("d", "MT-D", "In Progress"), None, None);
    orch.terminate_running_issue("d", false, "issue moved to non-active state Backlog")
        .await;
    // Let the recorder tasks flush.
    tokio::time::sleep(ms(100)).await;
    store.flush().await.unwrap();
    let page = store.list_runs(RunQuery::default()).await.unwrap();
    let status_of = |ident: &str| {
        page.runs
            .iter()
            .find(|r| r.issue_identifier == ident)
            .map(|r| {
                (
                    r.status,
                    r.error.clone(),
                    r.turns,
                    r.tokens.total,
                    r.workspace_path.clone(),
                )
            })
            .unwrap()
    };
    let a = status_of("MT-A");
    assert_eq!(a.0, RunStatus::Succeeded);
    assert_eq!((a.2, a.3), (1, 5));
    assert_eq!(a.4.as_deref(), Some("/tmp/ws/MT-1"));
    let b = status_of("MT-B");
    assert_eq!(b.0, RunStatus::Failed);
    assert_eq!(b.1.as_deref(), Some("agent exited: boom"));
    let c = status_of("MT-C");
    assert_eq!(c.0, RunStatus::Blocked);
    assert_eq!(c.1.as_deref(), Some("codex turn requires operator input"));
    let d = status_of("MT-D");
    assert_eq!(d.0, RunStatus::Cancelled);
    let run_a = page
        .runs
        .iter()
        .find(|r| r.issue_identifier == "MT-A")
        .unwrap();
    let events = store.list_events(run_a.id, None, None).await.unwrap();
    let deltas = events
        .iter()
        .filter(|e| e.message.as_deref() == Some("item/agentMessage/delta"))
        .count();
    assert_eq!(deltas, 1, "noisy deltas are throttled");
    assert!(events.iter().any(|e| e.kind == "session_started"));
}

// ----- shutdown ------------------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_cancels_and_awaits_every_worker() {
    let live = Arc::new(AtomicUsize::new(0));
    let live2 = Arc::clone(&live);
    let factory: Arc<dyn WorkerFactory> = Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        let live = Arc::clone(&live2);
        async move {
            live.fetch_add(1, Ordering::SeqCst);
            ctx.cancel.cancelled().await;
            live.fetch_sub(1, Ordering::SeqCst);
            Err(RunError::Cancelled)
        }
    }));
    let mut h = Harness::new(json!({}), factory);
    let mut workers = WorkerSet::default();
    let shutdown = CancellationToken::new();
    let rx = h.cmd_rx.take().unwrap();
    let mut orch = Orchestrator::new(Arc::clone(&h.ctx), rx, &mut workers, shutdown.clone());
    orch.do_dispatch_issue(issue("a", "MT-A", "In Progress"), None, None);
    orch.do_dispatch_issue(issue("b", "MT-B", "In Progress"), None, None);
    tokio::task::yield_now().await;
    crate::test_support::eventually(Duration::from_secs(2), || live.load(Ordering::SeqCst) == 2)
        .await;
    shutdown.cancel();
    orch.run().await;
    assert_eq!(live.load(Ordering::SeqCst), 0);
}
