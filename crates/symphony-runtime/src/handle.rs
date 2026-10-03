//! [`RuntimeHandle`]: the cheap, cloneable client of the orchestrator actor used by the HTTP server
//! and the terminal dashboard (`Orchestrator.snapshot/2`, `request_refresh/1`, PubSub).

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::snapshot::{IssueSnapshot, RefreshAck, Snapshot};

/// Default `snapshot` timeout (Elixir `snapshot/2` default 15 s).
pub const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default `request_refresh` timeout (Elixir `GenServer.call` default 5 s).
pub const DEFAULT_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);

/// A request to the orchestrator actor. Public so tests and embedders can build fake orchestrators
/// with [`RuntimeHandle::from_channel`].
#[derive(Debug)]
pub enum RuntimeCommand {
    /// Reply with a fresh [`Snapshot`].
    Snapshot(oneshot::Sender<Snapshot>),
    /// Queue an immediate poll + reconcile and reply with the acknowledgement.
    RequestRefresh(oneshot::Sender<RefreshAck>),
    /// Test-only: make the orchestrator task panic (exercises the supervisor).
    #[cfg(test)]
    Crash,
}

/// `snapshot` failures (`:timeout` / `:unavailable`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotError {
    /// The orchestrator did not answer in time (it may be blocked on a slow tracker call).
    #[error("snapshot_timeout")]
    Timeout,
    /// No orchestrator is running (not started, restarting, stopped, or the request was dropped).
    #[error("snapshot_unavailable")]
    Unavailable,
}

/// `request_refresh` failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefreshError {
    /// No orchestrator is running (HTTP 503 `orchestrator_unavailable`).
    #[error("orchestrator_unavailable")]
    Unavailable,
    /// The orchestrator did not answer within the refresh timeout (Elixir crashed the request: 500).
    #[error("refresh_timeout")]
    Timeout,
}

#[derive(Debug)]
pub(crate) struct HandleShared {
    pub(crate) commands: ArcSwapOption<mpsc::Sender<RuntimeCommand>>,
    pub(crate) generation: watch::Receiver<u64>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) snapshot_timeout: Duration,
    pub(crate) refresh_timeout: Duration,
}

/// Client of the orchestrator. Clone freely; it survives orchestrator restarts.
#[derive(Debug, Clone)]
pub struct RuntimeHandle {
    shared: Arc<HandleShared>,
}

impl RuntimeHandle {
    pub(crate) fn from_shared(shared: Arc<HandleShared>) -> Self {
        Self { shared }
    }

    /// A handle backed by a caller-provided command channel (fake orchestrators in tests). The
    /// generation receiver is what [`RuntimeHandle::subscribe`] returns.
    pub fn from_channel(
        commands: mpsc::Sender<RuntimeCommand>,
        generation: watch::Receiver<u64>,
    ) -> Self {
        Self::from_shared(Arc::new(HandleShared {
            commands: ArcSwapOption::from_pointee(commands),
            generation,
            shutdown: CancellationToken::new(),
            snapshot_timeout: DEFAULT_SNAPSHOT_TIMEOUT,
            refresh_timeout: DEFAULT_REFRESH_TIMEOUT,
        }))
    }

    /// A handle with no orchestrator: every call is `Unavailable`.
    pub fn unavailable() -> Self {
        let (_tx, generation) = watch::channel(0);
        Self::from_shared(Arc::new(HandleShared {
            commands: ArcSwapOption::empty(),
            generation,
            shutdown: CancellationToken::new(),
            snapshot_timeout: DEFAULT_SNAPSHOT_TIMEOUT,
            refresh_timeout: DEFAULT_REFRESH_TIMEOUT,
        }))
    }

    /// A handle answering every snapshot with `snapshot` and every refresh with a non-coalesced ack
    /// (spawns a responder task on the current tokio runtime).
    pub fn with_static_snapshot(snapshot: Snapshot) -> Self {
        let (tx, mut rx) = mpsc::channel::<RuntimeCommand>(16);
        let (gen_tx, generation) = watch::channel(snapshot.generation);
        tokio::spawn(async move {
            let _keep_generation = gen_tx;
            while let Some(command) = rx.recv().await {
                match command {
                    RuntimeCommand::Snapshot(reply) => {
                        let _ = reply.send(snapshot.clone());
                    }
                    RuntimeCommand::RequestRefresh(reply) => {
                        let _ = reply.send(RefreshAck {
                            queued: true,
                            coalesced: false,
                            requested_at: chrono::Utc::now(),
                            operations: vec!["poll".into(), "reconcile".into()],
                        });
                    }
                    #[cfg(test)]
                    RuntimeCommand::Crash => {}
                }
            }
        });
        Self::from_channel(tx, generation)
    }

    /// `true` while an orchestrator accepts commands.
    pub fn is_available(&self) -> bool {
        self.shared
            .commands
            .load()
            .as_ref()
            .is_some_and(|tx| !tx.is_closed())
    }

    async fn send(&self, command: RuntimeCommand) -> bool {
        let Some(tx) = self.shared.commands.load_full() else {
            return false;
        };
        tx.send(command).await.is_ok()
    }

    /// The configured snapshot timeout.
    pub fn snapshot_timeout(&self) -> Duration {
        self.shared.snapshot_timeout
    }

    /// `Orchestrator.snapshot/2` with the configured timeout (15 s by default).
    pub async fn snapshot(&self) -> Result<Snapshot, SnapshotError> {
        self.snapshot_with_timeout(self.shared.snapshot_timeout)
            .await
    }

    /// `Orchestrator.snapshot/2` with an explicit timeout (the whole call, including queueing).
    pub async fn snapshot_with_timeout(&self, limit: Duration) -> Result<Snapshot, SnapshotError> {
        let (reply, response) = oneshot::channel();
        let call = async {
            if !self.send(RuntimeCommand::Snapshot(reply)).await {
                return Err(SnapshotError::Unavailable);
            }
            response.await.map_err(|_| SnapshotError::Unavailable)
        };
        tokio::time::timeout(limit, call)
            .await
            .unwrap_or(Err(SnapshotError::Timeout))
    }

    /// `Orchestrator.request_refresh/1`: queue an immediate poll + reconcile (coalesced with a poll
    /// that is already running or due).
    pub async fn request_refresh(&self) -> Result<RefreshAck, RefreshError> {
        if !self.is_available() {
            return Err(RefreshError::Unavailable);
        }
        let (reply, response) = oneshot::channel();
        let call = async {
            if !self.send(RuntimeCommand::RequestRefresh(reply)).await {
                return Err(RefreshError::Unavailable);
            }
            response.await.map_err(|_| RefreshError::Unavailable)
        };
        tokio::time::timeout(self.shared.refresh_timeout, call)
            .await
            .unwrap_or(Err(RefreshError::Timeout))
    }

    /// `Presenter.issue_payload/3`: the issue's running/retry/blocked view, `Ok(None)` when the
    /// identifier is in none of them.
    pub async fn issue(&self, identifier: &str) -> Result<Option<IssueSnapshot>, SnapshotError> {
        Ok(self.snapshot().await?.issue(identifier))
    }

    /// Generation counter bumped on every orchestrator state change (the Elixir
    /// `observability:dashboard` PubSub). Receivers see only the latest value, so bursts coalesce.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.shared.generation.clone()
    }

    /// Requests a graceful shutdown: workers are cancelled (running `after_run` within the grace
    /// period), then aborted; the runtime task then completes.
    pub fn shutdown(&self) {
        self.shared.shutdown.cancel();
    }

    /// The shutdown token (cancelled by [`RuntimeHandle::shutdown`]).
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shared.shutdown.clone()
    }

    #[cfg(test)]
    pub(crate) async fn crash(&self) -> bool {
        self.send(RuntimeCommand::Crash).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_times_out_when_the_orchestrator_never_replies() {
        let (tx, mut rx) = mpsc::channel(4);
        let (_gen_tx, generation) = watch::channel(0);
        let handle = RuntimeHandle::from_channel(tx, generation);
        let hold = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Some(cmd) = rx.recv().await {
                held.push(cmd);
            }
        });
        assert_eq!(
            handle
                .snapshot_with_timeout(Duration::from_millis(10))
                .await,
            Err(SnapshotError::Timeout)
        );
        hold.abort();
    }

    #[tokio::test]
    async fn unavailable_handles_report_unavailable() {
        let handle = RuntimeHandle::unavailable();
        assert!(!handle.is_available());
        assert_eq!(handle.snapshot().await, Err(SnapshotError::Unavailable));
        assert_eq!(
            handle.request_refresh().await,
            Err(RefreshError::Unavailable)
        );
        assert_eq!(handle.issue("MT-1").await, Err(SnapshotError::Unavailable));
    }

    #[tokio::test]
    async fn dropped_requests_are_unavailable() {
        let (tx, mut rx) = mpsc::channel(4);
        let (_gen_tx, generation) = watch::channel(0);
        let handle = RuntimeHandle::from_channel(tx, generation);
        tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                drop(cmd);
            }
        });
        assert_eq!(handle.snapshot().await, Err(SnapshotError::Unavailable));
    }
}
