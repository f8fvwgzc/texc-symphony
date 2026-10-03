//! Tracker access for the runtime: adapter selection per settings snapshot, call timeouts, and the
//! [`IssueFetcher`] used by the agent runner between turns.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use symphony_core::{Issue, Settings, WorkflowStore};
use symphony_trackers::{ToolBinding, Tracker, TrackerDeps, TrackerError, build_tracker};

/// Default bound on one tracker read (Elixir had none; adapters retry and paginate internally).
pub const DEFAULT_TRACKER_TIMEOUT: Duration = Duration::from_secs(120);

/// A failed tracker read.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FetchError {
    /// The adapter returned an error.
    #[error("{0}")]
    Tracker(#[from] TrackerError),
    /// The read exceeded the runtime's tracker timeout (new in the Rust port).
    #[error("tracker_timeout: {0}ms")]
    Timeout(u64),
}

/// Selects the adapter for the current settings (or a fixed override) and bounds every read.
#[derive(Clone)]
pub struct TrackerClient {
    deps: TrackerDeps,
    fixed: Option<Arc<dyn Tracker>>,
    timeout: Duration,
}

impl std::fmt::Debug for TrackerClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackerClient")
            .field("fixed", &self.fixed.as_ref().map(|t| t.kind()))
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl TrackerClient {
    /// Builds adapters from `deps` by `tracker.kind` (re-read on every call, like Elixir's config).
    pub fn new(deps: TrackerDeps) -> Self {
        Self {
            deps,
            fixed: None,
            timeout: DEFAULT_TRACKER_TIMEOUT,
        }
    }

    /// Always uses `tracker`, whatever `tracker.kind` says (tests, embedding).
    pub fn with_tracker(mut self, tracker: Arc<dyn Tracker>) -> Self {
        self.fixed = Some(tracker);
        self
    }

    /// Per-read timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The shared adapter dependencies.
    pub fn deps(&self) -> &TrackerDeps {
        &self.deps
    }

    /// The adapter for `settings`.
    pub fn tracker(&self, settings: &Settings) -> Result<Arc<dyn Tracker>, TrackerError> {
        match &self.fixed {
            Some(tracker) => Ok(Arc::clone(tracker)),
            None => build_tracker(settings, &self.deps),
        }
    }

    /// `Tracker.bind_agent_tools/0` for one Codex session.
    pub fn bind_agent_tools(&self, settings: &Settings) -> Result<ToolBinding, TrackerError> {
        Ok(ToolBinding::bind(
            self.tracker(settings)?,
            settings.tracker.clone(),
        ))
    }

    async fn bounded<T>(
        &self,
        fut: impl Future<Output = Result<T, TrackerError>>,
    ) -> Result<T, FetchError> {
        match tokio::time::timeout(self.timeout, fut).await {
            Ok(result) => result.map_err(FetchError::Tracker),
            Err(_) => Err(FetchError::Timeout(
                u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
            )),
        }
    }

    /// `Tracker.fetch_issues_by_states/1`; an empty list makes no request.
    pub async fn fetch_issues_by_states(
        &self,
        settings: &Settings,
        states: &[String],
    ) -> Result<Vec<Issue>, FetchError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let tracker = self.tracker(settings)?;
        self.bounded(tracker.fetch_issues_by_states(&settings.tracker, states))
            .await
    }

    /// `Tracker.fetch_issues_by_ids/1`; an empty list makes no request.
    pub async fn fetch_issues_by_ids(
        &self,
        settings: &Settings,
        ids: &[String],
    ) -> Result<Vec<Issue>, FetchError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let tracker = self.tracker(settings)?;
        self.bounded(tracker.fetch_issues_by_ids(&settings.tracker, ids))
            .await
    }
}

/// Re-fetches issues by id (the runner's `issue_state_fetcher`).
#[async_trait]
pub trait IssueFetcher: Send + Sync {
    /// Current snapshots of `ids`.
    async fn fetch_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, FetchError>;
}

/// [`IssueFetcher`] reading through a [`TrackerClient`] with the workflow's current settings.
#[derive(Debug, Clone)]
pub struct LiveIssueFetcher {
    workflow: Arc<WorkflowStore>,
    client: TrackerClient,
}

impl LiveIssueFetcher {
    /// A fetcher over `client` using the settings currently held by `workflow`.
    pub fn new(workflow: Arc<WorkflowStore>, client: TrackerClient) -> Self {
        Self { workflow, client }
    }
}

#[async_trait]
impl IssueFetcher for LiveIssueFetcher {
    async fn fetch_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, FetchError> {
        let settings = self.workflow.settings();
        self.client.fetch_issues_by_ids(&settings, ids).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_core::config::TrackerSettings;

    struct SlowTracker;

    #[async_trait]
    impl Tracker for SlowTracker {
        fn kind(&self) -> &'static str {
            "memory"
        }

        async fn fetch_issues_by_states(
            &self,
            _settings: &TrackerSettings,
            _states: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Vec::new())
        }

        async fn fetch_issues_by_ids(
            &self,
            _settings: &TrackerSettings,
            _ids: &[String],
        ) -> Result<Vec<Issue>, TrackerError> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Vec::new())
        }

        fn secret_environment_names(&self, _settings: &TrackerSettings) -> Vec<String> {
            Vec::new()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reads_are_bounded_and_empty_requests_skip_the_tracker() {
        let client = TrackerClient::new(TrackerDeps::new().unwrap())
            .with_tracker(Arc::new(SlowTracker))
            .with_timeout(Duration::from_secs(2));
        let settings = Settings::default();
        assert_eq!(
            client.fetch_issues_by_ids(&settings, &["a".into()]).await,
            Err(FetchError::Timeout(2_000))
        );
        assert_eq!(
            client
                .fetch_issues_by_states(&settings, &["Todo".into()])
                .await,
            Err(FetchError::Timeout(2_000))
        );
        assert_eq!(client.fetch_issues_by_ids(&settings, &[]).await, Ok(vec![]));
        assert_eq!(
            client.fetch_issues_by_states(&settings, &[]).await,
            Ok(vec![])
        );
        assert_eq!(
            FetchError::Timeout(2_000).to_string(),
            "tracker_timeout: 2000ms"
        );
    }
}
