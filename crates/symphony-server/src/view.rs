//! View model: Rust types whose serde form *is* the JSON contract (`docs/api/openapi.yaml`).
//!
//! * [`StateView`] and its rows are what a [`ControlPlane`](crate::ControlPlane) hands to the
//!   server. The server adds `generated_at` and `counts` and wraps it in a [`StatePayload`]
//!   (the body of `GET /api/v1/state` and of every SSE `snapshot` event).
//! * [`IssueView`] is the body of `GET /api/v1/{issue_identifier}`; build it from a
//!   [`StateView`] with [`presenter::issue_view`](crate::presenter::issue_view).
//! * [`RefreshAccepted`] is the body of `POST /api/v1/refresh`.
//!
//! Timestamps are typed (`DateTime<Utc>`) and formatted on serialization exactly like the Elixir
//! `Presenter`: second-truncated with a `Z` suffix, except `requested_at` (microseconds).
//! `Option` fields always serialize as `null`; nothing is skipped.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::presenter;

/// Live orchestrator state as provided by a [`ControlPlane`](crate::ControlPlane).
///
/// Lists may be in any order: the server sorts each by `issue_id` before serializing (the Elixir
/// output order, which came from small-map iteration).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StateView {
    /// Issues with an active agent session.
    pub running: Vec<RunningEntry>,
    /// Issues waiting for their retry timer.
    pub retrying: Vec<RetryEntry>,
    /// Issues paused because Codex asked for operator input or approval.
    #[serde(default)]
    pub blocked: Vec<BlockedEntry>,
    /// Aggregate Codex token usage and **ended**-session runtime.
    pub codex_totals: CodexTotals,
    /// Last rate-limit object reported by Codex, passed through verbatim (or `null`).
    pub rate_limits: Option<Value>,
}

/// Token counters of one session (`tokens` of running rows).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    /// Input (prompt) tokens.
    pub input_tokens: u64,
    /// Output (completion) tokens.
    pub output_tokens: u64,
    /// Total tokens as reported by Codex.
    pub total_tokens: u64,
}

/// `codex_totals`: aggregate token usage and the runtime of **ended** sessions (Elixir parity;
/// clients add `now - started_at` of each running row for a live total).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CodexTotals {
    /// Input tokens over all sessions.
    pub input_tokens: u64,
    /// Output tokens over all sessions.
    pub output_tokens: u64,
    /// Total tokens over all sessions.
    pub total_tokens: u64,
    /// Seconds of ended sessions. Whole values serialize as integers (`42`), others as floats.
    #[serde(serialize_with = "serialize_seconds")]
    pub seconds_running: f64,
}

/// One row of `running`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningEntry {
    /// Tracker issue id.
    pub issue_id: String,
    /// Human identifier such as `MT-123`.
    pub issue_identifier: String,
    /// Tracker URL of the issue.
    pub issue_url: Option<String>,
    /// Tracker state name, e.g. `In Progress`.
    pub state: Option<String>,
    /// SSH worker host, `None` for local runs.
    pub worker_host: Option<String>,
    /// Workspace directory, once known.
    pub workspace_path: Option<String>,
    /// Codex thread/session id.
    pub session_id: Option<String>,
    /// Turns started in this session.
    pub turn_count: u32,
    /// Last Codex event name (e.g. `notification`).
    pub last_event: Option<String>,
    /// Humanized summary of the last Codex message (`humanize_codex_message`), `None` before the
    /// first message.
    pub last_message: Option<String>,
    /// Session start (serialized second-truncated).
    #[serde(serialize_with = "ts_opt")]
    pub started_at: Option<DateTime<Utc>>,
    /// Timestamp of the last Codex event (serialized second-truncated).
    #[serde(serialize_with = "ts_opt")]
    pub last_event_at: Option<DateTime<Utc>>,
    /// Token counters of the session.
    pub tokens: TokenCounts,
}

/// One row of `retrying`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetryEntry {
    /// Tracker issue id.
    pub issue_id: String,
    /// Human identifier.
    pub issue_identifier: String,
    /// Tracker URL of the issue.
    pub issue_url: Option<String>,
    /// Retry attempt number.
    pub attempt: Option<u32>,
    /// When the retry fires; compute it with [`presenter::due_at`] from the orchestrator's
    /// `due_in_ms` (serialized second-truncated).
    #[serde(serialize_with = "ts_opt")]
    pub due_at: Option<DateTime<Utc>>,
    /// Error that caused the retry.
    pub error: Option<String>,
    /// SSH worker host of the failed attempt.
    pub worker_host: Option<String>,
    /// Workspace directory of the failed attempt.
    pub workspace_path: Option<String>,
}

/// One row of `blocked`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockedEntry {
    /// Tracker issue id.
    pub issue_id: String,
    /// Human identifier.
    pub issue_identifier: String,
    /// Tracker URL of the issue.
    pub issue_url: Option<String>,
    /// Tracker state name.
    pub state: Option<String>,
    /// Why the issue is blocked (e.g. `codex turn requires operator input`).
    pub error: Option<String>,
    /// SSH worker host, `None` for local runs.
    pub worker_host: Option<String>,
    /// Workspace directory.
    pub workspace_path: Option<String>,
    /// Codex thread/session id.
    pub session_id: Option<String>,
    /// When the issue became blocked (serialized second-truncated).
    #[serde(serialize_with = "ts_opt")]
    pub blocked_at: Option<DateTime<Utc>>,
    /// Last Codex event name.
    pub last_event: Option<String>,
    /// Humanized last Codex message.
    pub last_message: Option<String>,
    /// Timestamp of the last Codex event (serialized second-truncated).
    #[serde(serialize_with = "ts_opt")]
    pub last_event_at: Option<DateTime<Utc>>,
}

/// `counts` of a state snapshot (always the list lengths).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// `running.len()`.
    pub running: usize,
    /// `retrying.len()`.
    pub retrying: usize,
    /// `blocked.len()`.
    pub blocked: usize,
}

/// Successful body of `GET /api/v1/state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Request time, second-truncated.
    #[serde(serialize_with = "ts")]
    pub generated_at: DateTime<Utc>,
    /// List lengths.
    pub counts: Counts,
    /// The snapshot itself.
    #[serde(flatten)]
    pub view: StateView,
}

/// In-band failure body of `GET /api/v1/state` (still HTTP 200).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateFailure {
    /// Request time, second-truncated.
    #[serde(serialize_with = "ts")]
    pub generated_at: DateTime<Utc>,
    /// `snapshot_timeout` / `snapshot_unavailable`.
    pub error: ErrorBody,
}

/// Body of `GET /api/v1/state` and the `data` of SSE `snapshot` events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StatePayload {
    /// The orchestrator answered.
    Snapshot(StateSnapshot),
    /// The snapshot timed out or the orchestrator is not running.
    Failure(StateFailure),
}

/// `status` of an [`IssueView`] (`running` wins over `retrying`, which wins over `blocked`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueStatus {
    /// The issue has an active session.
    Running,
    /// The issue waits for a retry.
    Retrying,
    /// The issue waits for operator input.
    Blocked,
}

/// `workspace` of an [`IssueView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueWorkspace {
    /// First known workspace path, else `<workspace.root>/<workspace_key(identifier)>`.
    pub path: String,
    /// First known worker host.
    pub host: Option<String>,
}

/// `attempts` of an [`IssueView`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueAttempts {
    /// `max(current_retry_attempt - 1, 0)`.
    pub restart_count: u32,
    /// `retry.attempt`, 0 without a retry.
    pub current_retry_attempt: u32,
}

/// `running` of an [`IssueView`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueRunning {
    /// SSH worker host.
    pub worker_host: Option<String>,
    /// Workspace directory.
    pub workspace_path: Option<String>,
    /// Codex session id.
    pub session_id: Option<String>,
    /// Turns started.
    pub turn_count: u32,
    /// Tracker state name.
    pub state: Option<String>,
    /// Session start.
    #[serde(serialize_with = "ts_opt")]
    pub started_at: Option<DateTime<Utc>>,
    /// Last Codex event name.
    pub last_event: Option<String>,
    /// Humanized last Codex message.
    pub last_message: Option<String>,
    /// Timestamp of the last Codex event.
    #[serde(serialize_with = "ts_opt")]
    pub last_event_at: Option<DateTime<Utc>>,
    /// Token counters.
    pub tokens: TokenCounts,
}

/// `retry` of an [`IssueView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueRetry {
    /// Retry attempt number.
    pub attempt: Option<u32>,
    /// When the retry fires.
    #[serde(serialize_with = "ts_opt")]
    pub due_at: Option<DateTime<Utc>>,
    /// Error of the failed attempt.
    pub error: Option<String>,
    /// SSH worker host.
    pub worker_host: Option<String>,
    /// Workspace directory.
    pub workspace_path: Option<String>,
}

/// `blocked` of an [`IssueView`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueBlocked {
    /// SSH worker host.
    pub worker_host: Option<String>,
    /// Workspace directory.
    pub workspace_path: Option<String>,
    /// Codex session id.
    pub session_id: Option<String>,
    /// Tracker state name.
    pub state: Option<String>,
    /// Blocker text.
    pub error: Option<String>,
    /// When the issue became blocked.
    #[serde(serialize_with = "ts_opt")]
    pub blocked_at: Option<DateTime<Utc>>,
    /// Last Codex event name.
    pub last_event: Option<String>,
    /// Humanized last Codex message.
    pub last_message: Option<String>,
    /// Timestamp of the last Codex event.
    #[serde(serialize_with = "ts_opt")]
    pub last_event_at: Option<DateTime<Utc>>,
}

/// `logs` of an [`IssueView`] (reserved; always `{"codex_session_logs": []}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueLogs {
    /// Always empty.
    pub codex_session_logs: Vec<Value>,
}

/// One entry of `recent_events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentEvent {
    /// Timestamp of the event.
    #[serde(serialize_with = "ts")]
    pub at: DateTime<Utc>,
    /// Codex event name.
    pub event: Option<String>,
    /// Humanized message.
    pub message: Option<String>,
}

/// Body of `GET /api/v1/{issue_identifier}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueView {
    /// The identifier that was looked up.
    pub issue_identifier: String,
    /// Tracker issue id (running, else retry, else blocked).
    pub issue_id: String,
    /// Where the issue currently is.
    pub status: IssueStatus,
    /// Workspace location.
    pub workspace: IssueWorkspace,
    /// Retry counters.
    pub attempts: IssueAttempts,
    /// Running details, if running.
    pub running: Option<IssueRunning>,
    /// Retry details, if retrying.
    pub retry: Option<IssueRetry>,
    /// Blocked details, if blocked.
    pub blocked: Option<IssueBlocked>,
    /// Reserved, always empty.
    pub logs: IssueLogs,
    /// At most one entry: the last Codex event of the running (else blocked) session.
    pub recent_events: Vec<RecentEvent>,
    /// `blocked.error`, else `retry.error`.
    pub last_error: Option<String>,
    /// Reserved, always `{}`.
    pub tracked: Map<String, Value>,
}

/// Body of `POST /api/v1/refresh` (HTTP 202).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshAccepted {
    /// Always `true`.
    pub queued: bool,
    /// `true` when a poll was already running or due, so nothing new was scheduled.
    pub coalesced: bool,
    /// When the orchestrator accepted the request (serialized with microseconds).
    #[serde(serialize_with = "ts_micros")]
    pub requested_at: DateTime<Utc>,
    /// Always `["poll", "reconcile"]` today.
    pub operations: Vec<String>,
}

impl RefreshAccepted {
    /// The standard acknowledgement: `queued: true`, `operations: ["poll", "reconcile"]`.
    pub fn new(coalesced: bool, requested_at: DateTime<Utc>) -> Self {
        RefreshAccepted {
            queued: true,
            coalesced,
            requested_at,
            operations: vec!["poll".to_owned(), "reconcile".to_owned()],
        }
    }
}

/// `store` of [`Health`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreMode {
    /// Run history endpoints are served from SQLite.
    Sqlite,
    /// Persistence is off; history endpoints answer `503 store_disabled`.
    Disabled,
}

/// Body of `GET /api/v1/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// Always `ok`.
    pub status: String,
    /// Crate version of the binary.
    pub version: String,
    /// Whole seconds since the HTTP server started.
    pub uptime_seconds: u64,
    /// Whether run history is available.
    pub store: StoreMode,
}

/// `data` of SSE `heartbeat` events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// Send time, second-truncated.
    #[serde(serialize_with = "ts")]
    pub at: DateTime<Utc>,
    /// Generation of the last snapshot sent on this stream.
    pub generation: u64,
}

/// `{"code": ..., "message": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Stable machine-readable code.
    pub code: String,
    /// Human-readable text.
    pub message: String,
}

/// `{"error": {"code": ..., "message": ...}}`: the body of every non-2xx response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    /// The error.
    pub error: ErrorBody,
}

impl ErrorEnvelope {
    /// Envelope with `code` and `message`.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        ErrorEnvelope {
            error: ErrorBody {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

fn ts<S: Serializer>(at: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&presenter::format_timestamp(at))
}

fn ts_opt<S: Serializer>(at: &Option<DateTime<Utc>>, serializer: S) -> Result<S::Ok, S::Error> {
    match at {
        Some(at) => ts(at, serializer),
        None => serializer.serialize_none(),
    }
}

fn ts_micros<S: Serializer>(at: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&presenter::format_precise_timestamp(at))
}

/// Largest integer an `f64` represents exactly (2^53).
const MAX_EXACT_F64_INT: f64 = 9_007_199_254_740_992.0;

fn serialize_seconds<S: Serializer>(seconds: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    let value = *seconds;
    if value.is_finite() && value.fract() == 0.0 && value.abs() <= MAX_EXACT_F64_INT {
        // Exact by the guard above: whole and within the exactly-representable range.
        #[allow(clippy::cast_possible_truncation)]
        serializer.serialize_i64(value as i64)
    } else {
        serializer.serialize_f64(value)
    }
}
