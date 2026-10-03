//! The worker boundary between the orchestrator and an agent run.
//!
//! The orchestrator never runs agents directly: it asks a [`WorkerFactory`] for a future per dispatch
//! and spawns it on its `JoinSet`. Production uses [`crate::runner::CodexWorkerFactory`] (workspace +
//! hooks + Codex turns); tests and embedders can supply their own (see [`FnWorkerFactory`]).
//!
//! A worker reports back only through its [`WorkerContext`]: Codex events go to `events`, the
//! workspace location to `reporter`, and termination is the future's own completion (the analogue of
//! the Elixir monitor `DOWN`). Cancellation is cooperative through `cancel`; a worker that does not
//! finish within the grace period is aborted (dropped).

use std::future::Future;
use std::time::Duration;

use futures::future::BoxFuture;
use symphony_codex::{Blocker, CodexError, EventSink};
use symphony_core::{Issue, PromptError};
use symphony_trackers::TrackerError;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::tracker::FetchError;
use crate::workspace::WorkspaceError;

/// Message from a worker to the orchestrator (besides Codex events).
#[derive(Debug)]
pub(crate) enum WorkerMessage {
    /// `{:worker_runtime_info, issue_id, %{worker_host, workspace_path}}`.
    RuntimeInfo {
        run_id: u64,
        worker_host: Option<String>,
        workspace_path: String,
    },
}

/// Reports runtime facts about one run to the orchestrator. Sends never block or fail.
#[derive(Debug, Clone)]
pub struct WorkerReporter {
    run_id: u64,
    tx: Option<mpsc::UnboundedSender<WorkerMessage>>,
}

impl WorkerReporter {
    pub(crate) fn new(run_id: u64, tx: mpsc::UnboundedSender<WorkerMessage>) -> Self {
        Self {
            run_id,
            tx: Some(tx),
        }
    }

    /// A reporter that drops everything (for running an agent outside the orchestrator).
    pub fn detached() -> Self {
        Self {
            run_id: 0,
            tx: None,
        }
    }

    /// Reports where the workspace lives (sent once, after the workspace is created).
    pub fn runtime_info(&self, worker_host: Option<&str>, workspace_path: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(WorkerMessage::RuntimeInfo {
                run_id: self.run_id,
                worker_host: worker_host.map(str::to_owned),
                workspace_path: workspace_path.to_owned(),
            });
        }
    }
}

/// Everything a worker needs for one dispatch.
#[derive(Debug)]
pub struct WorkerContext {
    /// The (revalidated) issue.
    pub issue: Issue,
    /// Retry attempt (`None` on first dispatch); rendered as `{{ attempt }}`.
    pub attempt: Option<u32>,
    /// SSH worker host chosen by the orchestrator (`None` = local). One run never hops hosts.
    pub worker_host: Option<String>,
    /// Orchestrator-assigned run id (unique per runtime process).
    pub run_id: u64,
    /// Codex events of this run.
    pub events: EventSink,
    /// Runtime info channel.
    pub reporter: WorkerReporter,
    /// Cooperative cancellation (reconciliation, stall, shutdown).
    pub cancel: CancellationToken,
    /// How long the worker may take to wind down after `cancel` fires before it is aborted.
    pub cancel_grace: Duration,
}

impl WorkerContext {
    /// A context for running a worker outside the orchestrator (events/info are dropped unless
    /// `events` is replaced).
    pub fn standalone(issue: Issue) -> Self {
        Self {
            issue,
            attempt: None,
            worker_host: None,
            run_id: 0,
            events: EventSink::none(),
            reporter: WorkerReporter::detached(),
            cancel: CancellationToken::new(),
            cancel_grace: Duration::from_secs(5),
        }
    }
}

/// Why an agent run failed. `Display` is the reason used in `"agent exited: <reason>"`.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RunError {
    /// Workspace creation or a fatal hook failed.
    #[error("{0}")]
    Workspace(#[from] WorkspaceError),
    /// The Codex session failed.
    #[error("{0}")]
    Codex(#[from] CodexError),
    /// The prompt could not be rendered.
    #[error("{0}")]
    Prompt(#[from] PromptError),
    /// `{:issue_state_refresh_failed, reason}` between turns.
    #[error("issue_state_refresh_failed: {0}")]
    IssueStateRefresh(FetchError),
    /// The tracker's agent tools could not be bound.
    #[error("tool_binding_failed: {0}")]
    ToolBinding(TrackerError),
    /// The run was cancelled by the orchestrator.
    #[error("cancelled")]
    Cancelled,
    /// Any other failure (custom workers).
    #[error("{0}")]
    Other(String),
}

impl RunError {
    /// `Some` when the run ended because Codex needs a human (the issue is blocked, not retried).
    pub fn blocker(&self) -> Option<Blocker> {
        match self {
            Self::Codex(err) => err.blocker(),
            _ => None,
        }
    }
}

/// The result of a worker future.
pub type WorkerResult = Result<(), RunError>;

/// Creates the future that performs one agent run.
pub trait WorkerFactory: Send + Sync + 'static {
    /// Starts a run. The returned future is spawned on the orchestrator's `JoinSet`; it must honour
    /// `ctx.cancel` (or tolerate being dropped).
    fn start(&self, ctx: WorkerContext) -> BoxFuture<'static, WorkerResult>;
}

/// A [`WorkerFactory`] from a closure (tests, embedding).
pub struct FnWorkerFactory<F>(pub F);

impl<F, Fut> WorkerFactory for FnWorkerFactory<F>
where
    F: Fn(WorkerContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = WorkerResult> + Send + 'static,
{
    fn start(&self, ctx: WorkerContext) -> BoxFuture<'static, WorkerResult> {
        Box::pin((self.0)(ctx))
    }
}

impl<F> std::fmt::Debug for FnWorkerFactory<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FnWorkerFactory")
    }
}
