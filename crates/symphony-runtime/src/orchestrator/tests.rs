//! Orchestrator state-machine tests (ports of `core_test.exs` / `orchestrator_status_test.exs`, B.13.2
//! and B.13.3, plus the Rust improvements). Workers are fakes from [`FnWorkerFactory`]; timing tests
//! run on tokio's paused clock, tests that run hook subprocesses use real time.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use symphony_codex::{CodexError, CodexEvent, CodexEventData, CodexEventKind, StreamMessage};
use symphony_core::Issue;
use symphony_store::{RunQuery, RunStatus, Store};
use symphony_trackers::{MemoryIssues, MemoryTracker, Provider, TrackerDeps, TransportError};
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
        let mut options = RuntimeOptions::new(Arc::clone(&wf.store), TrackerDeps::new().unwrap())
            .with_tracker(Arc::new(MemoryTracker::new(memory.clone())))
            .with_worker_factory(factory);
        // Default cancel grace, set before `customize` so a test can override it.
        options.worker_cancel_grace = Duration::from_millis(500);
        let options = customize(options);
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

/// Steps until `done` holds (bounded number of events). After every step the run index must
/// match the running entries, whatever the step added or removed.
async fn run_until(orch: &mut Orchestrator<'_>, done: impl Fn(&Orchestrator<'_>) -> bool) {
    for _ in 0..500 {
        if done(orch) {
            return;
        }
        assert!(orch.step().await, "orchestrator stopped");
        orch.state.running.assert_index_matches();
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

/// Worker that records its start, then ignores cancellation entirely — only an abort after the
/// cancel grace stops it. Lets a test make `stop_worker` take the full grace.
fn uncancellable_factory(started: Started) -> Arc<dyn WorkerFactory> {
    Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        started.lock().unwrap().push((
            ctx.issue.id.clone().unwrap_or_default(),
            ctx.attempt,
            ctx.worker_host.clone(),
        ));
        async move { std::future::pending::<WorkerResult>().await }
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
    // Unique like real run ids, and far above them (an orchestrator counts its runs up from 1).
    static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(1 << 40);
    let now = Instant::now();
    RunningEntry {
        run_id: NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed),
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
        pricing: Default::default(),
        cost_micro_usd: 0,
        task: None,
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

/// `agent.max_retry_attempts` failed retries in a row block the issue instead of a further retry;
/// it is released once it leaves the active states.
#[tokio::test(start_paused = true)]
async fn failed_retries_stop_at_the_retry_limit_and_block_the_issue() {
    let mut h = Harness::new(
        json!({"agent": {"max_retry_attempts": 2}}),
        result_factory(|| Err(RunError::Other("boom".into()))),
    );
    h.memory.set(vec![issue("i-1", "MT-559", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-559", "In Progress"), None, None)
        .await;

    // The first run and the two retries fail; a third retry would pass the limit.
    run_until(&mut orch, |o| o.state.blocked.contains_key("i-1")).await;
    let blocked = &orch.state.blocked["i-1"];
    assert_eq!(
        blocked.error,
        "retry limit reached: 2 retries failed (last error: agent exited: boom)"
    );
    assert_eq!(blocked.identifier, "MT-559");
    assert!(!orch.state.retry_attempts.contains_key("i-1"));
    assert!(
        orch.state.claimed.contains("i-1"),
        "a blocked issue stays claimed"
    );
    assert!(orch.state.running.is_empty());

    // Leaving the active states releases it.
    h.memory.set(vec![issue("i-1", "MT-559", "Backlog")]);
    orch.reconcile_blocked_issues().await;
    assert!(!orch.state.blocked.contains_key("i-1"));
    assert!(!orch.state.claimed.contains("i-1"));
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

    // Prompt-cache reads are part of the reported input and metered apart from it.
    let (live, _) = snapshot_after_events(vec![notification(
        json!({"method": "thread/tokenUsage/updated",
        "params": {"tokenUsage": {"total": {"inputTokens": 100, "cachedInputTokens": 70,
            "outputTokens": 5, "totalTokens": 105}}}}),
    )])
    .await;
    let row = &live.running[0];
    assert_eq!(
        (
            row.codex_input_tokens,
            row.codex_cached_input_tokens,
            row.codex_total_tokens
        ),
        (30, 70, 105)
    );
    assert_eq!(
        (
            live.codex_totals.input_tokens,
            live.codex_totals.cached_input_tokens
        ),
        (30, 70)
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
        orch.state
            .running
            .get("i-1")
            .unwrap()
            .issue
            .state
            .as_deref(),
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
    assert_eq!(orch.state.running.get("i-1").unwrap().session_id, None);
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

#[tokio::test(start_paused = true)]
async fn queued_retries_survive_a_restart_with_their_attempt_and_remaining_delay() {
    let store = Store::open_in_memory().unwrap();
    let failing = || result_factory(|| Err(RunError::Other("boom".into())));

    // First process: the run fails and its retry is queued for 10 s later.
    let mut h = Harness::with_store(json!({}), failing(), store.clone());
    h.memory.set(vec![issue("i-1", "MT-559", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-559", "In Progress"), None, None)
        .await;
    run_until(&mut orch, |o| o.state.retry_attempts.contains_key("i-1")).await;
    store.flush().await.unwrap();
    let saved = store.list_retries().await.unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!((saved[0].issue_id.as_str(), saved[0].attempt), ("i-1", 1));
    // The process dies here: nothing is released.
    drop(orch);
    drop(workers);

    // Second process on the same database.
    let mut h = Harness::with_store(json!({}), failing(), store.clone());
    h.memory.set(vec![issue("i-1", "MT-559", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.restore_retry_queue().await;
    assert!(orch.state.claimed.contains("i-1"));
    let entry = &orch.state.retry_attempts["i-1"];
    assert_eq!(entry.attempt, 1);
    assert_eq!(entry.identifier, "MT-559");
    assert_eq!(entry.error.as_deref(), Some("agent exited: boom"));
    assert!(retry_due_in(&orch, "i-1") <= 10_500);

    // The restored retry fires, fails again and is queued as attempt 2: the backoff continued.
    run_until(&mut orch, |o| {
        o.state
            .retry_attempts
            .get("i-1")
            .is_some_and(|entry| entry.attempt == 2)
    })
    .await;
    store.flush().await.unwrap();
    assert_eq!(store.list_retries().await.unwrap()[0].attempt, 2);

    // An issue that disappeared while Symphony was down is released when its retry fires.
    h.memory.set(vec![]);
    run_until(&mut orch, |o| !o.state.retry_attempts.contains_key("i-1")).await;
    assert!(!orch.state.claimed.contains("i-1"));
    store.flush().await.unwrap();
    assert!(store.list_retries().await.unwrap().is_empty());
}

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

// ----- spend controls (budgets, kill switch) --------------------------------------------------------

fn usage_event(input: u64, output: u64) -> CodexEvent {
    notification(json!({"method": "thread/tokenUsage/updated",
        "params": {"tokenUsage": {"total": {
            "inputTokens": input, "outputTokens": output, "totalTokens": input + output}}}}))
}

#[tokio::test(start_paused = true)]
async fn an_issue_over_its_budget_is_stopped_and_blocked_with_cost_recorded() {
    let store = Store::open_in_memory().unwrap();
    let mut h = Harness::with_store(
        json!({"budget": {"max_tokens_per_issue": 100, "input_usd_per_million_tokens": 2,
            "output_usd_per_million_tokens": 10}}),
        emitting_factory(vec![usage_event(60, 10), usage_event(100, 20)]),
        store.clone(),
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-1"));
    run_until(&mut orch, |o| o.state.blocked.contains_key("i-1")).await;
    assert!(orch.state.running.is_empty());
    assert_eq!(
        orch.state.blocked["i-1"].error,
        "issue token budget exhausted: 120 of 100 tokens used"
    );
    let snapshot = orch.snapshot();
    // 100 input at $2/M + 20 output at $10/M = 400 micro-USD.
    assert_eq!(snapshot.codex_totals.cost_micro_usd, 400);
    assert_eq!(snapshot.spend.day_tokens, 120);
    assert_eq!(snapshot.spend.day_cost_micro_usd, 400);
    assert_eq!(snapshot.spend.max_tokens_per_issue, Some(100));

    // The block holds: the next poll does not dispatch the issue again.
    orch.maybe_dispatch().await;
    assert!(orch.state.running.is_empty());
    let mut finished = None;
    for _ in 0..200 {
        let runs = store.list_runs(RunQuery::default()).await.unwrap().runs;
        if runs.first().is_some_and(|r| r.status != RunStatus::Running) {
            finished = runs.first().cloned();
            break;
        }
        tokio::time::sleep(ms(10)).await;
    }
    let run = finished.expect("run finished");
    assert_eq!(run.status, RunStatus::Blocked);
    assert_eq!(run.cost_micro_usd, 400);
}

/// Fix 2: on a shared store, `renew_leases` runs `enforce_spend` with no timeout around it, so a
/// run it stops is released all the way — claim and dispatch lease dropped, not just removed from
/// `running`. A timeout firing after `state.running.remove` but before `drop_lease`/`claimed.remove`
/// would strand the task `running` with its lease renewed forever; this drives the exact path.
///
/// Guard: the worker ignores cancellation and the cancel grace (9s) exceeds `LEASE_RENEW_TIMEOUT`
/// (7.5s), so `stop_running_issue` sits in `stop_worker` past that bound. If a timeout were
/// restored around `enforce_spend`, it would fire mid-stop and the final assertions would fail.
#[tokio::test(start_paused = true)]
async fn shared_store_renew_cycle_releases_a_spend_stopped_run_completely() {
    let store = Store::open_in_memory_shared().unwrap();
    assert!(
        store.is_shared(),
        "the renew/reconcile block only runs on a shared store"
    );
    let started: Started = Arc::default();
    let store_for_opts = store.clone();
    let mut h = Harness::build(
        json!({}),
        uncancellable_factory(Arc::clone(&started)),
        |options| {
            let mut options = options.with_store(store_for_opts);
            // Longer than LEASE_RENEW_TIMEOUT (7.5s): a restored `enforce_spend` timeout would fire
            // before this worker's forced abort, stranding the run.
            options.worker_cancel_grace = LEASE_RENEW_TIMEOUT + Duration::from_secs(2);
            options
        },
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-1"));
    // A held dispatch lease, as a shared-store run always has.
    assert!(orch.take_lease("i-1").await);
    assert!(orch.state.leases.contains_key("i-1"));

    // Engage the kill switch and persist it, so `sync_kill_switch` (which runs first in the block)
    // sees the store and memory agree and makes no change, leaving `enforce_spend` to stop the run.
    let (reply, ack) = tokio::sync::oneshot::channel();
    orch.handle_command(RuntimeCommand::SetPaused {
        paused: true,
        reason: Some("runaway spend".into()),
        reply,
    })
    .await;
    ack.await.unwrap();
    for _ in 0..100 {
        if store.control(PAUSED_CONTROL_KEY).await.unwrap().is_some() {
            break;
        }
        tokio::time::sleep(ms(10)).await;
    }

    orch.renew_leases().await;

    assert!(
        orch.state.running.is_empty(),
        "the stopped run left `running`"
    );
    assert!(
        !orch.state.claimed.contains("i-1"),
        "its claim was released"
    );
    assert!(
        !orch.state.leases.contains_key("i-1"),
        "its dispatch lease was dropped (not left to renew forever)"
    );
}

#[tokio::test(start_paused = true)]
async fn stored_history_counts_toward_the_issue_budget_after_a_restart() {
    let store = Store::open_in_memory().unwrap();
    let previous = store
        .start_run(symphony_store::NewRun::new("i-1", "MT-1"))
        .await
        .unwrap();
    store
        .update_usage(
            previous,
            symphony_store::TokenUsage {
                input: 80,
                cached: 0,
                output: 30,
                total: 110,
            },
            0,
        )
        .await
        .unwrap();
    let started: Started = Arc::default();
    let mut h = Harness::with_store(
        json!({"budget": {"max_tokens_per_issue": 100}}),
        blocking_factory(Arc::clone(&started)),
        store,
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(started.lock().unwrap().is_empty(), "never started");
    assert_eq!(
        orch.state.blocked["i-1"].error,
        "issue token budget exhausted: 110 of 100 tokens used"
    );
    // Other issues are unaffected.
    h.memory.set(vec![issue("i-2", "MT-2", "In Progress")]);
    orch.dispatch_issue(issue("i-2", "MT-2", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-2"));
    orch.terminate_running_issue("i-2", false, "test").await;
}

#[tokio::test(start_paused = true)]
async fn an_exhausted_day_stops_running_agents_and_holds_dispatch() {
    let mut h = Harness::new(
        json!({"budget": {"max_tokens_per_day": 50}}),
        emitting_factory(vec![usage_event(40, 20)]),
    );
    h.memory.set(vec![
        issue("i-1", "MT-1", "In Progress"),
        issue("i-2", "MT-2", "In Progress"),
    ]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    // Stopped, not blocked: the claim is released so the issue runs again tomorrow.
    assert!(orch.state.blocked.is_empty());
    assert!(!orch.state.claimed.contains("i-1"));
    let snapshot = orch.snapshot();
    assert_eq!(
        snapshot.spend.stopped_reason.as_deref(),
        Some("daily token budget exhausted: 60 of 50 tokens used today (UTC)")
    );
    assert!(!snapshot.spend.paused);
    orch.maybe_dispatch().await;
    assert!(orch.state.running.is_empty(), "dispatch is held");
}

#[tokio::test(start_paused = true)]
async fn the_kill_switch_stops_everything_survives_a_restart_and_resumes() {
    let store = Store::open_in_memory().unwrap();
    let started: Started = Arc::default();
    let mut h = Harness::with_store(
        json!({}),
        blocking_factory(Arc::clone(&started)),
        store.clone(),
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-1"));

    let (reply, ack) = tokio::sync::oneshot::channel();
    orch.handle_command(RuntimeCommand::SetPaused {
        paused: true,
        reason: Some("runaway spend".into()),
        reply,
    })
    .await;
    assert_eq!(
        ack.await.unwrap(),
        PauseAck {
            paused: true,
            reason: Some("runaway spend".into())
        }
    );
    orch.enforce_spend().await;
    assert!(orch.state.running.is_empty());
    assert!(!orch.state.claimed.contains("i-1"));
    orch.maybe_dispatch().await;
    assert!(orch.state.running.is_empty());
    let snapshot = orch.snapshot();
    assert!(snapshot.spend.paused);
    assert_eq!(
        snapshot.spend.stopped_reason.as_deref(),
        Some("dispatch paused: runaway spend")
    );
    let mut persisted = None;
    for _ in 0..100 {
        persisted = store.control(PAUSED_CONTROL_KEY).await.unwrap();
        if persisted.is_some() {
            break;
        }
        tokio::time::sleep(ms(10)).await;
    }
    assert_eq!(persisted.as_deref(), Some("runaway spend"));
    drop(orch);

    // A restarted orchestrator comes back paused.
    let mut h = Harness::with_store(
        json!({}),
        blocking_factory(Arc::clone(&started)),
        store.clone(),
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.restore_spend().await;
    assert_eq!(orch.state.spend.paused.as_deref(), Some("runaway spend"));
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.is_empty());

    let ack = orch.set_paused(false, None);
    assert!(!ack.paused);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-1"));
    // The write is retried in the background until the store takes it.
    let mut released = false;
    for _ in 0..100 {
        if store.control(PAUSED_CONTROL_KEY).await.unwrap().is_none() {
            released = true;
            break;
        }
        tokio::time::sleep(ms(10)).await;
    }
    assert!(released, "the release was persisted");
    assert_eq!(started.lock().unwrap().len(), 2);
    orch.terminate_running_issue("i-1", false, "test").await;
}

#[tokio::test(start_paused = true)]
async fn released_claims_drop_the_completed_bookkeeping() {
    let mut h = Harness::new(json!({}), result_factory(|| Ok(())));
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    run_until(&mut orch, |o| o.state.retry_attempts.contains_key("i-1")).await;
    assert!(orch.state.completed.contains("i-1"));
    orch.release_issue_claim("i-1");
    assert!(!orch.state.completed.contains("i-1"));
}

// ----- tasks (task API) ------------------------------------------------------------------------

async fn task_store() -> (Store, i64) {
    let store = Store::open_in_memory().unwrap();
    let task = store
        .create_task(symphony_store::NewTask {
            prompt: "Write a haiku about Rust".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    (store, task.id)
}

async fn task_status(
    store: &Store,
    id: i64,
    want: symphony_store::TaskStatus,
) -> symphony_store::TaskRecord {
    for _ in 0..200 {
        let task = store.get_task(id).await.unwrap().unwrap();
        if task.status == want {
            return task;
        }
        tokio::time::sleep(ms(5)).await;
    }
    panic!("task {id} never reached {want}");
}

/// `(task, backend)` of every started worker.
type SeenTasks = Arc<Mutex<Vec<(Option<crate::worker::TaskSpec>, Option<String>)>>>;

/// Worker that records its context's task and backend, reports a result and ends with `result`.
fn task_factory(seen: SeenTasks, result: fn() -> WorkerResult) -> Arc<dyn WorkerFactory> {
    Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        seen.lock()
            .unwrap()
            .push((ctx.task.clone(), ctx.backend.clone()));
        async move {
            ctx.reporter.task_result("five seven five");
            result()
        }
    }))
}

#[tokio::test(start_paused = true)]
async fn queued_tasks_run_once_and_record_their_result() {
    use symphony_store::TaskStatus;
    let (store, id) = task_store().await;
    let seen: SeenTasks = Arc::default();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}}),
        task_factory(Arc::clone(&seen), || Ok(())),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    let issue_id = format!("task:{id}");
    assert!(orch.state.running.contains_key(&issue_id));
    assert_eq!(
        orch.state.running.get(&issue_id).unwrap().identifier,
        format!("task-{id}")
    );
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    let task = task_status(&store, id, TaskStatus::Succeeded).await;
    assert_eq!(task.result.as_deref(), Some("five seven five"));
    assert_eq!(task.attempts, 1);
    assert!(
        orch.state.retry_attempts.is_empty(),
        "tasks are never retried"
    );
    assert!(!orch.state.claimed.contains(&issue_id));
    let (spec, backend) = seen.lock().unwrap()[0].clone();
    let spec = spec.unwrap();
    assert_eq!(spec.prompt, "Write a haiku about Rust");
    assert_eq!(backend, None);
    // Nothing is left to dispatch.
    orch.maybe_dispatch().await;
    assert!(orch.state.running.is_empty());
    let runs = store.list_runs(RunQuery::default()).await.unwrap().runs;
    assert_eq!(runs[0].issue_identifier, format!("task-{id}"));
}

/// A dependant waits for its dependency and is dispatched as soon as it succeeded (not at the next
/// poll); a failed dependency fails it instead.
#[tokio::test(start_paused = true)]
async fn dependants_run_once_their_dependency_succeeded() {
    use symphony_store::TaskStatus;
    let (store, first) = task_store().await;
    let after = |prompt: &str, on: i64| symphony_store::NewTask {
        prompt: prompt.into(),
        depends_on: vec![on],
        ..Default::default()
    };
    let second = store.create_task(after("second", first)).await.unwrap().id;
    let seen: SeenTasks = Arc::default();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}, "polling": {"interval_ms": 600_000}}),
        task_factory(Arc::clone(&seen), || Ok(())),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    assert_eq!(orch.state.running.len(), 1, "only the dependency runs");
    assert!(orch.state.running.contains_key(&format!("task:{first}")));
    let started = tokio::time::Instant::now();
    run_until(&mut orch, |o| {
        o.state.running.contains_key(&format!("task:{second}"))
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "dispatched after {:?}, not at the next poll",
        started.elapsed()
    );
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    task_status(&store, second, TaskStatus::Succeeded).await;
    let prompts: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(spec, _)| spec.as_ref().unwrap().prompt.clone())
        .collect();
    assert_eq!(prompts, ["Write a haiku about Rust", "second"]);
}

#[tokio::test(start_paused = true)]
async fn a_failed_dependency_fails_the_waiting_task() {
    use symphony_store::TaskStatus;
    let (store, first) = task_store().await;
    let second = store
        .create_task(symphony_store::NewTask {
            prompt: "second".into(),
            depends_on: vec![first],
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}}),
        task_factory(Arc::default(), || Err(RunError::Other("boom".into()))),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    task_status(&store, first, TaskStatus::Failed).await;
    let failed = task_status(&store, second, TaskStatus::Failed).await;
    assert_eq!(
        failed.error,
        Some(format!("dependency task-{first} did not succeed"))
    );
    assert_eq!(failed.attempts, 0, "it never ran");
}

/// The reason in run history, a retry entry and a task record is the whole chain, not the tag that
/// `Display` of a wrapping error stops at.
#[tokio::test(start_paused = true)]
async fn a_failed_run_records_its_root_cause() {
    use symphony_store::TaskStatus;
    fn refresh_failed() -> WorkerResult {
        Err(RunError::IssueStateRefresh(FetchError::Tracker(
            TrackerError::request(Provider::Linear, TransportError::Timeout),
        )))
    }
    let reason = "agent exited: issue_state_refresh_failed: linear_api_request: timeout";

    let mut h = Harness::new(json!({}), result_factory(refresh_failed));
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    run_until(&mut orch, |o| o.state.retry_attempts.contains_key("i-1")).await;
    assert_eq!(
        orch.state.retry_attempts["i-1"].error.as_deref(),
        Some(reason)
    );

    let (store, id) = task_store().await;
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}}),
        task_factory(Arc::default(), refresh_failed),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    let failed = task_status(&store, id, TaskStatus::Failed).await;
    assert_eq!(failed.error.as_deref(), Some(reason));
}

/// A failed run of a kind the task's policy retries runs again (here at once), and the task
/// succeeds on the next attempt.
#[tokio::test(start_paused = true)]
async fn a_failed_task_is_retried_by_its_policy() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use symphony_store::{TaskFailure, TaskRetry, TaskStatus};
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    fn first_fails() -> WorkerResult {
        if RUNS.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(RunError::Other("flaky".into()))
        } else {
            Ok(())
        }
    }
    let store = Store::open_in_memory().unwrap();
    let id = store
        .create_task(symphony_store::NewTask {
            prompt: "flaky".into(),
            retry: TaskRetry {
                max_attempts: 2,
                backoff_ms: 0,
                max_backoff_ms: 0,
                on: vec![TaskFailure::Error],
            },
            ..Default::default()
        })
        .await
        .unwrap()
        .id;
    let seen: SeenTasks = Arc::default();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}, "polling": {"interval_ms": 600_000}}),
        task_factory(Arc::clone(&seen), first_fails),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    run_until(&mut orch, |_| seen.lock().unwrap().len() == 2).await;
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    let task = task_status(&store, id, TaskStatus::Succeeded).await;
    assert_eq!((task.attempts, task.failed_attempts), (2, 1));
}

#[tokio::test(start_paused = true)]
async fn failed_tasks_and_unknown_backends_end_failed() {
    use symphony_store::TaskStatus;
    let (store, id) = task_store().await;
    let other = store
        .create_task(symphony_store::NewTask {
            prompt: "x".into(),
            backend: Some("nope".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}}),
        task_factory(Arc::default(), || Err(RunError::Other("boom".into()))),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    let refused = task_status(&store, other.id, TaskStatus::Failed).await;
    assert_eq!(refused.error.as_deref(), Some("unknown backend \"nope\""));
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    let failed = task_status(&store, id, TaskStatus::Failed).await;
    assert_eq!(failed.error.as_deref(), Some("agent exited: boom"));
}

#[tokio::test(start_paused = true)]
async fn blocked_tasks_wait_for_unblock_and_then_run_again() {
    use symphony_store::TaskStatus;
    let (store, id) = task_store().await;
    // The agent asks for input and its session ends with the blocker, like a Codex turn does.
    let factory: Arc<dyn WorkerFactory> =
        Arc::new(FnWorkerFactory(|ctx: WorkerContext| async move {
            ctx.events.emit(input_required());
            Err(RunError::Codex(CodexError::TurnInputRequired(json!({}))))
        }));
    let mut h = Harness::with_store(json!({"tracker": {"kind": "none"}}), factory, store.clone());
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    let issue_id = format!("task:{id}");
    run_until(&mut orch, |o| o.state.blocked.contains_key(&issue_id)).await;
    let blocked = task_status(&store, id, TaskStatus::Blocked).await;
    assert!(blocked.error.is_some());
    // Tracker reconciliation leaves blocked tasks alone.
    orch.reconcile_blocked_issues().await;
    assert!(orch.state.blocked.contains_key(&issue_id));

    store
        .requeue_task(id, Some("Use 5-7-5.".into()))
        .await
        .unwrap();
    let (reply, ack) = tokio::sync::oneshot::channel();
    orch.handle_command(RuntimeCommand::UnblockTask { id, reply })
        .await;
    ack.await.unwrap();
    assert!(!orch.state.blocked.contains_key(&issue_id));
    orch.maybe_dispatch().await;
    assert!(orch.state.running.contains_key(&issue_id));
    let task = store.get_task(id).await.unwrap().unwrap();
    assert_eq!(task.attempts, 2);
    assert!(task.prompt.ends_with("Operator reply:\nUse 5-7-5."));
    run_until(&mut orch, |o| o.state.blocked.contains_key(&issue_id)).await;
}

#[tokio::test(start_paused = true)]
async fn running_tasks_can_be_cancelled_and_survive_tracker_reconciliation() {
    use symphony_store::TaskStatus;
    let (store, id) = task_store().await;
    let started: Started = Arc::default();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "memory"}}),
        blocking_factory(Arc::clone(&started)),
        store.clone(),
    );
    h.memory.set(vec![]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    let issue_id = format!("task:{id}");
    assert!(orch.state.running.contains_key(&issue_id));
    // The tracker has never heard of task:N; reconciliation must not stop the run.
    orch.reconcile_running_issues().await;
    assert!(orch.state.running.contains_key(&issue_id));

    let (reply, ack) = tokio::sync::oneshot::channel();
    orch.handle_command(RuntimeCommand::CancelTask { id, reply })
        .await;
    assert!(ack.await.unwrap(), "it was running");
    assert!(orch.state.running.is_empty());
    let task = task_status(&store, id, TaskStatus::Cancelled).await;
    assert_eq!(task.error.as_deref(), Some("cancelled by client"));
}

#[tokio::test(start_paused = true)]
async fn the_kill_switch_puts_running_tasks_back_in_the_queue_and_restarts_requeue_them() {
    use symphony_store::TaskStatus;
    let (store, id) = task_store().await;
    let started: Started = Arc::default();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}}),
        blocking_factory(Arc::clone(&started)),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    let _ = orch.set_paused(true, Some("maintenance".into()));
    orch.enforce_spend().await;
    assert!(orch.state.running.is_empty());
    let task = task_status(&store, id, TaskStatus::Queued).await;
    assert!(task.error.unwrap().contains("dispatch paused"));
    orch.maybe_dispatch().await;
    assert!(orch.state.running.is_empty(), "held while paused");
    let _ = orch.set_paused(false, None);
    orch.maybe_dispatch().await;
    assert!(orch.state.running.contains_key(&format!("task:{id}")));
    // A process that dies mid-run leaves the task running under its lease: nothing recovers it
    // while the lease lives. A restart drops the old process's leases and queues it again.
    drop(orch);
    assert_eq!(store.requeue_interrupted_tasks().await.unwrap(), 0);
    store.release_owner_leases("local").await.unwrap();
    assert_eq!(store.requeue_interrupted_tasks().await.unwrap(), 1);
    task_status(&store, id, TaskStatus::Queued).await;
}

// ----- dispatch leases (replicas sharing a store) -----------------------------------------

/// A second orchestrator ("replica") on the same store.
fn replica(store: &Store, id: &str, factory: Arc<dyn WorkerFactory>, config: Value) -> Harness {
    let id = id.to_owned();
    let store = store.clone();
    Harness::build(config, factory, move |options| {
        options.with_store(store).with_replica_id(id)
    })
}

#[tokio::test(start_paused = true)]
async fn replicas_never_run_the_same_issue_or_task_at_once() {
    let store = Store::open_in_memory().unwrap();
    let started_a: Started = Arc::default();
    let started_b: Started = Arc::default();
    let mut a = replica(
        &store,
        "a",
        blocking_factory(Arc::clone(&started_a)),
        json!({}),
    );
    let mut b = replica(
        &store,
        "b",
        blocking_factory(Arc::clone(&started_b)),
        json!({}),
    );
    for h in [&a, &b] {
        h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    }
    let mut workers_a = WorkerSet::default();
    let mut workers_b = WorkerSet::default();
    let mut orch_a = a.orchestrator(&mut workers_a);
    let mut orch_b = b.orchestrator(&mut workers_b);

    orch_a
        .dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    orch_b
        .dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch_a.state.running.contains_key("i-1"));
    assert!(orch_b.state.running.is_empty(), "b sees a's lease");
    assert!(!orch_b.state.claimed.contains("i-1"));
    assert_eq!(
        store.lease_holder("i-1").await.unwrap().as_deref(),
        Some("a")
    );

    // A task queued once is started by one replica only.
    let task = store
        .create_task(symphony_store::NewTask {
            prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    orch_b.maybe_dispatch().await;
    orch_a.maybe_dispatch().await;
    let key = format!("task:{}", task.id);
    assert!(orch_b.state.running.contains_key(&key));
    assert!(!orch_a.state.running.contains_key(&key));
    assert_eq!(
        store.lease_holder(&key).await.unwrap().as_deref(),
        Some("b")
    );

    // Released with the claim: the other replica may take the issue now.
    orch_a.terminate_running_issue("i-1", false, "test").await;
    assert!(!orch_a.state.leases.contains_key("i-1"));
    let mut released = false;
    for _ in 0..100 {
        if store.lease_holder("i-1").await.unwrap().is_none() {
            released = true;
            break;
        }
        tokio::time::sleep(ms(5)).await;
    }
    assert!(released);
    orch_b
        .dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch_b.state.running.contains_key("i-1"));
    assert_eq!(started_a.lock().unwrap().len(), 1);
    assert_eq!(started_b.lock().unwrap().len(), 2);

    // Renewal keeps held leases alive and reports none lost.
    orch_b.renew_leases().await;
    assert_eq!(
        store.lease_holder("i-1").await.unwrap().as_deref(),
        Some("b")
    );
    orch_b.terminate_running_issue("i-1", false, "test").await;
    orch_b.terminate_running_issue(&key, false, "test").await;
}

// Real time: under a paused clock the renewal's timeout would fire while the store thread is
// still answering (the clock jumps whenever the runtime idles).
#[tokio::test]
async fn a_replica_that_lost_its_leases_stops_the_runs_and_leaves_the_task_to_the_new_owner() {
    use symphony_store::TaskStatus;
    let store = Store::open_in_memory().unwrap();
    let started: Started = Arc::default();
    let mut a = replica(
        &store,
        "a",
        blocking_factory(Arc::clone(&started)),
        json!({}),
    );
    a.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = a.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    let task = store
        .create_task(symphony_store::NewTask {
            prompt: "x".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    orch.maybe_dispatch().await;
    let key = format!("task:{}", task.id);
    assert!(orch.state.running.contains_key("i-1"));
    assert!(orch.state.running.contains_key(&key));

    // `a` was cut off long enough for both leases to expire, and `b` took them.
    for lease in ["i-1", key.as_str()] {
        store.release_lease(lease, "a").await.unwrap();
        assert!(
            store
                .acquire_lease(lease, "b", std::time::Duration::from_secs(60))
                .await
                .unwrap()
        );
    }
    orch.renew_leases().await;
    assert!(orch.state.running.is_empty(), "a stopped both runs");
    assert!(orch.state.leases.is_empty());
    tokio::time::sleep(ms(50)).await;
    for lease in ["i-1", key.as_str()] {
        assert_eq!(
            store.lease_holder(lease).await.unwrap().as_deref(),
            Some("b"),
            "b's lease is left alone"
        );
    }
    // The task is b's now: a did not put it back to `queued` (b runs it).
    assert_eq!(
        store.get_task(task.id).await.unwrap().unwrap().status,
        TaskStatus::Running
    );
}

#[tokio::test(start_paused = true)]
async fn a_renewal_stuck_on_the_database_is_not_waited_for() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("symphony.db");
    let store = Store::open(&path).unwrap();
    let started: Started = Arc::default();
    let mut h = replica(
        &store,
        "a",
        blocking_factory(Arc::clone(&started)),
        json!({}),
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-1"));
    // Another connection holds the write lock: the renewal waits on it (an unresponsive
    // database), and the orchestrator must move on after LEASE_RENEW_TIMEOUT.
    let lock = rusqlite::Connection::open(&path).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let deadline = orch.state.leases["i-1"];
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    let real = std::time::Instant::now();
    orch.renew_leases().await;
    assert_eq!(
        orch.state.leases["i-1"], deadline,
        "the renewal did not succeed (the lock held)"
    );
    assert!(
        real.elapsed() < symphony_store::BUSY_TIMEOUT / 2,
        "renew_leases waited for the stuck store ({:?})",
        real.elapsed()
    );
    assert!(
        orch.state.running.contains_key("i-1"),
        "one failed renewal is tolerated"
    );
    drop(lock);
    orch.terminate_running_issue("i-1", false, "test").await;
}

#[tokio::test]
async fn a_lease_is_stopped_before_a_second_missed_renewal_could_let_it_expire() {
    use std::collections::HashMap;
    let renewed = tokio::time::Instant::now();
    let leases = HashMap::from([("k".to_owned(), renewed + LEASE_TTL)]);
    // One failed renewal is tolerated; at the second the lease would expire before the third.
    assert!(expiring_leases(&leases, renewed + LEASE_RENEW_EVERY).is_empty());
    assert_eq!(
        expiring_leases(&leases, renewed + 2 * LEASE_RENEW_EVERY),
        vec!["k".to_owned()]
    );
    // Stopped while the lease is still ours, with at least a renewal period to spare.
    assert!(2 * LEASE_RENEW_EVERY + LEASE_RENEW_EVERY <= LEASE_TTL);
}

fn watched(deadline: tokio::time::Instant, cancel: &CancellationToken) -> LeaseWatch {
    LeaseWatch {
        deadline,
        cancel: cancel.clone(),
        fenced: false,
    }
}

/// Fix 1: the watchdog is a strict backstop. It cancels a run only once its lease is within the
/// backstop margin — which is shorter than `LEASE_STOP_WINDOW`, so a run merely inside the actor's
/// own (renewal-cadence) fence window, i.e. one tolerated missed renewal, is left alone. On firing
/// it also sets `fenced` so the actor abandons rather than fails the run.
#[tokio::test(start_paused = true)]
async fn fire_expiring_lease_cancels_only_past_the_backstop() {
    let backstop = lease_watchdog_backstop(Duration::from_secs(10));
    assert!(
        backstop < LEASE_STOP_WINDOW,
        "the backstop must fire after the actor's own fence"
    );
    let now = tokio::time::Instant::now();
    let map: LeaseWatchMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let fresh = CancellationToken::new();
    let actor_window = CancellationToken::new();
    let past_backstop = CancellationToken::new();
    {
        let mut guard = map.lock().unwrap();
        // Just renewed: a full TTL out.
        guard.insert("fresh".to_owned(), watched(now + LEASE_TTL, &fresh));
        // Inside the actor's LEASE_STOP_WINDOW but not the backstop: one missed renewal, tolerated.
        guard.insert(
            "actor_window".to_owned(),
            watched(
                now + LEASE_STOP_WINDOW - Duration::from_secs(1),
                &actor_window,
            ),
        );
        // Within the backstop: the actor has been parked far past its own fence.
        guard.insert(
            "past_backstop".to_owned(),
            watched(now + backstop - Duration::from_secs(1), &past_backstop),
        );
    }
    fire_expiring_lease_cancels(&map, backstop);
    assert!(
        !fresh.is_cancelled(),
        "a freshly-renewed lease keeps running"
    );
    assert!(
        !actor_window.is_cancelled(),
        "inside the actor's fence window but not the backstop: left for the actor to renew"
    );
    assert!(
        past_backstop.is_cancelled(),
        "within the backstop: cancelled"
    );
    assert!(
        map.lock().unwrap()["past_backstop"].fenced,
        "firing marks the run fenced so the actor abandons it"
    );
}

/// Fix 1: the watchdog task cancels an expiring run on its own schedule, with no actor loop
/// running at all — proving the fence survives a parked actor that can no longer renew or process
/// the `LeaseRenew` timer. The cancel happens before the lease deadline.
#[tokio::test(start_paused = true)]
async fn the_lease_watchdog_cancels_an_unrenewed_run_without_the_actor_loop() {
    let backstop = lease_watchdog_backstop(Duration::from_secs(10));
    let start = tokio::time::Instant::now();
    let deadline = start + LEASE_TTL;
    let map: LeaseWatchMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
    let shutdown = CancellationToken::new();
    let token = CancellationToken::new();
    map.lock()
        .unwrap()
        .insert("i-1".to_owned(), watched(deadline, &token));
    let watchdog = tokio::spawn(run_lease_watchdog(
        Arc::clone(&map),
        shutdown.clone(),
        backstop,
    ));

    // Outside the backstop (still within the actor's tolerated window): left alone.
    tokio::time::advance(LEASE_TTL - backstop - LEASE_RENEW_EVERY).await;
    tokio::task::yield_now().await;
    assert!(!token.is_cancelled(), "not yet within the backstop");

    // Nothing renews the lease (the actor loop is parked); the watchdog cancels once inside it.
    for _ in 0..LEASE_TTL.as_secs() {
        if token.is_cancelled() {
            break;
        }
        tokio::time::advance(LEASE_WATCHDOG_INTERVAL).await;
        tokio::task::yield_now().await;
    }
    assert!(token.is_cancelled(), "the watchdog cancelled the run");
    assert!(
        tokio::time::Instant::now() < deadline,
        "the cancel happened before the lease deadline"
    );
    assert!(
        map.lock().unwrap()["i-1"].fenced,
        "the run is marked fenced"
    );
    shutdown.cancel();
    let _ = watchdog.await;
}

/// Fix (bug 2): a run the watchdog fenced is abandoned, not failed — its lease and claim are
/// dropped and no failure retry is scheduled (which `handle_agent_down` would do for a plain
/// `Cancelled` exit). The workspace is kept (already sealed by the runner).
#[tokio::test(start_paused = true)]
async fn a_watchdog_fenced_run_is_abandoned_not_retried_as_a_failure() {
    let store = Store::open_in_memory_shared().unwrap();
    let started: Started = Arc::default();
    let mut h = Harness::with_store(
        json!({}),
        blocking_factory(Arc::clone(&started)),
        store.clone(),
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-1"));
    assert!(orch.state.leases.contains_key("i-1"));

    // Simulate the watchdog firing: mark fenced and cancel the worker, as `fire_expiring_lease_cancels` does.
    {
        let mut watches = orch.lease_watch.lock().unwrap();
        let watch = watches.get_mut("i-1").expect("the run is watched");
        watch.fenced = true;
        watch.cancel.cancel();
    }
    // The worker exits `Cancelled`; the actor processes the join as an abandon.
    run_until(&mut orch, |o| !o.state.running.contains_key("i-1")).await;

    assert!(orch.state.running.is_empty());
    assert!(
        !orch.state.claimed.contains("i-1"),
        "abandoned: the claim is released"
    );
    assert!(
        !orch.state.leases.contains_key("i-1"),
        "abandoned: the dispatch lease is dropped (a stale renewal can't revive it)"
    );
    assert!(
        !orch.state.retry_attempts.contains_key("i-1"),
        "abandon must not schedule a failure retry"
    );
    assert!(orch.state.blocked.is_empty());
}

/// A run that finished on its own (here `Ok`) while the actor was parked trips the fence flag too
/// (its lease neared the deadline), but it was never cancelled and carries a real outcome. Since we
/// fence before expiry, the lease is still ours: the outcome must be recorded, not abandoned and
/// requeued (which would re-run the agent — duplicate PRs/spend).
#[tokio::test(start_paused = true)]
async fn a_fenced_flag_does_not_abandon_a_run_that_finished_on_its_own() {
    let store = Store::open_in_memory_shared().unwrap();
    let mut h = Harness::with_store(json!({}), result_factory(|| Ok(())), store.clone());
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.leases.contains_key("i-1"));
    // The watchdog flags it (lease near its deadline) even though the worker finished Ok and was
    // never cancelled.
    orch.lease_watch
        .lock()
        .unwrap()
        .get_mut("i-1")
        .expect("the run is watched")
        .fenced = true;
    run_until(&mut orch, |o| o.state.completed.contains("i-1")).await;
    assert!(
        orch.state.completed.contains("i-1"),
        "the Ok outcome is recorded (not abandoned as a fence)"
    );
}

#[tokio::test(start_paused = true)]
async fn a_replica_drops_a_restored_retry_that_another_replica_holds() {
    let store = Store::open_in_memory().unwrap();
    store
        .acquire_lease("i-1", "other", std::time::Duration::from_secs(60))
        .await
        .unwrap();
    let started: Started = Arc::default();
    let mut h = replica(
        &store,
        "me",
        blocking_factory(Arc::clone(&started)),
        json!({}),
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.state.claimed.insert("i-1".into());
    orch.schedule_issue_retry(
        "i-1",
        Some(1),
        RetryMeta {
            identifier: Some("MT-1".into()),
            ..RetryMeta::default()
        },
        DelayType::Continuation,
    );
    run_until(&mut orch, |o| !o.state.claimed.contains("i-1")).await;
    assert!(orch.state.running.is_empty());
    assert!(orch.state.retry_attempts.is_empty());
    assert!(started.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_task_over_its_token_limit_fails() {
    use symphony_store::TaskStatus;
    let store = Store::open_in_memory().unwrap();
    let task = store
        .create_task(symphony_store::NewTask {
            prompt: "x".into(),
            limits: symphony_store::TaskLimits {
                max_tokens: Some(50),
                timeout_ms: None,
            },
            ..Default::default()
        })
        .await
        .unwrap();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}}),
        emitting_factory(vec![usage_event(40, 20)]),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    let failed = task_status(&store, task.id, TaskStatus::Failed).await;
    assert_eq!(
        failed.error.as_deref(),
        Some("task token limit exhausted: 60 of 50 tokens used")
    );
}

#[tokio::test(start_paused = true)]
async fn raising_the_issue_budget_releases_budget_blocked_issues() {
    let store = Store::open_in_memory().unwrap();
    let previous = store
        .start_run(symphony_store::NewRun::new("i-1", "MT-1"))
        .await
        .unwrap();
    store
        .update_usage(
            previous,
            symphony_store::TokenUsage {
                input: 100,
                cached: 0,
                output: 50,
                total: 150,
            },
            0,
        )
        .await
        .unwrap();
    let started: Started = Arc::default();
    let mut h = Harness::with_store(
        json!({"budget": {"max_tokens_per_issue": 100}}),
        blocking_factory(Arc::clone(&started)),
        store,
    );
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.blocked.contains_key("i-1"));
    // Still over budget: the block holds.
    orch.reconcile_blocked_issues().await;
    assert!(orch.state.blocked.contains_key("i-1"));

    h.wf.rewrite(json!({"budget": {"max_tokens_per_issue": 1000}}));
    orch.ctx.workflow.force_reload().unwrap();
    orch.reconcile_blocked_issues().await;
    assert!(!orch.state.blocked.contains_key("i-1"));
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch.state.running.contains_key("i-1"));
    orch.terminate_running_issue("i-1", false, "test").await;
}

#[tokio::test(start_paused = true)]
async fn the_kill_switch_of_one_replica_reaches_the_others() {
    let store = Store::open_in_memory().unwrap();
    let started: Started = Arc::default();
    let mut a = replica(
        &store,
        "a",
        blocking_factory(Arc::clone(&started)),
        json!({}),
    );
    let mut b = replica(
        &store,
        "b",
        blocking_factory(Arc::clone(&started)),
        json!({}),
    );
    b.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    let mut workers_a = WorkerSet::default();
    let mut workers_b = WorkerSet::default();
    let mut orch_a = a.orchestrator(&mut workers_a);
    let mut orch_b = b.orchestrator(&mut workers_b);
    orch_b
        .dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert!(orch_b.state.running.contains_key("i-1"));

    let _ = orch_a.set_paused(true, Some("incident".into()));
    let mut persisted = false;
    for _ in 0..100 {
        if store.control(PAUSED_CONTROL_KEY).await.unwrap().is_some() {
            persisted = true;
            break;
        }
        tokio::time::sleep(ms(5)).await;
    }
    assert!(persisted);
    orch_b.sync_kill_switch().await;
    orch_b.enforce_spend().await;
    assert_eq!(orch_b.state.spend.paused.as_deref(), Some("incident"));
    assert!(orch_b.state.running.is_empty(), "b stopped its agents");

    let _ = orch_a.set_paused(false, None);
    for _ in 0..100 {
        if store.control(PAUSED_CONTROL_KEY).await.unwrap().is_none() {
            break;
        }
        tokio::time::sleep(ms(5)).await;
    }
    orch_b.sync_kill_switch().await;
    assert_eq!(orch_b.state.spend.paused, None);
}

#[tokio::test]
async fn task_workspaces_are_separate_from_tracker_issues_and_removed_when_final() {
    use symphony_store::TaskStatus;
    let (store, id) = task_store().await;
    // A tracker issue whose identifier looks like a task's must not share anything with it.
    let started: Started = Arc::default();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "memory"}}),
        task_factory(Arc::default(), || Ok(())),
        store.clone(),
    );
    h.memory
        .set(vec![issue("lin-1", &format!("task-{id}"), "In Progress")]);
    let _ = started;
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    let workspace = orch.task_workspace(id);
    assert!(
        workspace.ends_with(format!(".tasks/{id}")),
        "{}",
        workspace.display()
    );
    std::fs::create_dir_all(workspace.join("clone")).unwrap();
    orch.maybe_dispatch().await;
    run_until(&mut orch, |o| o.state.running.is_empty()).await;
    task_status(&store, id, TaskStatus::Succeeded).await;
    let mut removed = false;
    for _ in 0..200 {
        if !workspace.exists() {
            removed = true;
            break;
        }
        tokio::time::sleep(ms(5)).await;
    }
    assert!(removed, "the workspace of a final task is removed");

    // A stale workspace of a finished task is swept at startup; a queued task keeps its own.
    std::fs::create_dir_all(&workspace).unwrap();
    let queued = store
        .create_task(symphony_store::NewTask {
            prompt: "later".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let kept = orch.task_workspace(queued.id);
    std::fs::create_dir_all(&kept).unwrap();
    orch.sweep_task_workspaces().await;
    for _ in 0..200 {
        if !workspace.exists() {
            break;
        }
        tokio::time::sleep(ms(5)).await;
    }
    assert!(!workspace.exists());
    assert!(kept.exists());
}

// ----- the per-event path: run index, loaded settings, spend check of one issue --------------------

/// What a test holds of a worker started by [`fed_factory`]: its event sink and a way to end it.
struct Fed {
    events: EventSink,
    exit: mpsc::UnboundedSender<WorkerResult>,
}

/// The workers a [`fed_factory`] started, by issue id (the newest run of each issue).
type Feds = Arc<Mutex<std::collections::HashMap<String, Fed>>>;

/// Worker that does nothing by itself: the test emits its events ([`feed`]) and ends it
/// ([`end_run`]); until then it runs until it is cancelled.
fn fed_factory(feds: Feds) -> Arc<dyn WorkerFactory> {
    Arc::new(FnWorkerFactory(move |ctx: WorkerContext| {
        let (exit, mut exits) = mpsc::unbounded_channel();
        feds.lock().unwrap().insert(
            ctx.issue.id.clone().unwrap_or_default(),
            Fed {
                events: ctx.events.clone(),
                exit,
            },
        );
        async move {
            tokio::select! {
                () = ctx.cancel.cancelled() => Err(RunError::Cancelled),
                Some(result) = exits.recv() => result,
            }
        }
    }))
}

/// Emits `event` as the running agent of `issue_id` and steps the orchestrator until it has
/// integrated it: the entry carries the event's time, or the event stopped the run.
async fn feed(orch: &mut Orchestrator<'_>, feds: &Feds, issue_id: &str, mut event: CodexEvent) {
    let entry = orch.state.running.get(issue_id).expect("issue is running");
    // Later than the entry's last event, whatever the clock's resolution.
    if let Some(last) = entry.last_codex_timestamp {
        event.timestamp = event
            .timestamp
            .max(last + chrono::Duration::microseconds(1));
    }
    let at = event.timestamp;
    feds.lock().unwrap()[issue_id].events.emit(event);
    run_until(orch, |o| {
        o.state
            .running
            .get(issue_id)
            .is_none_or(|entry| entry.last_codex_timestamp == Some(at))
    })
    .await;
}

/// Ends the worker of `issue_id` with `result` and steps until the orchestrator handled its exit.
async fn end_run(orch: &mut Orchestrator<'_>, feds: &Feds, issue_id: &str, result: WorkerResult) {
    let run_id = orch.state.running.get(issue_id).expect("running").run_id;
    feds.lock().unwrap()[issue_id].exit.send(result).unwrap();
    run_until(orch, |o| {
        o.state
            .running
            .get(issue_id)
            .is_none_or(|entry| entry.run_id != run_id)
    })
    .await;
}

fn some_event() -> CodexEvent {
    notification(json!({"method": "some-event"}))
}

/// `i-1` and `i-2` running on fed workers.
async fn dispatch_two(h: &Harness, orch: &mut Orchestrator<'_>) {
    let issues = vec![
        issue("i-1", "MT-1", "In Progress"),
        issue("i-2", "MT-2", "In Progress"),
    ];
    h.memory.set(issues.clone());
    for issue in issues {
        orch.dispatch_issue(issue, None, None).await;
    }
    assert_eq!(orch.state.running.len(), 2);
}

#[tokio::test]
async fn the_run_index_follows_inserts_replacements_removals_and_drains() {
    let entry = |id: &str| fake_entry(id, issue(id, id, "Todo"), None);
    let issue_of = |running: &mut state::Running, run_id: u64| {
        running.by_run_mut(run_id).map(|(id, _)| id.to_owned())
    };
    let mut running = state::Running::default();
    let (a, b) = (entry("a"), entry("b"));
    let (run_a, run_b) = (a.run_id, b.run_id);
    assert!(running.insert("a".into(), a).is_none());
    assert!(running.insert("b".into(), b).is_none());
    running.assert_index_matches();
    assert_eq!(issue_of(&mut running, run_a).as_deref(), Some("a"));
    assert_eq!(issue_of(&mut running, run_b).as_deref(), Some("b"));
    assert_eq!(issue_of(&mut running, run_b + 1), None);

    // A newer run of the same issue replaces the entry, and the older run is not found again.
    let rerun = entry("a");
    let run_a2 = rerun.run_id;
    assert_eq!(running.insert("a".into(), rerun).unwrap().run_id, run_a);
    running.assert_index_matches();
    assert_eq!(issue_of(&mut running, run_a), None);
    assert_eq!(issue_of(&mut running, run_a2).as_deref(), Some("a"));

    assert_eq!(running.remove("a").unwrap().run_id, run_a2);
    assert!(running.remove("a").is_none());
    running.assert_index_matches();
    assert_eq!(issue_of(&mut running, run_a2), None);
    assert_eq!(issue_of(&mut running, run_b).as_deref(), Some("b"));

    assert_eq!(running.drain().count(), 1);
    assert!(running.is_empty());
    running.assert_index_matches();
    assert_eq!(issue_of(&mut running, run_b), None);

    // The lookup compares the entry's own run id too: an index entry that pointed at another
    // run's entry would find nothing instead of delivering to it.
    let c = entry("c");
    let run_c = c.run_id;
    running.insert("c".into(), c);
    running.get_mut("c").unwrap().run_id = run_c + 1;
    assert_eq!(issue_of(&mut running, run_c), None);
}

#[tokio::test(start_paused = true)]
async fn events_and_worker_messages_reach_their_own_run_and_stale_ones_reach_none() {
    let feds: Feds = Arc::default();
    let mut h = Harness::new(json!({}), fed_factory(Arc::clone(&feds)));
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    dispatch_two(&h, &mut orch).await;
    let run_1 = orch.state.running.get("i-1").unwrap().run_id;
    let run_2 = orch.state.running.get("i-2").unwrap().run_id;
    let info = |run_id, path: &str| WorkerMessage::RuntimeInfo {
        run_id,
        worker_host: None,
        workspace_path: path.to_owned(),
    };

    let integrated = orch.handle_codex_event(run_2, session_started("s-2"));
    assert_eq!(integrated.as_deref(), Some("i-2"));
    orch.handle_worker_message(info(run_1, "/w/MT-1"));
    let (one, two) = (
        orch.state.running.get("i-1").unwrap(),
        orch.state.running.get("i-2").unwrap(),
    );
    assert_eq!(
        (one.session_id.as_deref(), one.workspace_path.as_deref()),
        (None, Some("/w/MT-1"))
    );
    assert_eq!(
        (two.session_id.as_deref(), two.workspace_path.as_deref()),
        (Some("s-2"), None)
    );

    // `i-1` stops and runs again as a newer run: whatever the old run still sends finds nothing.
    orch.terminate_running_issue("i-1", false, "test").await;
    assert_eq!(
        orch.handle_codex_event(run_1, session_started("late")),
        None
    );
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    assert_ne!(orch.state.running.get("i-1").unwrap().run_id, run_1);
    assert_eq!(
        orch.handle_codex_event(run_1, session_started("late")),
        None
    );
    orch.handle_worker_message(info(run_1, "/w/late"));
    orch.handle_worker_message(WorkerMessage::TaskResult {
        run_id: run_1,
        result: "late".into(),
    });
    let rerun = orch.state.running.get("i-1").unwrap();
    assert_eq!(
        (rerun.session_id.as_deref(), &rerun.workspace_path),
        (None, &None)
    );
    orch.state.running.assert_index_matches();
    orch.stop_all_workers().await;
}

/// The run index has no entry of a run that ended, whichever way it ended: an agent event of that
/// run finds nothing afterwards, and the index still matches the running entries.
#[tokio::test(start_paused = true)]
async fn every_way_a_run_ends_takes_it_out_of_the_run_index() {
    // A shared store, so runs are watched by the lease watchdog (the fence below needs it).
    let store = Store::open_in_memory_shared().unwrap();
    let feds: Feds = Arc::default();
    let mut h = Harness::with_store(
        json!({"codex": {"stall_timeout_ms": 1000}}),
        fed_factory(Arc::clone(&feds)),
        store,
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);

    /// Starts a run of `id` and returns its run id.
    async fn start(h: &Harness, orch: &mut Orchestrator<'_>, id: &str) -> u64 {
        let issue = issue(id, &id.to_uppercase(), "In Progress");
        h.memory.set(vec![issue.clone()]);
        orch.dispatch_issue(issue, None, None).await;
        orch.state.running.assert_index_matches();
        orch.state.running.get(id).expect("dispatched").run_id
    }
    /// The run is gone from the entries and from the index; the issue is let go for the next case.
    fn ended(orch: &mut Orchestrator<'_>, id: &str, run_id: u64, how: &str) {
        assert!(!orch.state.running.contains_key(id), "{how}: still running");
        orch.state.running.assert_index_matches();
        assert_eq!(
            orch.handle_codex_event(run_id, some_event()),
            None,
            "{how}: an event of the ended run found an entry"
        );
        orch.release_issue_claim(id);
    }

    let run = start(&h, &mut orch, "exit-ok").await;
    end_run(&mut orch, &feds, "exit-ok", Ok(())).await;
    ended(&mut orch, "exit-ok", run, "normal worker exit");

    let run = start(&h, &mut orch, "exit-err").await;
    let failure = Err(RunError::Other("boom".into()));
    end_run(&mut orch, &feds, "exit-err", failure).await;
    ended(&mut orch, "exit-err", run, "failed worker exit");

    let run = start(&h, &mut orch, "terminated").await;
    orch.terminate_running_issue("terminated", false, "test")
        .await;
    ended(&mut orch, "terminated", run, "terminate");

    let run = start(&h, &mut orch, "lease-lost").await;
    orch.abandon_running_issue("lease-lost", "dispatch lease lost")
        .await;
    ended(&mut orch, "lease-lost", run, "abandon after a lost lease");

    let run = start(&h, &mut orch, "blocked").await;
    orch.stop_and_block("blocked", "issue token budget exhausted".into())
        .await;
    ended(&mut orch, "blocked", run, "stop and block");

    // Stalled while waiting for operator input: blocked by the stall check.
    let run = start(&h, &mut orch, "stalled").await;
    feed(&mut orch, &feds, "stalled", input_required()).await;
    tokio::time::advance(ms(5_000)).await;
    orch.reconcile_stalled_running_issues().await;
    assert!(orch.state.blocked.contains_key("stalled"));
    ended(&mut orch, "stalled", run, "stall of a blocked agent");

    // Fenced by the lease watchdog: the worker's `Cancelled` exit is an abandon.
    let run = start(&h, &mut orch, "fenced").await;
    {
        let mut watches = orch.lease_watch.lock().unwrap();
        let watch = watches.get_mut("fenced").expect("the run is watched");
        watch.fenced = true;
        watch.cancel.cancel();
    }
    run_until(&mut orch, |o| !o.state.running.contains_key("fenced")).await;
    ended(&mut orch, "fenced", run, "lease fence");

    // Shutdown ends whatever is still running.
    let run_a = start(&h, &mut orch, "last-a").await;
    let run_b = start(&h, &mut orch, "last-b").await;
    assert_eq!(orch.state.running.len(), 2);
    orch.stop_all_workers().await;
    ended(&mut orch, "last-a", run_a, "shutdown");
    ended(&mut orch, "last-b", run_b, "shutdown");
    assert!(orch.state.running.is_empty());
}

/// The kill switch, engaged without the full check that follows a command, stops every running
/// agent at the next event of any one of them.
#[tokio::test(start_paused = true)]
async fn the_kill_switch_stops_every_agent_at_the_next_agent_event() {
    let feds: Feds = Arc::default();
    let mut h = Harness::new(json!({}), fed_factory(Arc::clone(&feds)));
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    dispatch_two(&h, &mut orch).await;

    feed(&mut orch, &feds, "i-2", some_event()).await;
    assert_eq!(orch.state.running.len(), 2, "not paused: both keep running");

    assert!(orch.set_paused(true, Some("incident".into())).paused);
    feed(&mut orch, &feds, "i-2", some_event()).await;
    assert!(orch.state.running.is_empty(), "i-1 had no event of its own");
    // Stopped, not blocked: both run again once the switch is released.
    assert!(orch.state.claimed.is_empty());
    assert!(orch.state.blocked.is_empty());
}

/// The daily budget, reached by one agent's usage, stops every running agent in the same step.
#[tokio::test(start_paused = true)]
async fn an_exhausted_day_stops_every_agent_at_the_event_that_reached_the_budget() {
    let feds: Feds = Arc::default();
    let mut h = Harness::new(
        json!({"budget": {"max_tokens_per_day": 50}}),
        fed_factory(Arc::clone(&feds)),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    dispatch_two(&h, &mut orch).await;

    feed(&mut orch, &feds, "i-1", usage_event(30, 10)).await;
    assert_eq!(
        orch.state.running.len(),
        2,
        "40 of 50 tokens: both keep running"
    );

    feed(&mut orch, &feds, "i-1", usage_event(40, 20)).await;
    assert!(orch.state.running.is_empty(), "i-2 had no event of its own");
    assert!(orch.state.claimed.is_empty());
    assert!(orch.state.blocked.is_empty());
    assert_eq!(
        orch.snapshot().spend.stopped_reason.as_deref(),
        Some("daily token budget exhausted: 60 of 50 tokens used today (UTC)")
    );
}

/// An issue budget stops the issue whose usage reached it, when it reaches it, and no other.
#[tokio::test(start_paused = true)]
async fn an_issue_budget_stops_the_issue_whose_event_reached_it_and_no_other() {
    let feds: Feds = Arc::default();
    let mut h = Harness::new(
        json!({"budget": {"max_tokens_per_issue": 100}}),
        fed_factory(Arc::clone(&feds)),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    dispatch_two(&h, &mut orch).await;

    feed(&mut orch, &feds, "i-1", usage_event(60, 10)).await;
    feed(&mut orch, &feds, "i-2", usage_event(100, 20)).await;
    assert_eq!(
        orch.state.blocked["i-2"].error,
        "issue token budget exhausted: 120 of 100 tokens used"
    );
    assert!(orch.state.running.contains_key("i-1"), "70 of 100 tokens");
    assert!(!orch.state.blocked.contains_key("i-1"));

    feed(&mut orch, &feds, "i-1", usage_event(90, 20)).await;
    assert_eq!(
        orch.state.blocked["i-1"].error,
        "issue token budget exhausted: 110 of 100 tokens used"
    );
    assert!(orch.state.running.is_empty());
}

/// A task's token limit fails the task whose usage reached it and leaves the other task running.
#[tokio::test(start_paused = true)]
async fn a_task_token_limit_stops_the_task_whose_event_reached_it_and_no_other() {
    use symphony_store::TaskStatus;
    let store = Store::open_in_memory().unwrap();
    let mut tasks = Vec::new();
    for max_tokens in [50, 500] {
        let task = store
            .create_task(symphony_store::NewTask {
                prompt: "x".into(),
                limits: symphony_store::TaskLimits {
                    max_tokens: Some(max_tokens),
                    timeout_ms: None,
                },
                ..Default::default()
            })
            .await
            .unwrap();
        tasks.push(task.id);
    }
    let (small, large) = (task_issue_id(tasks[0]), task_issue_id(tasks[1]));
    let feds: Feds = Arc::default();
    let mut h = Harness::with_store(
        json!({"tracker": {"kind": "none"}}),
        fed_factory(Arc::clone(&feds)),
        store.clone(),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    orch.maybe_dispatch().await;
    assert_eq!(orch.state.running.len(), 2);

    // The same usage is over the first task's limit and within the second's.
    feed(&mut orch, &feds, &large, usage_event(40, 20)).await;
    assert_eq!(orch.state.running.len(), 2);
    feed(&mut orch, &feds, &small, usage_event(40, 20)).await;
    assert!(!orch.state.running.contains_key(&small));
    assert!(orch.state.running.contains_key(&large));
    let failed = task_status(&store, tasks[0], TaskStatus::Failed).await;
    assert_eq!(
        failed.error.as_deref(),
        Some("task token limit exhausted: 60 of 50 tokens used")
    );
    assert_eq!(
        store.get_task(tasks[1]).await.unwrap().unwrap().status,
        TaskStatus::Running
    );
    orch.stop_all_workers().await;
}

/// An agent event does not read `WORKFLOW.md`. Once an edit has been loaded (here by the
/// workflow store's poll, which the binary runs every second), every running issue is checked
/// against it without any agent sending an event.
#[tokio::test(start_paused = true)]
async fn a_reloaded_budget_is_checked_for_every_run_without_waiting_for_an_event() {
    let feds: Feds = Arc::default();
    let mut h = Harness::new(
        json!({"budget": {"max_tokens_per_issue": 1000}}),
        fed_factory(Arc::clone(&feds)),
    );
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    dispatch_two(&h, &mut orch).await;
    feed(&mut orch, &feds, "i-1", usage_event(50, 10)).await;

    // An operator lowers the budget below what `i-1` has used.
    h.wf.rewrite(json!({"budget": {"max_tokens_per_issue": 50}}));
    feed(&mut orch, &feds, "i-1", some_event()).await;
    feed(&mut orch, &feds, "i-2", some_event()).await;
    assert_eq!(
        orch.settings().budget.max_tokens_per_issue,
        Some(1000),
        "agent events left the file alone"
    );
    assert_eq!(orch.state.running.len(), 2);

    // The load wakes the orchestrator; both agents stay silent.
    h.wf.store.poll();
    assert!(orch.step().await);
    assert_eq!(
        orch.state.blocked["i-1"].error,
        "issue token budget exhausted: 60 of 50 tokens used"
    );
    assert!(orch.state.running.contains_key("i-2"));

    // An edit that does not load keeps the last good budget in force, for events too, and the
    // orchestrator is not woken for it (the next step is the event's).
    h.wf.rewrite(json!({"budget": {"max_tokens_per_issue": 0}}));
    h.wf.store.poll();
    feed(&mut orch, &feds, "i-2", usage_event(40, 5)).await;
    assert!(orch.state.running.contains_key("i-2"), "45 of 50 tokens");
    feed(&mut orch, &feds, "i-2", usage_event(40, 15)).await;
    assert_eq!(
        orch.state.blocked["i-2"].error,
        "issue token budget exhausted: 55 of 50 tokens used"
    );
}

/// An agent event, a worker message and a worker exit use the loaded settings and leave
/// `WORKFLOW.md` alone. A tick, a poll cycle, a snapshot and a due retry re-read it.
#[tokio::test(start_paused = true)]
async fn agent_events_and_exits_use_the_loaded_workflow_and_polls_and_retries_re_read_it() {
    let feds: Feds = Arc::default();
    let mut h = Harness::new(json!({}), fed_factory(Arc::clone(&feds)));
    let mut workers = WorkerSet::default();
    let mut orch = h.orchestrator(&mut workers);
    let limit = |agents: u32| json!({"agent": {"max_concurrent_agents": agents}});
    let loaded = |orch: &Orchestrator<'_>| orch.settings().agent.max_concurrent_agents;
    h.memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
    orch.dispatch_issue(issue("i-1", "MT-1", "In Progress"), None, None)
        .await;
    let run_id = orch.state.running.get("i-1").unwrap().run_id;
    assert_eq!(loaded(&orch), 10);

    h.wf.rewrite(limit(11));
    feed(&mut orch, &feds, "i-1", usage_event(1, 1)).await;
    orch.handle_worker_message(WorkerMessage::RuntimeInfo {
        run_id,
        worker_host: None,
        workspace_path: "/w/MT-1".into(),
    });
    end_run(&mut orch, &feds, "i-1", Err(RunError::Other("boom".into()))).await;
    assert!(orch.state.retry_attempts.contains_key("i-1"));
    assert_eq!(
        loaded(&orch),
        10,
        "event, message and exit used what was loaded"
    );

    orch.handle_tick(None);
    assert_eq!(loaded(&orch), 11);
    assert_eq!(orch.state.max_concurrent_agents, 11);
    h.wf.rewrite(limit(12));
    orch.run_poll_cycle().await;
    assert_eq!(loaded(&orch), 12);
    // By the cycle's own read, before its dispatch preflight reads the file again.
    assert_eq!(orch.state.max_concurrent_agents, 12);
    h.wf.rewrite(limit(13));
    assert_eq!(orch.snapshot().max_concurrent_agents, 13);
    h.wf.rewrite(limit(14));
    orch.handle_retry_issue("i-gone", 1, RetryMeta::default())
        .await;
    assert_eq!(loaded(&orch), 14);
}
