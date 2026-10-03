//! Runtime assembly and supervision (`AgentRuntimeSupervisor`, B.1 / §1.4).
//!
//! [`Runtime::start`] spawns one supervisor task that runs the orchestrator future and owns the
//! worker `JoinSet`. If the orchestrator panics, every worker is aborted **and awaited** (their Codex
//! and hook process groups are killed on drop) before a fresh orchestrator — with empty state, a new
//! startup cleanup and re-dispatch from the tracker — is started, so at most one worker per issue ever
//! exists (`:one_for_all`). More than [`RestartPolicy::max_restarts`] restarts within
//! [`RestartPolicy::window`] (3 in 5 s, OTP's default intensity) end the runtime with
//! [`RuntimeError::RestartBudgetExceeded`].

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use futures::FutureExt;
use symphony_core::WorkflowStore;
use symphony_store::Store;
use symphony_trackers::{Tracker, TrackerDeps};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::handle::{
    DEFAULT_REFRESH_TIMEOUT, DEFAULT_SNAPSHOT_TIMEOUT, HandleShared, RuntimeCommand, RuntimeHandle,
};
use crate::orchestrator::Orchestrator;
use crate::runner::{AgentRunner, CodexWorkerFactory, RunnerOptions};
use crate::ssh::SshConfig;
use crate::tracker::{DEFAULT_TRACKER_TIMEOUT, TrackerClient};
use crate::worker::{WorkerFactory, WorkerResult};

/// Commands buffered for the orchestrator before senders wait.
const COMMAND_BUFFER: usize = 64;

/// Default time a cancelled worker gets to wind down (stop Codex, run `after_run`) before it is
/// aborted.
pub const DEFAULT_WORKER_CANCEL_GRACE: Duration = Duration::from_secs(10);
/// Default number of concurrent workspace removals during startup terminal cleanup.
pub const DEFAULT_STARTUP_CLEANUP_CONCURRENCY: usize = 8;

/// Orchestrator restart budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Restarts tolerated within `window`.
    pub max_restarts: usize,
    /// Sliding window.
    pub window: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 3,
            window: Duration::from_secs(5),
        }
    }
}

/// Everything the runtime needs. Fields are public; [`RuntimeOptions::new`] fills the defaults.
#[derive(Clone)]
pub struct RuntimeOptions {
    /// Last-known-good workflow (settings re-read on every tick, cycle and snapshot).
    pub workflow: Arc<WorkflowStore>,
    /// Tracker access (adapter per `tracker.kind`, or a fixed override; read timeout).
    pub tracker: TrackerClient,
    /// Run history store (`Store::disabled()` to turn persistence off).
    pub store: Store,
    /// SSH invocation for remote workers.
    pub ssh: SshConfig,
    /// Custom worker factory; `None` runs [`AgentRunner`] (workspace + hooks + Codex).
    pub worker_factory: Option<Arc<dyn WorkerFactory>>,
    /// Options of the default runner.
    pub runner: RunnerOptions,
    /// [`RuntimeHandle::snapshot`] timeout.
    pub snapshot_timeout: Duration,
    /// [`RuntimeHandle::request_refresh`] timeout.
    pub refresh_timeout: Duration,
    /// Grace given to a cancelled worker before it is aborted.
    pub worker_cancel_grace: Duration,
    /// Concurrency of startup terminal workspace cleanup (local and SSH removals).
    pub startup_cleanup_concurrency: usize,
    /// Orchestrator restart budget.
    pub restart_policy: RestartPolicy,
}

impl std::fmt::Debug for RuntimeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeOptions")
            .field("tracker", &self.tracker)
            .field("ssh", &self.ssh)
            .field("custom_worker_factory", &self.worker_factory.is_some())
            .field("runner", &self.runner)
            .field("snapshot_timeout", &self.snapshot_timeout)
            .field("refresh_timeout", &self.refresh_timeout)
            .field("worker_cancel_grace", &self.worker_cancel_grace)
            .field(
                "startup_cleanup_concurrency",
                &self.startup_cleanup_concurrency,
            )
            .field("restart_policy", &self.restart_policy)
            .finish()
    }
}

impl RuntimeOptions {
    /// Defaults: adapters from `tracker_deps`, persistence disabled, ssh from the environment
    /// (`SYMPHONY_SSH_CONFIG`), Codex workers.
    pub fn new(workflow: Arc<WorkflowStore>, tracker_deps: TrackerDeps) -> Self {
        Self {
            workflow,
            tracker: TrackerClient::new(tracker_deps).with_timeout(DEFAULT_TRACKER_TIMEOUT),
            store: Store::disabled(),
            ssh: SshConfig::from_env(),
            worker_factory: None,
            runner: RunnerOptions::default(),
            snapshot_timeout: DEFAULT_SNAPSHOT_TIMEOUT,
            refresh_timeout: DEFAULT_REFRESH_TIMEOUT,
            worker_cancel_grace: DEFAULT_WORKER_CANCEL_GRACE,
            startup_cleanup_concurrency: DEFAULT_STARTUP_CLEANUP_CONCURRENCY,
            restart_policy: RestartPolicy::default(),
        }
    }

    /// Persists runs to `store`.
    pub fn with_store(mut self, store: Store) -> Self {
        self.store = store;
        self
    }

    /// Uses `ssh` for remote workers.
    pub fn with_ssh(mut self, ssh: SshConfig) -> Self {
        self.ssh = ssh;
        self
    }

    /// Uses a custom worker factory instead of the Codex runner.
    pub fn with_worker_factory(mut self, factory: Arc<dyn WorkerFactory>) -> Self {
        self.worker_factory = Some(factory);
        self
    }

    /// Always uses `tracker` (ignores `tracker.kind`).
    pub fn with_tracker(mut self, tracker: Arc<dyn Tracker>) -> Self {
        self.tracker = self.tracker.with_tracker(tracker);
        self
    }

    /// Sets the default runner's options.
    pub fn with_runner_options(mut self, runner: RunnerOptions) -> Self {
        self.runner = runner;
        self
    }
}

/// Shared, immutable runtime context (one per [`Runtime`], shared across orchestrator restarts).
pub(crate) struct RuntimeContext {
    pub workflow: Arc<WorkflowStore>,
    pub tracker: TrackerClient,
    pub store: Store,
    pub ssh: SshConfig,
    pub factory: Arc<dyn WorkerFactory>,
    pub strip_hook_secrets: bool,
    pub worker_cancel_grace: Duration,
    pub startup_cleanup_concurrency: usize,
    pub generation: watch::Sender<u64>,
}

impl RuntimeContext {
    pub(crate) fn from_options(options: &RuntimeOptions, generation: watch::Sender<u64>) -> Self {
        let factory = options.worker_factory.clone().unwrap_or_else(|| {
            let runner = AgentRunner::new(
                Arc::clone(&options.workflow),
                options.tracker.clone(),
                options.ssh.clone(),
            )
            .with_options(options.runner.clone());
            Arc::new(CodexWorkerFactory::new(runner))
        });
        Self {
            workflow: Arc::clone(&options.workflow),
            tracker: options.tracker.clone(),
            store: options.store.clone(),
            ssh: options.ssh.clone(),
            factory,
            strip_hook_secrets: options.runner.strip_hook_secrets,
            worker_cancel_grace: options.worker_cancel_grace,
            startup_cleanup_concurrency: options.startup_cleanup_concurrency.max(1),
            generation,
        }
    }
}

/// The agent tasks of one orchestrator incarnation.
#[derive(Default)]
pub(crate) struct WorkerSet {
    pub set: JoinSet<WorkerResult>,
    /// Task id → (issue id, run id).
    pub runs: HashMap<tokio::task::Id, (String, u64)>,
}

impl WorkerSet {
    /// Aborts every worker and waits until all of them have been dropped.
    pub(crate) async fn shutdown(&mut self) {
        self.set.shutdown().await;
        self.runs.clear();
    }
}

/// Why the runtime stopped abnormally.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The orchestrator crashed more often than the restart policy allows.
    #[error(
        "orchestrator restart budget exceeded: more than {max_restarts} restarts within {window_ms}ms"
    )]
    RestartBudgetExceeded {
        /// The policy's restart limit.
        max_restarts: usize,
        /// The policy's window.
        window_ms: u64,
    },
    /// The supervisor task itself failed.
    #[error("runtime_task_failed: {0}")]
    Task(String),
}

/// A running agent runtime: the supervisor task plus its [`RuntimeHandle`].
#[derive(Debug)]
pub struct Runtime {
    handle: RuntimeHandle,
    task: JoinHandle<Result<(), RuntimeError>>,
}

impl Runtime {
    /// Starts the supervisor (and the first orchestrator) on the current tokio runtime.
    ///
    /// Startup terminal-workspace cleanup runs before the first tick; snapshot requests made in the
    /// meantime wait (bounded by their timeout).
    pub fn start(options: RuntimeOptions) -> Self {
        let (generation_tx, generation_rx) = watch::channel(0u64);
        let shared = Arc::new(HandleShared {
            commands: ArcSwapOption::empty(),
            generation: generation_rx,
            shutdown: CancellationToken::new(),
            snapshot_timeout: options.snapshot_timeout,
            refresh_timeout: options.refresh_timeout,
        });
        let ctx = Arc::new(RuntimeContext::from_options(&options, generation_tx));
        // Install the first command channel before returning, so requests made right after `start`
        // queue up instead of reporting `Unavailable`.
        let (tx, rx) = mpsc::channel::<RuntimeCommand>(COMMAND_BUFFER);
        shared.commands.store(Some(Arc::new(tx)));
        let task = tokio::spawn(supervise(
            ctx,
            Arc::clone(&shared),
            options.restart_policy,
            rx,
        ));
        Self {
            handle: RuntimeHandle::from_shared(shared),
            task,
        }
    }

    /// A client handle (clone freely).
    pub fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }

    /// Waits until the runtime stops (after [`RuntimeHandle::shutdown`], or on a fatal error).
    pub async fn wait(self) -> Result<(), RuntimeError> {
        match self.task.await {
            Ok(result) => result,
            Err(err) => Err(RuntimeError::Task(err.to_string())),
        }
    }

    /// Requests shutdown and waits for it to complete.
    pub async fn shutdown(self) -> Result<(), RuntimeError> {
        self.handle.shutdown();
        self.wait().await
    }
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_owned())
}

async fn supervise(
    ctx: Arc<RuntimeContext>,
    shared: Arc<HandleShared>,
    policy: RestartPolicy,
    first_commands: mpsc::Receiver<RuntimeCommand>,
) -> Result<(), RuntimeError> {
    let mut restarts: VecDeque<Instant> = VecDeque::new();
    let mut next_commands = Some(first_commands);
    loop {
        let rx = match next_commands.take() {
            Some(rx) => rx,
            None => {
                let (tx, rx) = mpsc::channel::<RuntimeCommand>(COMMAND_BUFFER);
                shared.commands.store(Some(Arc::new(tx)));
                rx
            }
        };
        let mut workers = WorkerSet::default();
        let orchestrator =
            Orchestrator::new(Arc::clone(&ctx), rx, &mut workers, shared.shutdown.clone());
        let outcome = AssertUnwindSafe(orchestrator.run()).catch_unwind().await;
        shared.commands.store(None);
        // `:one_for_all`: no worker of this incarnation survives into the next one.
        workers.shutdown().await;
        match outcome {
            Ok(()) => return Ok(()),
            Err(payload) => {
                if shared.shutdown.is_cancelled() {
                    return Ok(());
                }
                tracing::error!(
                    "Orchestrator crashed: {}; restarting agent runtime",
                    panic_text(payload.as_ref())
                );
                let now = Instant::now();
                restarts.push_back(now);
                while restarts
                    .front()
                    .is_some_and(|at| now.duration_since(*at) > policy.window)
                {
                    restarts.pop_front();
                }
                if restarts.len() > policy.max_restarts {
                    tracing::error!("Agent runtime restart budget exceeded; giving up");
                    return Err(RuntimeError::RestartBudgetExceeded {
                        max_restarts: policy.max_restarts,
                        window_ms: u64::try_from(policy.window.as_millis()).unwrap_or(u64::MAX),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use symphony_trackers::{MemoryIssues, MemoryTracker};

    use super::*;
    use crate::test_support::{TestWorkflow, eventually, issue};
    use crate::worker::{FnWorkerFactory, RunError, WorkerContext};

    struct Live {
        live: Arc<AtomicUsize>,
        max: Arc<AtomicUsize>,
        started: Arc<AtomicUsize>,
    }

    struct Guard(Arc<AtomicUsize>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn counting_factory() -> (Arc<dyn WorkerFactory>, Live) {
        let live = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let (l, m, s) = (Arc::clone(&live), Arc::clone(&max), Arc::clone(&started));
        let factory = FnWorkerFactory(move |ctx: WorkerContext| {
            let (live, max, started) = (Arc::clone(&l), Arc::clone(&m), Arc::clone(&s));
            async move {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(now, Ordering::SeqCst);
                started.fetch_add(1, Ordering::SeqCst);
                let _guard = Guard(live);
                ctx.cancel.cancelled().await;
                Err(RunError::Cancelled)
            }
        });
        (Arc::new(factory), Live { live, max, started })
    }

    fn options(
        wf: &TestWorkflow,
        memory: &MemoryIssues,
        factory: Arc<dyn WorkerFactory>,
    ) -> RuntimeOptions {
        let mut options = RuntimeOptions::new(
            Arc::clone(&wf.store),
            symphony_trackers::TrackerDeps::new().unwrap(),
        )
        .with_tracker(Arc::new(MemoryTracker::new(memory.clone())))
        .with_worker_factory(factory);
        options.worker_cancel_grace = Duration::from_millis(200);
        options
    }

    async fn crash(handle: &RuntimeHandle) {
        // The channel is replaced on restart; retry until the current orchestrator takes the command.
        for _ in 0..200 {
            if handle.crash().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no orchestrator accepted the crash command");
    }

    #[tokio::test]
    async fn start_runs_the_orchestrator_and_answers_snapshots() {
        let wf = TestWorkflow::new(json!({}));
        let memory = MemoryIssues::new();
        let (factory, _) = counting_factory();
        let runtime = Runtime::start(options(&wf, &memory, factory));
        let handle = runtime.handle();
        let snapshot = handle.snapshot().await.unwrap();
        assert!(snapshot.running.is_empty());
        assert!(handle.is_available());
        let ack = handle.request_refresh().await.unwrap();
        assert!(ack.queued);
        runtime.shutdown().await.unwrap();
        assert!(!handle.is_available());
    }

    #[tokio::test]
    async fn restarting_the_orchestrator_does_not_overlap_redispatched_work() {
        let wf = TestWorkflow::new(json!({"polling": {"interval_ms": 10}}));
        let memory = MemoryIssues::new();
        memory.set(vec![issue("i-1", "MT-1", "In Progress")]);
        let (factory, live) = counting_factory();
        let runtime = Runtime::start(options(&wf, &memory, factory));
        let handle = runtime.handle();
        eventually(Duration::from_secs(5), || {
            live.started.load(Ordering::SeqCst) == 1
        })
        .await;
        let mut updates = handle.subscribe();
        crash(&handle).await;
        eventually(Duration::from_secs(5), || {
            live.started.load(Ordering::SeqCst) == 2
        })
        .await;
        assert_eq!(live.max.load(Ordering::SeqCst), 1, "workers overlapped");
        assert_eq!(live.live.load(Ordering::SeqCst), 1);
        // The new orchestrator answers and publishes updates.
        let snapshot = handle.snapshot().await.unwrap();
        assert_eq!(snapshot.running.len(), 1);
        assert!(updates.has_changed().unwrap_or(false) || updates.changed().await.is_ok());
        runtime.shutdown().await.unwrap();
        assert_eq!(live.live.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn restart_keeps_the_last_good_settings_after_an_invalid_reload() {
        let wf = TestWorkflow::new(json!({}));
        let memory = MemoryIssues::new();
        let (factory, _) = counting_factory();
        let runtime = Runtime::start(options(&wf, &memory, factory));
        let handle = runtime.handle();
        handle.snapshot().await.unwrap();
        wf.rewrite(
            json!({"tracker": {"kind": "linear", "api_key": "token", "project_slug": null}}),
        );
        assert!(wf.store.force_reload().is_err());
        assert_eq!(wf.store.settings().tracker.kind.as_deref(), Some("memory"));
        crash(&handle).await;
        let snapshot = loop {
            if let Ok(snapshot) = handle
                .snapshot_with_timeout(Duration::from_millis(500))
                .await
            {
                break snapshot;
            }
        };
        assert_eq!(snapshot.tracker.kind.as_deref(), Some("memory"));
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn exceeding_the_restart_budget_stops_the_runtime() {
        let wf = TestWorkflow::new(json!({}));
        let memory = MemoryIssues::new();
        let (factory, _) = counting_factory();
        let runtime = Runtime::start(options(&wf, &memory, factory));
        let handle = runtime.handle();
        for round in 0..4 {
            crash(&handle).await;
            if round < 3 {
                // Wait for the restarted orchestrator before crashing it again.
                while handle
                    .snapshot_with_timeout(Duration::from_millis(500))
                    .await
                    .is_err()
                {}
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(5), runtime.wait())
            .await
            .expect("runtime stops");
        assert_eq!(
            result,
            Err(RuntimeError::RestartBudgetExceeded {
                max_restarts: 3,
                window_ms: 5_000
            })
        );
        assert_eq!(
            handle.snapshot().await,
            Err(crate::SnapshotError::Unavailable)
        );
    }
}
