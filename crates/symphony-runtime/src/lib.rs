//! symphony-runtime: the scheduling heart of Symphony (Elixir `Orchestrator`, `AgentRunner`,
//! `Workspace`, `SSH`, `AgentRuntimeSupervisor`).
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use symphony_runtime::{Runtime, RuntimeOptions};
//! let workflow = symphony_core::WorkflowStore::start(None)?;
//! let deps = symphony_trackers::TrackerDeps::new()?;
//! let runtime = Runtime::start(RuntimeOptions::new(workflow, deps));
//! let handle = runtime.handle();
//! let snapshot = handle.snapshot().await?;
//! println!("{} running", snapshot.running.len());
//! let mut updates = handle.subscribe();
//! updates.changed().await?;
//! runtime.shutdown().await?;
//! # Ok(())
//! # }
//! ```
//!
//! Modules:
//! - [`runtime`]: [`Runtime`] (supervisor with the 3-restarts-in-5-s budget) and [`RuntimeOptions`].
//! - [`handle`]: [`RuntimeHandle`] — snapshot / refresh / issue lookup / generation subscription.
//! - [`snapshot`]: the typed [`Snapshot`] consumed by the HTTP API and dashboards.
//! - [`humanize`]: one-line summaries of Codex messages (`last_message`, dashboard EVENT column).
//! - [`orchestrator`]: scheduling rules (dispatch order, backoff formula, host selection).
//! - [`runner`]: [`AgentRunner`] — workspace, hooks and multi-turn Codex session per dispatch.
//! - [`worker`]: the [`WorkerFactory`] seam between orchestrator and runner.
//! - [`workspace`]: [`WorkspaceManager`] — layout, reuse, hooks, removal, path safety.
//! - [`ssh`]: SSH argv construction, `host:port` parsing and the Codex [`SshLauncher`].
//! - [`process`]: hook processes in their own process groups, killed on timeout.
//! - [`tracker`]: adapter selection and per-read timeouts.

#![warn(missing_docs)]

mod agents;
pub mod handle;
pub mod humanize;
pub mod orchestrator;
pub mod process;
mod recorder;
pub mod runner;
pub mod runtime;
pub mod snapshot;
pub mod ssh;
pub mod tracker;
pub mod worker;
pub mod workspace;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
pub(crate) mod test_support;

pub use handle::{
    DEFAULT_REFRESH_TIMEOUT, DEFAULT_SNAPSHOT_TIMEOUT, RefreshError, RuntimeCommand, RuntimeHandle,
    SnapshotError,
};
pub use humanize::humanize_codex_message;
pub use runner::{
    AgentRunner, CodexWorkerFactory, Continuation, RunnerOptions, TrackerToolHandler,
};
pub use runtime::{RestartPolicy, Runtime, RuntimeError, RuntimeOptions};
pub use snapshot::{
    BlockedSnapshot, CodexMessage, CodexTotals, IssueSnapshot, IssueStatus, PollingStatus,
    RefreshAck, RetrySnapshot, RunningSnapshot, Snapshot, TrackerInfo,
};
pub use ssh::{SshConfig, SshError, SshLauncher};
pub use tracker::{FetchError, IssueFetcher, LiveIssueFetcher, TrackerClient};
pub use worker::{
    FnWorkerFactory, RunError, WorkerContext, WorkerFactory, WorkerReporter, WorkerResult,
};
pub use workspace::{IssueContext, WorkspaceError, WorkspaceManager};
