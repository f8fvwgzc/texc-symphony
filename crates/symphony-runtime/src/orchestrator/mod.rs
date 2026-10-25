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

use std::collections::HashSet;
use std::sync::Arc;
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
fn spawn_pause_writer(store: symphony_store::Store) -> mpsc::UnboundedSender<Option<String>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Option<String>>();
    tokio::spawn(async move {
        while let Some(mut reason) = rx.recv().await {
            let mut delay = Duration::from_millis(50);
            loop {
                while let Ok(newer) = rx.try_recv() {
                    reason = newer;
                }
                match store.set_control(PAUSED_CONTROL_KEY, reason.clone()).await {
                    Ok(()) => break,
                    Err(symphony_store::StoreError::Busy) => {
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(5));
                    }
                    Err(err) => {
                        tracing::error!("Failed to persist the kill switch: {err}");
                        break;
                    }
                }
            }
        }
    });
    tx
}

/// Extra wait, beyond the cancel grace, before a cancelled worker is aborted.
const CANCEL_MARGIN: Duration = Duration::from_millis(500);
/// Bound on waiting for an aborted worker to be dropped.
const ABORT_WAIT: Duration = Duration::from_secs(5);

/// Timer events (each tick/retry carries its token).
#[derive(Debug)]
pub(crate) enum TimerEvent {
    Tick { token: u64 },
    RunPollCycle,
    RetryDue { issue_id: String, token: u64 },
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
        let pause_writes = spawn_pause_writer(ctx.store.clone());
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
        self.startup().await;
        while self.step().await {}
        self.stop_all_workers().await;
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
        match self.ctx.store.requeue_interrupted_tasks().await {
            Ok(0) => {}
            Ok(count) => tracing::info!("Queued {count} tasks interrupted by the previous process"),
            Err(err) => tracing::warn!("Failed to re-queue interrupted tasks: {err}"),
        }
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
                tokens: usage.tokens.input.saturating_add(usage.tokens.output),
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
