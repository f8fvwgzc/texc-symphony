//! symphony-trackers: the tracker adapter boundary (`SymphonyElixir.Tracker`).
//!
//! - [`Tracker`]: the async, object-safe adapter trait (reads, secret env names, agent tools).
//! - Adapters: [`MemoryTracker`], [`LinearTracker`], [`GitHubTracker`], [`GitLabTracker`],
//!   [`JiraTracker`], [`AsanaTracker`]; [`build_tracker`] / [`tracker_for_kind`] pick one by
//!   `tracker.kind`.
//! - [`transport`]: the HTTP layer reproducing Req's timeout/retry/redirect/decoding defaults, with
//!   credential scrubbing.
//! - [`tool`]: dynamic tool responses and the session-bound [`ToolBinding`].
//!
//! Reads take the *current* [`TrackerSettings`] on every call (Elixir read live config per call);
//! tool execution uses the settings snapshot captured by [`ToolBinding::bind`].
//!
//! Writes: per SPEC §11.5 Symphony has no generic comment/state CRUD. Ticket mutations go through
//! each adapter's provider-native agent tool, backed by the raw passthroughs
//! [`LinearTracker::graphql`], [`GitHubTracker::request`], [`GitLabTracker::request`],
//! [`JiraTracker::request`] and [`AsanaTracker::request`].

#![warn(missing_docs)]

pub mod asana;
pub mod error;
pub mod github;
pub mod gitlab;
pub mod jira;
pub mod linear;
pub mod memory;
pub mod rest;
pub mod tool;
pub mod transport;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use symphony_core::config::{Settings, TrackerSettings};
use symphony_core::issue::normalize_state;
use symphony_core::{EnvSource, Issue, ProcessEnv};

pub use asana::AsanaTracker;
pub use error::{ErrorCategory, Provider, TrackerError};
pub use github::GitHubTracker;
pub use gitlab::GitLabTracker;
pub use jira::JiraTracker;
pub use linear::LinearTracker;
pub use memory::{MemoryIssues, MemoryTracker};
pub use tool::{ContentItem, ToolBinding, ToolContext, ToolResult};
pub use transport::{
    HttpClient, HttpRequest, HttpResponse, Method, ReqwestTransport, RetryPolicy, Transport,
    TransportError,
};

/// Safety cap on pages fetched by one paginated read (new in the Rust port; Elixir had no cap).
///
/// 1,000 pages is 50,000 Linear issues or 100,000 GitHub/GitLab/Jira/Asana records — far beyond any
/// realistic active-state scope — so hitting it means a provider pagination loop. The read then fails
/// with [`TrackerError::PaginationLimitExceeded`] (category `tracker_pagination`). Cursor-based
/// providers (Linear, Jira, Asana) additionally fail fast with
/// [`TrackerError::PaginationRepeatedCursor`] when a cursor repeats within one read.
pub const MAX_PAGES: usize = 1_000;

/// A tracker adapter.
///
/// The orchestrator only depends on the read methods; agent-side mutations stay behind the optional
/// provider-native tools so tracker-specific capabilities do not leak into scheduler policy.
#[async_trait]
pub trait Tracker: Send + Sync {
    /// The `tracker.kind` this adapter serves.
    fn kind(&self) -> &'static str;

    /// Adapter preflight (`validate_config/1`). Defaults to `Ok(())`.
    fn validate_config(&self, _settings: &TrackerSettings) -> Result<(), TrackerError> {
        Ok(())
    }

    /// Normalized issues in the configured scope whose state is one of `states`.
    /// An empty `states` list returns `Ok(vec![])` without a provider request.
    async fn fetch_issues_by_states(
        &self,
        settings: &TrackerSettings,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError>;

    /// Current snapshots of the given dispatch ids (request order, deduplicated; ids no longer in
    /// scope are omitted). An empty `ids` list returns `Ok(vec![])` without a provider request.
    async fn fetch_issues_by_ids(
        &self,
        settings: &TrackerSettings,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError>;

    /// Env var names to strip from the Codex child process.
    fn secret_environment_names(&self, settings: &TrackerSettings) -> Vec<String>;

    /// Provider-native agent tool specs (`{"name", "description", "inputSchema"}`). Default: none.
    fn agent_tool_specs(&self) -> Vec<Value> {
        Vec::new()
    }

    /// Executes an agent tool. Default: the generic "unsupported" response (`supportedTools: []`).
    async fn execute_agent_tool(
        &self,
        tool: Option<&str>,
        _arguments: &Value,
        _ctx: &ToolContext,
    ) -> ToolResult {
        ToolResult::unsupported_generic(tool)
    }

    /// Candidate poll: `fetch_issues_by_states(settings.active_states)`.
    async fn fetch_candidate_issues(
        &self,
        settings: &TrackerSettings,
    ) -> Result<Vec<Issue>, TrackerError> {
        let states = settings.active_states.clone().unwrap_or_default();
        self.fetch_issues_by_states(settings, &states).await
    }

    /// Startup cleanup read: `fetch_issues_by_states(settings.terminal_states)`.
    async fn fetch_terminal_issues(
        &self,
        settings: &TrackerSettings,
    ) -> Result<Vec<Issue>, TrackerError> {
        let states = settings.terminal_states.clone().unwrap_or_default();
        self.fetch_issues_by_states(settings, &states).await
    }
}

/// `true` when `state` (trim + lowercase) is one of the configured active states.
pub fn is_active_state(settings: &TrackerSettings, state: &str) -> bool {
    state_in(settings.active_states.as_deref(), state)
}

/// `true` when `state` (trim + lowercase) is one of the configured terminal states.
pub fn is_terminal_state(settings: &TrackerSettings, state: &str) -> bool {
    state_in(settings.terminal_states.as_deref(), state)
}

fn state_in(states: Option<&[String]>, state: &str) -> bool {
    let wanted = normalize_state(state);
    states
        .unwrap_or_default()
        .iter()
        .any(|s| normalize_state(s) == wanted)
}

/// Shared dependencies handed to every adapter.
#[derive(Debug, Clone)]
pub struct TrackerDeps {
    /// Environment used for `$VAR` and default-variable resolution (read on every call).
    pub env: Arc<dyn EnvSource>,
    /// HTTP client (retry/redirect policy over a transport).
    pub http: HttpClient,
    /// Issue list backing the `memory` adapter.
    pub memory: MemoryIssues,
}

impl TrackerDeps {
    /// Production dependencies: the process environment and a reqwest transport.
    pub fn new() -> Result<Self, TransportError> {
        Ok(Self::with_http(
            Arc::new(ProcessEnv),
            HttpClient::reqwest()?,
        ))
    }

    /// Custom environment and HTTP client; empty memory issue list.
    pub fn with_http(env: Arc<dyn EnvSource>, http: HttpClient) -> Self {
        Self {
            env,
            http,
            memory: MemoryIssues::new(),
        }
    }
}

/// Supported `tracker.kind` values (exact, case-sensitive).
pub const TRACKER_KINDS: [&str; 6] = symphony_core::config::SUPPORTED_TRACKER_KINDS;

/// `Tracker.adapter_for_kind/1`: the adapter for an exact kind string.
pub fn tracker_for_kind(kind: &str, deps: &TrackerDeps) -> Result<Arc<dyn Tracker>, TrackerError> {
    let tracker: Arc<dyn Tracker> = match kind {
        "asana" => Arc::new(AsanaTracker::new(deps.http.clone(), Arc::clone(&deps.env))),
        "github" => Arc::new(GitHubTracker::new(deps.http.clone(), Arc::clone(&deps.env))),
        "gitlab" => Arc::new(GitLabTracker::new(deps.http.clone(), Arc::clone(&deps.env))),
        "jira" => Arc::new(JiraTracker::new(deps.http.clone(), Arc::clone(&deps.env))),
        "linear" => Arc::new(LinearTracker::new(deps.http.clone())),
        "memory" => Arc::new(MemoryTracker::new(deps.memory.clone())),
        other => return Err(TrackerError::UnsupportedTrackerKind(other.to_owned())),
    };
    Ok(tracker)
}

/// Builds the adapter selected by `settings.tracker.kind`.
pub fn build_tracker(
    settings: &Settings,
    deps: &TrackerDeps,
) -> Result<Arc<dyn Tracker>, TrackerError> {
    let kind = settings
        .tracker
        .kind
        .as_deref()
        .ok_or(TrackerError::MissingTrackerKind)?;
    tracker_for_kind(kind, deps)
}

/// `Tracker.bind_agent_tools/0`: builds the adapter for the current settings and snapshots it.
pub fn bind_agent_tools(
    settings: &Settings,
    deps: &TrackerDeps,
) -> Result<ToolBinding, TrackerError> {
    let tracker = build_tracker(settings, deps)?;
    Ok(ToolBinding::bind(tracker, settings.tracker.clone()))
}

/// `Tracker.validate_config/1` for the configured kind (memory: always `Ok`).
pub fn validate_config(settings: &TrackerSettings, deps: &TrackerDeps) -> Result<(), TrackerError> {
    let kind = settings
        .kind
        .as_deref()
        .ok_or(TrackerError::MissingTrackerKind)?;
    tracker_for_kind(kind, deps)?.validate_config(settings)
}
