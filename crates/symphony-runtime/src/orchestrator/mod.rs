//! The orchestrator actor (`SymphonyElixir.Orchestrator`, B.2–B.9).
//!
//! One task owns all scheduling state and processes, one at a time: handle commands (snapshot,
//! refresh), timer events (tick, poll cycle, retry — each carrying a token so superseded timers are
//! ignored), worker runtime info, Codex events (one stream per run, keyed by run id so a stale run's
//! events can never land on a newer run), and worker completions (`JoinSet`, the monitor `DOWN`
//! analogue). Tracker calls are awaited inline, like the GenServer, so state is never shared.
//!
//! Time: durations (backoff, poll countdown, stall detection, runtime seconds) use tokio's monotonic
//! clock — pausable in tests — while displayed timestamps use `Utc::now()`.

// Modified 2026-10-25: spend controls and the kill switch (audit A4).

mod dispatch;
mod reconcile;
mod retry;
mod spend;
mod state;
#[cfg(test)]
mod tests;
mod workers;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use futures::StreamExt;
use symphony_codex::{CodexEvent, EventSink};
use symphony_core::{ConfigError, Issue, ProcessEnv, Settings, TrackerConfigError};
use symphony_store::{NewRun, RetryRecord, RunStatus, TaskRecord, TaskStatus};
use symphony_trackers::TrackerError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinError;
use tokio::time::Instant;
use tokio_stream::StreamMap;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

pub use spend::PAUSED_CONTROL_KEY;
pub(crate) use spend::{Pricing, SpendStop, Usage};
pub(crate) use state::{
    BlockedEntry, Counter, RetryEntry, RetryMeta, RunningEntry, State, StateSets, TaskRun,
    WorkerHandle, blocker_error, candidate_issue, next_retry_attempt_from_running, task_id,
    task_issue_id,
};
pub use state::{
    DelayType, FAILURE_RETRY_BASE_MS, HostChoice, POLL_TRANSITION_RENDER_DELAY, retry_delay,
    sort_issues_for_dispatch,
};

use crate::handle::RuntimeCommand;
use crate::recorder::RunRecorder;
use crate::redact::Redactor;
use crate::runner::hook_env_policy;
use crate::runtime::{RuntimeContext, WorkerSet};
use crate::snapshot::{
    BlockedSnapshot, PauseAck, PollingStatus, RefreshAck, RetrySnapshot, RunningSnapshot, Snapshot,
    SpendSnapshot, TrackerInfo,
};
use crate::tracker::FetchError;
use crate::worker::{RunError, TaskSpec, WorkerContext, WorkerMessage, WorkerResult};
use crate::workspace::{WorkspaceManager, worker_hosts};

/// Writes kill-switch changes to the store from one task, in order, retrying while the store is
/// busy, so an acknowledged pause also holds after a restart. A newer change replaces a pending
/// older one, so an old retry can never overwrite the latest state.
/// `in_flight` counts changes not yet written (another replica's change is adopted only when it is
/// zero, so a pending local change is never undone).
fn spawn_pause_writer(
    store: symphony_store::Store,
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
) -> mpsc::UnboundedSender<Option<String>> {
    use std::sync::atomic::Ordering;
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<String>>();
    tokio::spawn(async move {
        while let Some(mut reason) = rx.recv().await {
            let mut delay = Duration::from_millis(50);
            let mut taken = 1;
            loop {
                while let Ok(newer) = rx.try_recv() {
                    reason = newer;
                    taken += 1;
                }
                match store.set_control(PAUSED_CONTROL_KEY, reason.clone()).await {
                    Ok(()) => {
                        in_flight.fetch_sub(taken, Ordering::SeqCst);
                        break;
                    }
                    Err(symphony_store::StoreError::Busy) => {
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                    Err(err) => {
                        tracing::error!("Failed to persist the kill switch: {err}");
                        in_flight.fetch_sub(taken, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
    });
    tx
}

/// How long a dispatch lease lives without renewal (a dead replica's work is recovered after it).
pub const LEASE_TTL: Duration = Duration::from_secs(60);
/// How often held leases are renewed (and, with a shared store, dead replicas' work recovered).
pub const LEASE_RENEW_EVERY: Duration = Duration::from_secs(15);
/// A run whose lease could expire within this window (the renewal after next might come too
/// late) is stopped while the lease is still ours, so another replica never runs it at the same
/// time: renewals may fail once, not twice in a row.
pub const LEASE_STOP_WINDOW: Duration = Duration::from_secs(2 * LEASE_RENEW_EVERY.as_secs());
/// Bound on waiting for one renewal: a renewal stuck on an unresponsive database counts as
/// failed, so the expiry check still stops the runs whose lease may lapse.
pub const LEASE_RENEW_TIMEOUT: Duration = Duration::from_secs(LEASE_RENEW_EVERY.as_secs() / 2);

/// Extra wait, beyond the cancel grace, before a cancelled worker is aborted.
const CANCEL_MARGIN: Duration = Duration::from_millis(500);
/// Bound on waiting for an aborted worker to be dropped.
const ABORT_WAIT: Duration = Duration::from_secs(5);
/// How often the lease watchdog checks deadlines — short relative to its backstop window (see
/// [`lease_watchdog_backstop`]) so a run is cancelled in time even while the actor loop is parked.
const LEASE_WATCHDOG_INTERVAL: Duration = Duration::from_secs(1);

/// A running issue's dispatch-lease deadline and the token that cancels its worker. Shared between
/// the actor (which registers, renews and removes entries) and [`run_lease_watchdog`].
pub(crate) struct LeaseWatch {
    pub deadline: Instant,
    pub cancel: CancellationToken,
    /// Set by the watchdog when it cancels this run, so the actor treats the worker's `Cancelled`
    /// exit as an abandon (requeue, keep the sealed workspace, drop the lease) rather than a
    /// failure. Read under the map lock in `handle_worker_joined`; cleared with the entry.
    pub fenced: bool,
}

/// Issue id → [`LeaseWatch`]. A plain `std::sync::Mutex`: the critical sections are tiny and hold
/// no `.await`.
pub(crate) type LeaseWatchMap = Arc<Mutex<HashMap<String, LeaseWatch>>>;

/// The watchdog backstop: a run is cancelled once its lease would expire within this margin. It
/// must be (a) long enough to stop a worker before the lease lapses — `worker_cancel_grace` plus
/// the cancel/abort margins plus a tick — and (b) strictly shorter than `LEASE_STOP_WINDOW`, so
/// the watchdog fires *after* the actor's own `expiring_leases` fence would have. That ordering is
/// what keeps the watchdog from undercutting the actor's "tolerate one missed renewal" behaviour:
/// one timed-out renewal or a brief tracker/reconcile stall must not trip it, only a genuinely
/// parked actor that has stopped renewing for far longer.
fn lease_watchdog_backstop(worker_cancel_grace: Duration) -> Duration {
    // The watchdog only cancels the worker; the actor's `stop_worker` does the abort, and it can't
    // run while the actor is parked, so `ABORT_WAIT` doesn't apply here — the run exits
    // cooperatively within `worker_cancel_grace`. Fire that long plus a small margin before expiry.
    // Clamp strictly below `LEASE_STOP_WINDOW` so a large configured grace can't push the backstop
    // up to (or past) the actor's own fence — which would undercut its one-missed-renewal tolerance
    // or, past `LEASE_TTL`, fence every run on the first tick.
    let raw = worker_cancel_grace + CANCEL_MARGIN + 2 * LEASE_WATCHDOG_INTERVAL;
    raw.clamp(
        2 * LEASE_WATCHDOG_INTERVAL,
        LEASE_STOP_WINDOW - LEASE_WATCHDOG_INTERVAL,
    )
}

/// Cancels, independently of the actor loop, the worker of any run whose dispatch lease is within
/// `backstop` of expiry. The actor renews held leases every `LEASE_RENEW_EVERY`, pushing each
/// deadline out, so this fires only once the actor loop has been parked long enough that renewals
/// stopped and its own `expiring_leases` fence never ran. The cancelled worker exits through the
/// normal path and the actor abandons it (see the `fenced` flag); the lease then lapses by TTL,
/// and another replica takes the issue only after this run has stopped, so two replicas never run
/// one issue at once.
async fn run_lease_watchdog(map: LeaseWatchMap, shutdown: CancellationToken, backstop: Duration) {
    let mut ticker = tokio::time::interval(LEASE_WATCHDOG_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = ticker.tick() => fire_expiring_lease_cancels(&map, backstop),
        }
    }
}

/// Marks and cancels the worker of every watched run within `backstop` of its deadline. Idempotent:
/// an already-cancelled run is skipped, and entries are removed by `drop_lease` on the normal exit.
fn fire_expiring_lease_cancels(map: &LeaseWatchMap, backstop: Duration) {
    let now = Instant::now();
    let mut watches = map.lock().unwrap_or_else(|poison| poison.into_inner());
    for (issue_id, watch) in watches.iter_mut() {
        if watch.deadline <= now + backstop && !watch.cancel.is_cancelled() {
            tracing::warn!(
                "Stopping issue_id={issue_id}: its dispatch lease is within the backstop window and \
                 renewals have stalled; cancelling its run independently of the actor loop"
            );
            // Mark before cancelling (same lock): the worker's `Cancelled` exit can only become
            // visible after the cancel, so the actor always sees `fenced` first.
            watch.fenced = true;
            watch.cancel.cancel();
        }
    }
}

/// Timer events (each tick/retry carries its token).
#[derive(Debug)]
pub(crate) enum TimerEvent {
    Tick { token: u64 },
    RunPollCycle,
    RetryDue { issue_id: String, token: u64 },
    LeaseRenew,
}

/// `refresh_issue_for_dispatch/1` outcome.
#[derive(Debug)]
pub(crate) enum Revalidation {
    Ok(Issue),
    SkipMissing,
    Skip(Issue),
    Error(FetchError),
}

fn issue_context(issue: &Issue) -> String {
    format!(
        "issue_id={} issue_identifier={}",
        issue.id.as_deref().unwrap_or(""),
        issue.identifier.as_deref().unwrap_or("")
    )
}

/// The orchestrator actor. Borrows the [`WorkerSet`] from the supervisor so that, if this future
/// panics, the supervisor can still abort and await every worker.
pub(crate) struct Orchestrator<'w> {
    pub(crate) ctx: Arc<RuntimeContext>,
    pub(crate) state: State,
    pub(crate) workers: &'w mut WorkerSet,
    cmd_rx: mpsc::Receiver<RuntimeCommand>,
    timer_tx: mpsc::UnboundedSender<TimerEvent>,
    timer_rx: mpsc::UnboundedReceiver<TimerEvent>,
    worker_tx: mpsc::UnboundedSender<WorkerMessage>,
    worker_rx: mpsc::UnboundedReceiver<WorkerMessage>,
    codex_streams: StreamMap<u64, UnboundedReceiverStream<CodexEvent>>,
    shutdown: CancellationToken,
    tokens: Counter,
    /// Task status writes, applied in order (and retried while the store is busy).
    task_writes: mpsc::UnboundedSender<TaskWrite>,
    /// Kill-switch changes, persisted in order by [`spawn_pause_writer`].
    pause_writes: mpsc::UnboundedSender<Option<String>>,
    /// Kill-switch changes sent but not written yet.
    pause_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// Shared with [`run_lease_watchdog`]: each running issue's lease deadline and worker-cancel
    /// token, so a run is stopped near expiry even when the actor loop is parked.
    lease_watch: LeaseWatchMap,
}

/// A task status change for the store.
#[derive(Debug)]
pub(crate) enum TaskWrite {
    Finish {
        id: i64,
        status: TaskStatus,
        result: Option<String>,
        error: Option<String>,
    },
    Release {
        id: i64,
        reason: String,
    },
    /// A task's dispatch lease, released after its status writes (a replica recovering tasks
    /// must not see it `running` without a lease).
    DropLease {
        key: String,
        owner: String,
    },
}

/// Applies task writes one after another, so a finish always lands before a later release of the
/// same task, and retries a write the store refused as busy instead of dropping it (a dropped
/// finish would leave the task `running` forever).
fn spawn_task_writer(store: symphony_store::Store) -> mpsc::UnboundedSender<TaskWrite> {
    let (tx, mut rx) = mpsc::unbounded_channel::<TaskWrite>();
    tokio::spawn(async move {
        while let Some(write) = rx.recv().await {
            let mut delay = Duration::from_millis(20);
            for _ in 0..12 {
                let result = match &write {
                    TaskWrite::Finish {
                        id,
                        status,
                        result,
                        error,
                    } => store
                        .finish_task(*id, *status, result.clone(), error.clone())
                        .await
                        .map(drop),
                    TaskWrite::Release { id, reason } => {
                        store.release_task(*id, reason.clone()).await.map(drop)
                    }
                    TaskWrite::DropLease { key, owner } => {
                        store.release_lease(key.clone(), owner.clone()).await
                    }
                };
                match result {
                    Err(symphony_store::StoreError::Busy) => {
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                    Err(err) => {
                        tracing::error!("Failed to record a task status change: {err}");
                        break;
                    }
                    Ok(()) => break,
                }
            }
        }
    });
    tx
}

impl<'w> Orchestrator<'w> {
    pub(crate) fn new(
        ctx: Arc<RuntimeContext>,
        cmd_rx: mpsc::Receiver<RuntimeCommand>,
        workers: &'w mut WorkerSet,
        shutdown: CancellationToken,
    ) -> Self {
        let settings = ctx.workflow.settings();
        let ctx_store = ctx.store.clone();
        let (timer_tx, timer_rx) = mpsc::unbounded_channel();
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        let pause_in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pause_writes = spawn_pause_writer(ctx.store.clone(), Arc::clone(&pause_in_flight));
        Self {
            state: State::new(&settings),
            ctx,
            workers,
            cmd_rx,
            timer_tx,
            timer_rx,
            worker_tx,
            worker_rx,
            codex_streams: StreamMap::new(),
            shutdown,
            tokens: Counter::default(),
            task_writes: spawn_task_writer(ctx_store),
            pause_writes,
            pause_in_flight,
            lease_watch: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn settings(&self) -> Arc<Settings> {
        self.ctx.workflow.settings()
    }

    /// Records the end of a task's run (`status` final or `blocked`).
    pub(crate) fn task_finish(
        &self,
        id: i64,
        status: TaskStatus,
        result: Option<String>,
        error: Option<String>,
    ) {
        let _ = self.task_writes.send(TaskWrite::Finish {
            id,
            status,
            result,
            error,
        });
    }

    /// Puts a running task back to `queued` (stopped from outside).
    pub(crate) fn task_release(&self, id: i64, reason: &str) {
        let _ = self.task_writes.send(TaskWrite::Release {
            id,
            reason: reason.to_owned(),
        });
    }

    /// `<workspace.root>/.tasks/<id>`, the workspace of task `id` (see `workspace::TASK_WORKSPACES`).
    pub(crate) fn task_workspace(&self, id: i64) -> std::path::PathBuf {
        self.task_workspaces().join(id.to_string())
    }

    fn task_workspaces(&self) -> std::path::PathBuf {
        self.settings()
            .local_workspace_root(&self.ctx.workflow.workflow_file_path())
            .join(crate::workspace::TASK_WORKSPACES)
    }

    /// Removes task `id`'s workspace in the background (its agent is already stopped). Task
    /// workspaces run no workflow hooks.
    pub(crate) fn remove_task_workspace(&self, id: i64) {
        let path = self.task_workspace(id);
        tokio::spawn(async move {
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => tracing::debug!("Removed task workspace path={}", path.display()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => tracing::warn!(
                    "Failed to remove task workspace path={}: {err}",
                    path.display()
                ),
            }
        });
    }

    /// Startup: removes the workspaces of tasks that are final or unknown (a previous process
    /// stopped before cleaning them up).
    pub(crate) async fn sweep_task_workspaces(&self) {
        if !self.ctx.store.is_enabled() {
            return;
        }
        let Ok(mut entries) = tokio::fs::read_dir(self.task_workspaces()).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<i64>().ok())
            else {
                continue;
            };
            // Seal first, before asking the store what to do: a leftover uid-owned workspace must
            // be handed to root even if `get_task` errors (which would otherwise leave it
            // reachable by a re-leased uid). Removal of final/unknown tasks follows.
            self.seal_kept_task_workspace(id).await;
            match self.ctx.store.get_task(id).await {
                Ok(Some(task)) if !task.status.is_final() => {}
                Ok(_) => self.remove_task_workspace(id),
                Err(err) => tracing::warn!("Failed to read task {id}: {err}"),
            }
        }
    }

    /// Startup: a non-final task's workspace survived a crash still owned by the uid it last ran
    /// as. Seal it to root (only under `task_uids`) so a reused uid cannot tamper with it before
    /// the task resumes; if it cannot be sealed, remove it so the task re-clones rather than
    /// resuming beside a uid-owned tree.
    async fn seal_kept_task_workspace(&self, id: i64) {
        #[cfg(unix)]
        if self
            .settings()
            .isolation
            .as_ref()
            .and_then(|iso| iso.task_uids)
            .is_some()
        {
            let path = self.task_workspace(id);
            if let Err(err) = crate::isolation::seal_workspace(&path) {
                tracing::warn!(
                    "Removing task workspace {} (could not seal it on startup): {err}",
                    path.display()
                );
                self.remove_task_workspace(id);
            }
        }
        #[cfg(not(unix))]
        let _ = id;
    }

    pub(crate) fn workspace_manager(&self, settings: &Arc<Settings>) -> WorkspaceManager {
        WorkspaceManager::new(
            Arc::clone(settings),
            self.ctx.workflow.workflow_file_path(),
            self.ctx.ssh.clone(),
        )
        .with_hook_env(hook_env_policy(settings, self.ctx.strip_hook_secrets))
        .with_remote_cleanup_concurrency(self.ctx.startup_cleanup_concurrency)
    }

    /// `notify_dashboard/0`: bumps the generation counter.
    pub(crate) fn notify(&self) {
        self.ctx.generation.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// Runs until shutdown: startup cleanup, first tick, then the event loop; finally stops workers.
    pub(crate) async fn run(mut self) {
        // Fires worker-cancel tokens near lease expiry on its own schedule, so a run cannot
        // outlive its lease even if the actor loop below is parked on a slow dispatch-path store
        // call (which would otherwise stop the `LeaseRenew` timer from being processed).
        let watchdog = tokio::spawn(run_lease_watchdog(
            Arc::clone(&self.lease_watch),
            self.shutdown.clone(),
            lease_watchdog_backstop(self.ctx.worker_cancel_grace),
        ));
        self.startup().await;
        while self.step().await {}
        self.stop_all_workers().await;
        watchdog.abort();
    }

    /// `init/1`: startup terminal cleanup, then an immediate tick.
    pub(crate) async fn startup(&mut self) {
        // Before anything is dispatched: agents of a hard-killed predecessor must not run next to
        // the ones this process starts for the same issues.
        let root = self
            .settings()
            .local_workspace_root(&self.ctx.workflow.workflow_file_path());
        crate::agents::reap_stale(&crate::agents::registry_dir(&root)).await;
        self.restore_spend().await;
        // A restarted replica with a stable id owns no work any more.
        if let Err(err) = self
            .ctx
            .store
            .release_owner_leases(self.ctx.replica_id.clone())
            .await
        {
            tracing::warn!("Failed to drop the leases of the previous process: {err}");
        }
        match self.ctx.store.requeue_interrupted_tasks().await {
            Ok(0) => {}
            Ok(count) => tracing::info!("Queued {count} tasks interrupted by the previous process"),
            Err(err) => tracing::warn!("Failed to re-queue interrupted tasks: {err}"),
        }
        self.spawn_timer(LEASE_RENEW_EVERY, TimerEvent::LeaseRenew);
        self.sweep_task_workspaces().await;
        self.restore_retry_queue().await;
        self.run_terminal_workspace_cleanup().await;
        self.schedule_tick(Duration::ZERO);
    }

    /// Processes one event. Returns `false` when the orchestrator should stop.
    pub(crate) async fn step(&mut self) -> bool {
        tokio::select! {
            biased;
            () = self.shutdown.cancelled() => false,
            Some(command) = self.cmd_rx.recv() => {
                self.handle_command(command).await;
                self.enforce_spend().await;
                true
            }
            Some(timer) = self.timer_rx.recv() => {
                self.handle_timer(timer).await;
                true
            }
            Some(message) = self.worker_rx.recv() => {
                self.handle_worker_message(message);
                true
            }
            Some((run_id, event)) = self.codex_streams.next(), if !self.codex_streams.is_empty() => {
                self.handle_codex_event(run_id, event);
                self.enforce_spend().await;
                true
            }
            Some(joined) = self.workers.set.join_next_with_id(), if !self.workers.set.is_empty() => {
                self.handle_worker_joined(joined);
                true
            }
        }
    }

    pub(crate) async fn handle_command(&mut self, command: RuntimeCommand) {
        match command {
            RuntimeCommand::Snapshot(reply) => {
                let snapshot = self.snapshot();
                let _ = reply.send(snapshot);
            }
            RuntimeCommand::RequestRefresh(reply) => {
                let ack = self.request_refresh();
                let _ = reply.send(ack);
            }
            RuntimeCommand::CancelTask { id, reply } => {
                let issue_id = task_issue_id(id);
                let running = self.state.running.contains_key(&issue_id);
                if running {
                    tracing::info!("Cancelling task run issue_id={issue_id}");
                    self.task_finish(
                        id,
                        TaskStatus::Cancelled,
                        None,
                        Some("cancelled by client".into()),
                    );
                    self.terminate_running_issue(&issue_id, false, "cancelled by client")
                        .await;
                } else {
                    self.release_issue_claim(&issue_id);
                }
                // A cancelled task never runs again (a blocked one leaves its workspace here).
                self.remove_task_workspace(id);
                self.notify();
                let _ = reply.send(running);
            }
            RuntimeCommand::UnblockTask { id, reply } => {
                self.release_issue_claim(&task_issue_id(id));
                self.schedule_tick(Duration::ZERO);
                self.notify();
                let _ = reply.send(());
            }
            RuntimeCommand::SetPaused {
                paused,
                reason,
                reply,
            } => {
                let ack = self.set_paused(paused, reason);
                let _ = reply.send(ack);
            }
            #[cfg(test)]
            RuntimeCommand::Crash => panic!("orchestrator crash requested by test"),
        }
    }

    pub(crate) async fn handle_timer(&mut self, timer: TimerEvent) {
        match timer {
            TimerEvent::Tick { token } => self.handle_tick(Some(token)),
            TimerEvent::RunPollCycle => self.run_poll_cycle().await,
            TimerEvent::RetryDue { issue_id, token } => {
                self.handle_retry_due(&issue_id, token).await
            }
            TimerEvent::LeaseRenew => self.renew_leases().await,
        }
    }

    // ----- dispatch leases -------------------------------------------------------------------

    /// Takes the dispatch lease of `issue_id` (kept while the issue is claimed). `false` when
    /// another live replica holds it or the store cannot be reached (dispatch then waits).
    pub(crate) async fn take_lease(&mut self, issue_id: &str) -> bool {
        if self.state.leases.contains_key(issue_id) {
            return true;
        }
        // The deadline counts from before the request: the stored expiry is no earlier.
        let asked = Instant::now();
        match self
            .ctx
            .store
            .acquire_lease(issue_id, self.ctx.replica_id.clone(), LEASE_TTL)
            .await
        {
            Ok(true) => {
                self.state
                    .leases
                    .insert(issue_id.to_owned(), asked + LEASE_TTL);
                true
            }
            Ok(false) => {
                tracing::debug!("Not dispatching issue_id={issue_id}: another replica holds it");
                false
            }
            Err(err) => {
                tracing::warn!("Not dispatching issue_id={issue_id}: lease failed: {err}");
                false
            }
        }
    }

    /// Registers `issue_id`'s run with the lease watchdog (shared stores only, once the lease
    /// exists): the watchdog cancels its worker near expiry even if the actor loop is parked.
    fn watch_lease(&self, issue_id: &str, cancel: &CancellationToken) {
        if !self.ctx.store.is_shared() {
            return;
        }
        let Some(&deadline) = self.state.leases.get(issue_id) else {
            return;
        };
        self.lease_watch
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                issue_id.to_owned(),
                LeaseWatch {
                    deadline,
                    cancel: cancel.clone(),
                    fenced: false,
                },
            );
    }

    /// Whether the watchdog fenced (cancelled) this run's lease: its worker exit is an abandon, not
    /// a failure.
    fn lease_fenced(&self, issue_id: &str) -> bool {
        self.lease_watch
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(issue_id)
            .is_some_and(|watch| watch.fenced)
    }

    /// Pushes a renewed lease's deadline out in the watchdog map, matching `state.leases`.
    fn renew_lease_watch(&self, issue_id: &str, deadline: Instant) {
        if let Some(watch) = self
            .lease_watch
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(issue_id)
        {
            watch.deadline = deadline;
        }
    }

    /// Gives up the lease of `issue_id` if this replica holds it.
    pub(crate) fn drop_lease(&mut self, issue_id: &str) {
        self.lease_watch
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(issue_id);
        if self.state.leases.remove(issue_id).is_none() {
            return;
        }
        if task_id(issue_id).is_some() {
            let _ = self.task_writes.send(TaskWrite::DropLease {
                key: issue_id.to_owned(),
                owner: self.ctx.replica_id.clone(),
            });
        } else {
            self.ctx
                .store
                .release_lease(issue_id, self.ctx.replica_id.clone())
                .detach();
        }
    }

    /// Renews every held lease and stops the runs whose lease was lost (or, with a shared store,
    /// could expire before a later renewal: another replica may then take it); with a shared
    /// store, also recovers the runs and tasks of replicas whose leases expired. Re-arms itself.
    pub(crate) async fn renew_leases(&mut self) {
        let keys: Vec<String> = self.state.leases.keys().cloned().collect();
        if !keys.is_empty() {
            let asked = Instant::now();
            let renewal = tokio::time::timeout(
                LEASE_RENEW_TIMEOUT,
                self.ctx
                    .store
                    .renew_leases(self.ctx.replica_id.clone(), keys.clone(), LEASE_TTL),
            )
            .await;
            match renewal {
                Ok(Ok(held)) => {
                    let renewed = asked + LEASE_TTL;
                    for key in &held {
                        if let Some(deadline) = self.state.leases.get_mut(key) {
                            *deadline = renewed;
                        } else {
                            continue;
                        }
                        self.renew_lease_watch(key, renewed);
                    }
                    for key in keys.iter().filter(|key| !held.contains(key)) {
                        tracing::warn!(
                            "Stopping issue_id={key}: its dispatch lease expired and another \
                             replica took it"
                        );
                        self.abandon_running_issue(key, "dispatch lease lost").await;
                    }
                }
                Ok(Err(err)) => tracing::warn!("Failed to renew dispatch leases: {err}"),
                Err(_) => tracing::warn!(
                    "Renewing dispatch leases timed out after {}ms",
                    LEASE_RENEW_TIMEOUT.as_millis()
                ),
            }
            if self.ctx.store.is_shared() {
                let expiring = expiring_leases(&self.state.leases, Instant::now());
                // Cancel every expiring run's worker up front (the watchdog may already have), so
                // they shut down concurrently rather than one waiting on the next's cancel grace.
                for key in &expiring {
                    tracing::warn!(
                        "Stopping issue_id={key}: its dispatch lease could not be renewed and \
                         may expire before the next renewal"
                    );
                    if let Some(cancel) = self
                        .state
                        .running
                        .get(key)
                        .and_then(|entry| entry.worker.as_ref())
                        .map(|worker| &worker.cancel)
                    {
                        cancel.cancel();
                    }
                }
                // Then abandon each (drops the lease and claim, awaits the already-cancelled stop).
                for key in &expiring {
                    self.abandon_running_issue(key, "dispatch lease could not be renewed")
                        .await;
                }
            }
        }
        // Re-arm the next renewal before the reconciliation calls below, so a store call that
        // stalls on an unresponsive database cannot delay the schedule — the next renewal (and its
        // `expiring_leases` check) fires on time. Should a reconciliation call park the loop past a
        // lease deadline anyway, the lease watchdog ([`run_lease_watchdog`]) still stops runs near
        // expiry on its own schedule.
        self.spawn_timer(LEASE_RENEW_EVERY, TimerEvent::LeaseRenew);
        if self.ctx.store.is_shared() {
            // `sync_kill_switch` and `recover_orphans` only read the shared control row and close
            // other replicas' rows, so a stalled database must not park them past the schedule —
            // bound each. `enforce_spend` is deliberately NOT bounded: it stops local runs, and
            // stopping a run removes the `running` entry and then releases the task, the dispatch
            // lease, and the claim. A timeout firing between those steps would drop the rest,
            // stranding the task `running` with its lease renewed forever. Run-stopping must run
            // to completion with no cancellation point; and should a slow `enforce_spend` delay
            // the next renewal anyway, the lease watchdog ([`run_lease_watchdog`]) still cancels
            // runs near expiry on its own schedule.
            let _ = tokio::time::timeout(LEASE_RENEW_TIMEOUT, self.sync_kill_switch()).await;
            self.enforce_spend().await;
            let _ = tokio::time::timeout(LEASE_RENEW_TIMEOUT, self.recover_orphans()).await;
        }
    }

    /// Adopts a kill-switch change another replica made (shared store), unless a local change is
    /// still being written.
    pub(crate) async fn sync_kill_switch(&mut self) {
        if self
            .pause_in_flight
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            return;
        }
        let stored = match self.ctx.store.control(PAUSED_CONTROL_KEY).await {
            Ok(stored) => stored,
            Err(err) => {
                tracing::warn!("Failed to read the kill switch: {err}");
                return;
            }
        };
        if stored == self.state.spend.paused {
            return;
        }
        match &stored {
            Some(reason) => tracing::warn!("Kill switch engaged by another replica: {reason}"),
            None => tracing::info!("Kill switch released by another replica; dispatch resumes"),
        }
        let resumed = stored.is_none();
        self.state.spend.paused = stored;
        if resumed {
            self.schedule_tick(Duration::ZERO);
        }
        self.notify();
    }

    /// Closes runs and re-queues tasks whose process is gone (no live lease).
    pub(crate) async fn recover_orphans(&mut self) {
        match self.ctx.store.mark_interrupted_runs().await {
            Ok(0) => {}
            Ok(count) => tracing::info!("Closed {count} runs of stopped replicas"),
            Err(err) => tracing::warn!("Failed to close interrupted runs: {err}"),
        }
        match self.ctx.store.requeue_interrupted_tasks().await {
            Ok(0) => {}
            Ok(count) => tracing::info!("Queued {count} tasks of stopped replicas again"),
            Err(err) => tracing::warn!("Failed to re-queue interrupted tasks: {err}"),
        }
    }

    // ----- timers --------------------------------------------------------------------------------

    pub(crate) fn spawn_timer(
        &self,
        delay: Duration,
        event: TimerEvent,
    ) -> tokio::task::AbortHandle {
        let tx = self.timer_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(event);
        })
        .abort_handle()
    }

    /// `schedule_tick/2`: cancels the previous tick and arms a new one with a fresh token.
    pub(crate) fn schedule_tick(&mut self, delay: Duration) {
        if let Some(timer) = self.state.tick_timer.take() {
            timer.abort();
        }
        let token = self.tokens.next();
        self.state.tick_timer = Some(self.spawn_timer(delay, TimerEvent::Tick { token }));
        self.state.tick_token = Some(token);
        self.state.next_poll_due = Some(Instant::now() + delay);
    }

    /// `handle_info({:tick, token})`; `None` is the bare `:tick` (forced, no token check).
    pub(crate) fn handle_tick(&mut self, token: Option<u64>) {
        if let Some(token) = token
            && self.state.tick_token != Some(token)
        {
            return;
        }
        let settings = self.settings();
        self.state.refresh_runtime_config(&settings);
        self.state.poll_check_in_progress = true;
        self.state.next_poll_due = None;
        self.state.tick_timer = None;
        self.state.tick_token = None;
        self.notify();
        // Not cancellable and without a token, like Elixir's `:run_poll_cycle` timer.
        let _ = self.spawn_timer(POLL_TRANSITION_RENDER_DELAY, TimerEvent::RunPollCycle);
    }

    /// `handle_info(:run_poll_cycle)`.
    pub(crate) async fn run_poll_cycle(&mut self) {
        let settings = self.settings();
        self.state.refresh_runtime_config(&settings);
        self.maybe_dispatch().await;
        let interval = Duration::from_millis(self.state.poll_interval_ms);
        self.schedule_tick(interval);
        self.state.poll_check_in_progress = false;
        self.notify();
    }

    /// `handle_call(:request_refresh)`.
    pub(crate) fn request_refresh(&mut self) -> RefreshAck {
        let now = Instant::now();
        let already_due = self.state.next_poll_due.is_some_and(|due| due <= now);
        let coalesced = self.state.poll_check_in_progress || already_due;
        if !coalesced {
            self.schedule_tick(Duration::ZERO);
        }
        RefreshAck {
            queued: true,
            coalesced,
            requested_at: Utc::now(),
            operations: vec!["poll".into(), "reconcile".into()],
        }
    }

    // ----- spend controls ------------------------------------------------------------------------

    /// Startup: the kill switch and today's usage come back from the store.
    pub(crate) async fn restore_spend(&mut self) {
        match self.ctx.store.control(PAUSED_CONTROL_KEY).await {
            Ok(Some(reason)) => {
                tracing::warn!("Dispatch is paused (kill switch engaged before restart): {reason}");
                self.state.spend.paused = Some(reason);
            }
            Ok(None) => {}
            Err(err) => tracing::warn!("Failed to read the kill switch: {err}"),
        }
        let now = Utc::now();
        let midnight = now
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc())
            .unwrap_or(now);
        let usage = match self.ctx.store.usage_since(midnight).await {
            Ok(usage) => Usage {
                tokens: usage.tokens.processed(),
                cost_micro_usd: usage.cost_micro_usd,
            },
            Err(err) => {
                tracing::warn!("Failed to load today's usage: {err}");
                Usage::default()
            }
        };
        self.state.spend.seed_day(now, usage);
    }

    /// Engages (`paused`) or releases the kill switch; persisted in the store. Running agents
    /// are stopped by the next [`Self::enforce_spend`].
    pub(crate) fn set_paused(&mut self, paused: bool, reason: Option<String>) -> PauseAck {
        let reason = paused.then(|| {
            reason
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "operator request".to_owned())
        });
        if reason != self.state.spend.paused {
            match &reason {
                Some(reason) => tracing::warn!("Kill switch engaged: {reason}"),
                None => tracing::info!("Kill switch released; dispatch resumes"),
            }
            self.pause_in_flight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = self.pause_writes.send(reason.clone());
            self.state.spend.paused = reason.clone();
            if reason.is_none() {
                self.schedule_tick(Duration::ZERO);
            }
            self.notify();
        }
        PauseAck {
            paused: reason.is_some(),
            reason,
        }
    }

    /// Stops what spending rules no longer allow: everything while paused or when today's
    /// budget is used up (claims released, so they run again later), and each run whose issue
    /// used up its budget (blocked).
    pub(crate) async fn enforce_spend(&mut self) {
        if self.state.running.is_empty() {
            return;
        }
        let settings = self.settings();
        let now = Utc::now();
        if let Some(stop) = self.state.spend.global_stop(&settings.budget, now) {
            let ids: Vec<String> = self.state.running.keys().cloned().collect();
            tracing::warn!("Stopping {} running agents: {}", ids.len(), stop.reason());
            for issue_id in ids {
                self.terminate_running_issue(&issue_id, false, stop.reason())
                    .await;
            }
            self.notify();
            return;
        }
        let over: Vec<(String, String)> = self
            .state
            .running
            .keys()
            .filter_map(|issue_id| {
                self.state
                    .spend
                    .issue_stop(issue_id, &settings.budget)
                    .map(|stop| (issue_id.clone(), stop.reason().to_owned()))
            })
            .collect();
        for (issue_id, reason) in over {
            self.stop_and_block(&issue_id, reason).await;
        }
        let over_task_limit: Vec<(String, i64, String)> = self
            .state
            .running
            .iter()
            .filter_map(|(issue_id, entry)| {
                let task = entry.task.as_ref()?;
                let limit = task.max_tokens?;
                let used = self.state.spend.issue(issue_id).tokens;
                (used >= limit).then(|| {
                    (
                        issue_id.clone(),
                        task.id,
                        format!("task token limit exhausted: {used} of {limit} tokens used"),
                    )
                })
            })
            .collect();
        for (issue_id, task, reason) in over_task_limit {
            tracing::warn!("Stopping task run issue_id={issue_id}: {reason}");
            self.task_finish(task, TaskStatus::Failed, None, Some(reason.clone()));
            self.terminate_running_issue(&issue_id, false, &reason)
                .await;
        }
    }

    /// Stops a running agent and blocks its issue with `error`.
    pub(crate) async fn stop_and_block(&mut self, issue_id: &str, error: String) {
        let Some(mut entry) = self.state.running.remove(issue_id) else {
            return;
        };
        tracing::warn!(
            "Stopping and blocking issue_id={issue_id} issue_identifier={}: {error}",
            entry.identifier
        );
        self.record_session_completion_totals(&entry);
        self.codex_streams.remove(&entry.run_id);
        self.stop_worker(entry.worker.take()).await;
        entry
            .recorder
            .finish(RunStatus::Blocked, Some(error.clone()));
        self.block_issue_from_entry(issue_id, entry, error);
        self.notify();
    }

    fn spend_snapshot(&mut self, settings: &Settings, now: chrono::DateTime<Utc>) -> SpendSnapshot {
        let today = self.state.spend.today(now);
        SpendSnapshot {
            paused: self.state.spend.paused.is_some(),
            stopped_reason: self
                .state
                .spend
                .global_stop(&settings.budget, now)
                .map(|stop| stop.reason().to_owned()),
            day: now.date_naive().to_string(),
            day_tokens: today.tokens,
            day_cost_micro_usd: today.cost_micro_usd,
            max_tokens_per_day: settings.budget.max_tokens_per_day,
            max_cost_usd_per_day: settings.budget.max_cost_usd_per_day,
            max_tokens_per_issue: settings.budget.max_tokens_per_issue,
            max_cost_usd_per_issue: settings.budget.max_cost_usd_per_issue,
        }
    }

    // ----- snapshot ------------------------------------------------------------------------------

    /// `handle_call(:snapshot)` (refreshes and keeps the runtime config first).
    pub(crate) fn snapshot(&mut self) -> Snapshot {
        let settings = self.settings();
        self.state.refresh_runtime_config(&settings);
        let now = Instant::now();
        let now_utc = Utc::now();
        let mut running: Vec<RunningSnapshot> = self
            .state
            .running
            .iter()
            .map(|(issue_id, entry)| RunningSnapshot {
                issue_id: issue_id.clone(),
                identifier: entry.identifier.clone(),
                issue_url: entry.issue.url.clone(),
                state: entry.issue.state.clone(),
                worker_host: entry.worker_host.clone(),
                workspace_path: entry.workspace_path.clone(),
                session_id: entry.session_id.clone(),
                codex_app_server_pid: entry.codex_app_server_pid.clone(),
                codex_input_tokens: entry.tokens.totals.input_tokens,
                codex_cached_input_tokens: entry.tokens.totals.cached_input_tokens,
                codex_output_tokens: entry.tokens.totals.output_tokens,
                codex_total_tokens: entry.tokens.totals.total_tokens,
                turn_count: entry.turn_count,
                retry_attempt: entry.retry_attempt,
                started_at: entry.started_at,
                last_codex_timestamp: entry.last_codex_timestamp,
                last_codex_message: entry.last_codex_message.clone(),
                last_codex_event: entry.last_codex_event,
                runtime_seconds: now.duration_since(entry.started).as_secs(),
            })
            .collect();
        running.sort_by(|a, b| {
            a.identifier
                .cmp(&b.identifier)
                .then(a.issue_id.cmp(&b.issue_id))
        });

        let mut retrying: Vec<RetrySnapshot> = self
            .state
            .retry_attempts
            .iter()
            .map(|(issue_id, entry)| {
                let due_in = entry.due_at.saturating_duration_since(now);
                RetrySnapshot {
                    issue_id: issue_id.clone(),
                    identifier: entry.identifier.clone(),
                    attempt: entry.attempt,
                    due_in_ms: u64::try_from(due_in.as_millis()).unwrap_or(u64::MAX),
                    due_at: now_utc
                        + chrono::Duration::from_std(due_in).unwrap_or(chrono::Duration::zero()),
                    issue_url: entry.issue_url.clone(),
                    error: entry.error.clone(),
                    worker_host: entry.worker_host.clone(),
                    workspace_path: entry.workspace_path.clone(),
                }
            })
            .collect();
        retrying.sort_by(|a, b| {
            a.due_in_ms
                .cmp(&b.due_in_ms)
                .then(a.issue_id.cmp(&b.issue_id))
        });

        let mut blocked: Vec<BlockedSnapshot> = self
            .state
            .blocked
            .iter()
            .map(|(issue_id, entry)| BlockedSnapshot {
                issue_id: issue_id.clone(),
                identifier: entry.identifier.clone(),
                issue_url: entry.issue.as_ref().and_then(|i| i.url.clone()),
                state: entry.issue.as_ref().and_then(|i| i.state.clone()),
                worker_host: entry.worker_host.clone(),
                workspace_path: entry.workspace_path.clone(),
                session_id: entry.session_id.clone(),
                error: entry.error.clone(),
                blocked_at: entry.blocked_at,
                last_codex_timestamp: entry.last_codex_timestamp,
                last_codex_message: entry.last_codex_message.clone(),
                last_codex_event: entry.last_codex_event,
            })
            .collect();
        blocked.sort_by(|a, b| {
            a.identifier
                .cmp(&b.identifier)
                .then(a.issue_id.cmp(&b.issue_id))
        });

        let spend = self.spend_snapshot(&settings, now_utc);
        Snapshot {
            generated_at: now_utc,
            generation: *self.ctx.generation.borrow(),
            running,
            retrying,
            blocked,
            codex_totals: self.state.codex_totals,
            rate_limits: self.state.codex_rate_limits.clone(),
            polling: PollingStatus {
                checking: self.state.poll_check_in_progress,
                next_poll_in_ms: self.state.next_poll_due.map(|due| {
                    u64::try_from(due.saturating_duration_since(now).as_millis())
                        .unwrap_or(u64::MAX)
                }),
                poll_interval_ms: self.state.poll_interval_ms,
            },
            max_concurrent_agents: settings.agent.max_concurrent_agents,
            workspace_root: settings.workspace.root.clone(),
            tracker: TrackerInfo {
                kind: settings.tracker.kind.clone(),
                project_slug: settings.tracker.project_slug.clone(),
            },
            spend,
        }
    }
}

/// Held leases that may expire within [`LEASE_STOP_WINDOW`] of `now`.
pub(crate) fn expiring_leases(
    leases: &std::collections::HashMap<String, Instant>,
    now: Instant,
) -> Vec<String> {
    let mut keys: Vec<String> = leases
        .iter()
        .filter(|(_, deadline)| **deadline <= now + LEASE_STOP_WINDOW)
        .map(|(key, _)| key.clone())
        .collect();
    keys.sort();
    keys
}
