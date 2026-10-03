//! The seam between the HTTP server and the orchestrator.
//!
//! The server never talks to `symphony-runtime` directly: the binary adapts the runtime's
//! orchestrator handle to [`ControlPlane`]. This keeps the server testable with a static double
//! ([`crate::testing::StaticControlPlane`]) and lets both crates evolve independently.

use async_trait::async_trait;
use tokio::sync::watch;

use crate::presenter;
use crate::view::{IssueView, RefreshAccepted, StateView};

/// Why a state snapshot could not be produced. Reported **in-band** (HTTP 200) by
/// `GET /api/v1/state` and in SSE `snapshot` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum StateError {
    /// The orchestrator did not answer in time (`snapshot_timeout`).
    #[error("snapshot_timeout")]
    Timeout,
    /// The orchestrator is not running (`snapshot_unavailable`).
    #[error("snapshot_unavailable")]
    Unavailable,
}

impl StateError {
    /// Wire code: `snapshot_timeout` / `snapshot_unavailable`.
    pub fn code(self) -> &'static str {
        match self {
            StateError::Timeout => "snapshot_timeout",
            StateError::Unavailable => "snapshot_unavailable",
        }
    }

    /// Wire message: `Snapshot timed out` / `Snapshot unavailable`.
    pub fn message(self) -> &'static str {
        match self {
            StateError::Timeout => "Snapshot timed out",
            StateError::Unavailable => "Snapshot unavailable",
        }
    }
}

/// The orchestrator is not running (or did not answer): `503 orchestrator_unavailable`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, thiserror::Error)]
#[error("orchestrator_unavailable")]
pub struct Unavailable;

/// What the HTTP server needs from the orchestrator.
///
/// Implementations should answer quickly but do not need their own timeouts: the server wraps
/// [`state`](Self::state) and [`issue`](Self::issue) in `ServerConfig::snapshot_timeout`
/// (15 s; elapsed means [`StateError::Timeout`] / `404 issue_not_found`) and
/// [`refresh`](Self::refresh) in `ServerConfig::refresh_timeout` (5 s; elapsed means
/// `503 orchestrator_unavailable`).
#[async_trait]
pub trait ControlPlane: Send + Sync + 'static {
    /// Current orchestrator snapshot (Elixir `Orchestrator.snapshot/2`). Lists may be unsorted;
    /// fill `RetryEntry::due_at` with [`presenter::due_at`] and `last_message` with the humanized
    /// Codex message.
    async fn state(&self) -> Result<StateView, StateError>;

    /// Queue an immediate poll + reconcile (Elixir `Orchestrator.request_refresh/1`).
    async fn refresh(&self) -> Result<RefreshAccepted, Unavailable>;

    /// Configured `workspace.root` (verbatim, not expanded), used for the workspace path of an
    /// issue whose rows carry none. Read per call so `WORKFLOW.md` reloads apply.
    fn workspace_root(&self) -> String;

    /// Detail of one issue, `None` when it is not running, retrying or blocked, or when the
    /// snapshot failed (`404 issue_not_found`). The default derives it from
    /// [`state`](Self::state) with [`presenter::issue_view`]; override only to avoid a full
    /// snapshot.
    async fn issue(&self, identifier: &str) -> Option<IssueView> {
        let view = self.state().await.ok()?;
        presenter::issue_view(identifier, &view, &self.workspace_root())
    }

    /// Change notifications: a receiver whose value is the orchestrator **generation**, a counter
    /// bumped (`send_modify(|g| *g += 1)`) on every state change the dashboard should see. It is
    /// the SSE `id:`. When the sender is dropped, live streams keep their heartbeats but stop
    /// sending snapshots.
    fn changes(&self) -> watch::Receiver<u64>;
}
