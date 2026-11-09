//! The world outside the orchestrator: the issue board behind the memory tracker, the fake worker
//! and the counters both feed.
//!
//! The fake worker stands in for a Codex run: it reports its workspace, emits a `session_started`
//! event, then emits Codex-style notifications at the configured rate until its run length is over
//! (it then hands the issue off, like an agent moving its ticket to review) or it is cancelled. No
//! process is spawned and no model is called.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use symphony_codex::{CodexEvent, CodexEventData, StreamMessage};
use symphony_core::Issue;
use symphony_core::config::TrackerSettings;
use symphony_runtime::{RunError, WorkerContext, WorkerResult};
use symphony_trackers::{MemoryIssues, MemoryTracker, Tracker, TrackerError};

/// State of an issue waiting for, or held by, an agent.
pub const ACTIVE_STATE: &str = "Todo";
/// State a finished worker leaves its issue in (neither active nor terminal).
pub const HANDOFF_STATE: &str = "Human Review";
/// Label of an issue whose fake run ends after [`Plan::short_run`] at most (arrival probes).
pub const SHORT_RUN_LABEL: &str = "short-run";
/// How often the board applies queued changes to the tracker.
pub const BOARD_TICK: Duration = Duration::from_millis(50);
/// A handed-off issue stays on the board this long (the orchestrator's continuation check, one
/// second after the exit by default, must still find it).
const HANDOFF_RETENTION: Duration = Duration::from_secs(5);
/// Shortest and longest pause between two emission bursts of one worker.
const MIN_GAP: f64 = 0.010;
const MAX_GAP: f64 = 0.250;
/// Of every `MIX` events, one updates the token totals, two are item lifecycle events (recorded
/// in run history) and the rest are streaming deltas (throttled by the recorder).
const MIX: u64 = 20;

/// How long a fake run lasts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunLength {
    /// Until cancelled.
    Forever,
    /// Uniform between the bounds, then a normal exit.
    Uniform(Duration, Duration),
}

/// What the workers do; the rate can be changed while they run.
#[derive(Debug)]
pub struct Plan {
    /// Events per second of each worker, times 1000.
    rate_milli: AtomicU64,
    pub run_length: RunLength,
    /// Longest run of an issue labelled [`SHORT_RUN_LABEL`] (the shortest is a third of it).
    pub short_run: Duration,
    /// Add a fresh eligible issue whenever a worker starts (keeps a standing backlog).
    pub refill: bool,
}

impl Plan {
    pub fn new(rate: f64, run_length: RunLength, refill: bool) -> Self {
        Self {
            rate_milli: AtomicU64::new((rate * 1_000.0) as u64),
            run_length,
            short_run: Duration::from_secs(9),
            refill,
        }
    }

    /// Sets the longest run of short-run issues.
    pub fn with_short_run(mut self, longest: Duration) -> Self {
        self.short_run = longest;
        self
    }

    /// Sets each worker's event rate (events per second).
    pub fn set_rate(&self, rate: f64) {
        self.rate_milli
            .store((rate * 1_000.0) as u64, Ordering::Relaxed);
    }

    fn rate(&self) -> f64 {
        self.rate_milli.load(Ordering::Relaxed) as f64 / 1_000.0
    }
}

/// Counters and samples written by the workers and the board.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Codex events handed to the orchestrator (every kind).
    pub emitted: AtomicU64,
    pub started: AtomicU64,
    pub exited: AtomicU64,
    samples: Mutex<Samples>,
}

#[derive(Debug, Default)]
struct Samples {
    /// Issue became eligible -> its worker started.
    dispatch: Vec<Duration>,
    /// A worker exited -> the next worker started (while work was waiting).
    slot_idle: Vec<Duration>,
    /// When each worker started.
    starts: Vec<Instant>,
}

impl Metrics {
    fn samples(&self) -> MutexGuard<'_, Samples> {
        // The guarded data stays valid when a holder panicked: plain pushes.
        self.samples.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn dispatch_latencies(&self) -> Vec<Duration> {
        self.samples().dispatch.clone()
    }

    pub fn slot_idle(&self) -> Vec<Duration> {
        self.samples().slot_idle.clone()
    }

    pub fn starts(&self) -> Vec<Instant> {
        self.samples().starts.clone()
    }
}

#[derive(Debug, Default)]
struct Board {
    issues: Vec<Issue>,
    /// Issues created so far (the sequence number of the newest).
    next: u64,
    /// Value of `next` when eligible times were last recorded.
    stamped: u64,
    eligible_at: HashMap<String, Instant>,
    /// Exit times of workers whose slot has not been taken again yet.
    freed: VecDeque<Instant>,
    /// Issues to add at the next tick.
    pending_adds: u64,
    /// Issues to hand off at the next tick.
    pending_handoffs: Vec<String>,
    handed_off: VecDeque<(Instant, String)>,
}

/// The issue board, the worker plan and the metrics.
#[derive(Debug)]
pub struct World {
    tracker_issues: MemoryIssues,
    board: Mutex<Board>,
    pub plan: Plan,
    pub metrics: Metrics,
}

fn new_issue(seq: u64) -> Issue {
    Issue {
        id: Some(format!("load-{seq:07}")),
        identifier: Some(format!("LOAD-{seq}")),
        title: Some(format!("Load test issue {seq}")),
        description: Some(
            "Synthetic issue of the load harness. The body has the size of a short ticket: a \
             paragraph that says what is wrong, where it was seen, and what done looks like, so \
             copies of the issue cost what copies of a real one would."
                .to_owned(),
        ),
        priority: Some(2),
        state: Some(ACTIVE_STATE.to_owned()),
        url: Some(format!("https://tracker.example/issues/LOAD-{seq}")),
        dispatchable: true,
        created_at: Some(Utc::now()),
        updated_at: Some(Utc::now()),
        ..Issue::default()
    }
}

impl World {
    pub fn new(plan: Plan) -> Arc<Self> {
        Arc::new(Self {
            tracker_issues: MemoryIssues::new(),
            board: Mutex::new(Board::default()),
            plan,
            metrics: Metrics::default(),
        })
    }

    /// The list the memory tracker serves.
    pub fn tracker_issues(&self) -> MemoryIssues {
        self.tracker_issues.clone()
    }

    fn board(&self) -> MutexGuard<'_, Board> {
        // The board stays consistent when a holder panicked: every change is applied whole.
        self.board.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Makes `count` new issues eligible now and returns that instant.
    pub fn add_issues(&self, count: u64) -> Instant {
        let mut board = self.board();
        Self::push_issues(&mut board, count);
        self.tracker_issues.set(board.issues.clone());
        let now = Instant::now();
        Self::stamp_unstamped(&mut board, now);
        now
    }

    /// Makes one new issue eligible now whose run ends after a few seconds.
    pub fn add_short_issue(&self) {
        let mut board = self.board();
        board.next += 1;
        let mut issue = new_issue(board.next);
        issue.labels = vec![SHORT_RUN_LABEL.to_owned()];
        board.issues.push(issue);
        self.tracker_issues.set(board.issues.clone());
        Self::stamp_unstamped(&mut board, Instant::now());
    }

    fn push_issues(board: &mut Board, count: u64) {
        for _ in 0..count {
            board.next += 1;
            let issue = new_issue(board.next);
            board.issues.push(issue);
        }
    }

    /// Records `now` as the eligible time of the issues added since the last publication (new
    /// issues are appended, so they are the tail of the list).
    fn stamp_unstamped(board: &mut Board, now: Instant) {
        let fresh = usize::try_from(board.next - board.stamped).unwrap_or(usize::MAX);
        let Board {
            issues,
            eligible_at,
            ..
        } = board;
        let first = issues.len().saturating_sub(fresh);
        for issue in &issues[first..] {
            if let Some(id) = &issue.id {
                eligible_at.insert(id.clone(), now);
            }
        }
        board.stamped = board.next;
    }

    /// Applies queued hand-offs and refills to the tracker (one publication per call).
    pub fn tick(&self) {
        let mut board = self.board();
        let now = Instant::now();
        let mut dirty = false;
        if !board.pending_handoffs.is_empty() {
            let ids: HashSet<String> = board.pending_handoffs.drain(..).collect();
            for issue in &mut board.issues {
                if issue.id.as_ref().is_some_and(|id| ids.contains(id)) {
                    issue.state = Some(HANDOFF_STATE.to_owned());
                }
            }
            board.handed_off.extend(ids.into_iter().map(|id| (now, id)));
            dirty = true;
        }
        let mut expired = HashSet::new();
        while board
            .handed_off
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > HANDOFF_RETENTION)
        {
            if let Some((_, id)) = board.handed_off.pop_front() {
                expired.insert(id);
            }
        }
        if !expired.is_empty() {
            board
                .issues
                .retain(|issue| !issue.id.as_ref().is_some_and(|id| expired.contains(id)));
            dirty = true;
        }
        let adds = std::mem::take(&mut board.pending_adds);
        if adds > 0 {
            Self::push_issues(&mut board, adds);
            dirty = true;
        }
        if dirty {
            self.tracker_issues.set(board.issues.clone());
            Self::stamp_unstamped(&mut board, Instant::now());
        }
    }

    /// Runs [`World::tick`] every [`BOARD_TICK`] until the task is aborted.
    pub fn spawn_board(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let world = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(BOARD_TICK);
            loop {
                ticker.tick().await;
                world.tick();
            }
        })
    }

    fn on_start(&self, issue_id: &str) {
        let now = Instant::now();
        let (eligible, freed) = {
            let mut board = self.board();
            if self.plan.refill {
                board.pending_adds += 1;
            }
            (board.eligible_at.remove(issue_id), board.freed.pop_front())
        };
        let mut samples = self.metrics.samples();
        samples.starts.push(now);
        if let Some(at) = eligible {
            samples.dispatch.push(now.duration_since(at));
        }
        if let Some(at) = freed {
            samples.slot_idle.push(now.duration_since(at));
        }
        drop(samples);
        self.metrics.started.fetch_add(1, Ordering::Relaxed);
    }

    fn on_exit(&self, issue_id: &str) {
        let mut board = self.board();
        board.pending_handoffs.push(issue_id.to_owned());
        board.freed.push_back(Instant::now());
        drop(board);
        self.metrics.exited.fetch_add(1, Ordering::Relaxed);
    }

    /// One fake agent run (the body of the runtime's worker factory).
    pub async fn run_worker(self: Arc<Self>, ctx: WorkerContext) -> WorkerResult {
        let issue_id = ctx.issue.id.clone().unwrap_or_default();
        self.on_start(&issue_id);
        let mut events = EventSource::new(ctx.run_id);
        ctx.reporter
            .runtime_info(None, &format!("/tmp/symphony-load/{issue_id}"));
        ctx.events.emit(events.session_started());
        self.metrics.emitted.fetch_add(1, Ordering::Relaxed);

        let mut random = SplitMix(ctx.run_id);
        let short = ctx.issue.labels.iter().any(|l| l == SHORT_RUN_LABEL);
        let run_length = if short {
            RunLength::Uniform(self.plan.short_run / 3, self.plan.short_run)
        } else {
            self.plan.run_length
        };
        let run_for = match run_length {
            RunLength::Forever => None,
            RunLength::Uniform(low, high) => {
                Some(low + high.saturating_sub(low).mul_f64(random.unit()))
            }
        };
        let started = Instant::now();
        let mut last = started;
        let mut owed = 0.0_f64;
        // A random phase keeps the workers from emitting in lockstep.
        let mut gap = MAX_GAP * random.unit();
        loop {
            tokio::select! {
                () = ctx.cancel.cancelled() => return Err(RunError::Cancelled),
                () = tokio::time::sleep(Duration::from_secs_f64(gap)) => {}
            }
            let now = Instant::now();
            if run_for.is_some_and(|limit| now.duration_since(started) >= limit) {
                break;
            }
            let rate = self.plan.rate();
            // Open loop: what is owed follows the clock, not the last burst, so a slow consumer
            // does not slow the producer down. At most one second is caught up at once.
            owed = (owed + rate * now.duration_since(last).as_secs_f64()).min(rate.max(1.0));
            last = now;
            let burst = owed.floor() as u64;
            owed -= burst as f64;
            for _ in 0..burst {
                ctx.events.emit(events.next());
            }
            self.metrics.emitted.fetch_add(burst, Ordering::Relaxed);
            gap = if rate > 0.0 {
                (1.0 / rate).clamp(MIN_GAP, MAX_GAP)
            } else {
                MAX_GAP
            };
        }
        ctx.events.emit(events.turn_completed());
        self.metrics.emitted.fetch_add(1, Ordering::Relaxed);
        self.on_exit(&issue_id);
        Ok(())
    }
}

/// Deterministic per-run randomness (no dependency, repeatable runs).
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Builds the events of one fake run, shaped like Codex app-server messages.
struct EventSource {
    thread_id: String,
    pid: Option<String>,
    seq: u64,
}

impl EventSource {
    fn new(run_id: u64) -> Self {
        Self {
            thread_id: format!("thread-{run_id:08x}"),
            pid: Some("4242".to_owned()),
            seq: 0,
        }
    }

    fn session_started(&self) -> CodexEvent {
        CodexEvent::new(
            CodexEventData::SessionStarted {
                session_id: format!("{}-turn-1", self.thread_id),
                thread_id: self.thread_id.clone(),
                turn_id: "turn-1".to_owned(),
            },
            self.pid.clone(),
            None,
        )
    }

    fn message(&self, payload: serde_json::Value) -> StreamMessage {
        StreamMessage {
            raw: payload.to_string(),
            payload,
        }
    }

    fn notification(&self, payload: serde_json::Value) -> CodexEvent {
        CodexEvent::new(
            CodexEventData::Notification(self.message(payload)),
            self.pid.clone(),
            None,
        )
    }

    fn turn_completed(&self) -> CodexEvent {
        let payload = json!({
            "method": "turn/completed",
            "params": {"threadId": self.thread_id, "turn": {"id": "turn-1", "status": "completed"}},
        });
        CodexEvent::new(
            CodexEventData::TurnCompleted(self.message(payload)),
            self.pid.clone(),
            None,
        )
    }

    fn next(&mut self) -> CodexEvent {
        self.seq += 1;
        let seq = self.seq;
        let item = format!("item-{}", seq / MIX);
        let payload = match seq % MIX {
            0 => json!({
                "method": "thread/tokenUsage/updated",
                "params": {
                    "threadId": self.thread_id,
                    "turnId": "turn-1",
                    "tokenUsage": {
                        "total": {
                            "inputTokens": 2_000 + seq * 40,
                            "cachedInputTokens": 1_000 + seq * 30,
                            "outputTokens": seq * 6,
                            "totalTokens": 2_000 + seq * 46,
                        },
                        "last": {"inputTokens": 800, "outputTokens": 120, "totalTokens": 920},
                    },
                },
            }),
            7 => json!({
                "method": "item/started",
                "params": {
                    "threadId": self.thread_id,
                    "turnId": "turn-1",
                    "item": {
                        "type": "commandExecution",
                        "id": item,
                        "command": "cargo test -p symphony-runtime --lib orchestrator",
                        "cwd": "/workspaces/LOAD/repo",
                        "status": "inProgress",
                    },
                },
            }),
            14 => json!({
                "method": "item/completed",
                "params": {
                    "threadId": self.thread_id,
                    "turnId": "turn-1",
                    "item": {
                        "type": "commandExecution",
                        "id": item,
                        "command": "cargo test -p symphony-runtime --lib orchestrator",
                        "cwd": "/workspaces/LOAD/repo",
                        "aggregatedOutput": "running 42 tests\ntest orchestrator::tests::dispatches_in_priority_order ... ok\ntest orchestrator::tests::retries_with_backoff ... ok\n\ntest result: ok. 42 passed; 0 failed; 0 ignored; finished in 1.84s\n",
                        "exitCode": 0,
                        "durationMs": 1_840,
                        "status": "completed",
                    },
                },
            }),
            _ => json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": self.thread_id,
                    "turnId": "turn-1",
                    "itemId": item,
                    "delta": " the next few words of",
                },
            }),
        };
        self.notification(payload)
    }
}

/// The memory tracker with a fixed delay before every answer (0 = none) and a call counter.
#[derive(Debug)]
pub struct SlowTracker {
    inner: MemoryTracker,
    latency: Duration,
    pub calls: AtomicU64,
}

impl SlowTracker {
    pub fn new(issues: MemoryIssues, latency: Duration) -> Self {
        Self {
            inner: MemoryTracker::new(issues),
            latency,
            calls: AtomicU64::new(0),
        }
    }

    async fn wait(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
    }
}

#[async_trait]
impl Tracker for SlowTracker {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    async fn fetch_issues_by_states(
        &self,
        settings: &TrackerSettings,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        self.wait().await;
        self.inner.fetch_issues_by_states(settings, states).await
    }

    async fn fetch_issues_by_ids(
        &self,
        settings: &TrackerSettings,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        self.wait().await;
        self.inner.fetch_issues_by_ids(settings, ids).await
    }

    fn secret_environment_names(&self, settings: &TrackerSettings) -> Vec<String> {
        self.inner.secret_environment_names(settings)
    }
}
