//! Record types. Their serde field names are the JSON contract used by the HTTP API and web UI.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

/// Default page size for [`RunQuery`].
pub const DEFAULT_RUN_LIMIT: u32 = 50;
/// Maximum page size for [`RunQuery`].
pub const MAX_RUN_LIMIT: u32 = 200;
/// Default page size for `list_events`.
pub const DEFAULT_EVENT_LIMIT: u32 = 200;
/// Maximum page size for `list_events`.
pub const MAX_EVENT_LIMIT: u32 = 1000;
/// Event messages longer than this many bytes are truncated (on a char boundary).
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;
/// Event payloads whose JSON encoding exceeds this many bytes are replaced by a marker object
/// `{"truncated": true, "original_bytes": n}`.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

/// Database id of a run. Serializes as a bare integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub i64);

impl RunId {
    /// Id handed out by a disabled store; never refers to a persisted row.
    pub const DISABLED: RunId = RunId(0);

    /// Raw integer id.
    pub fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<i64> for RunId {
    fn from(id: i64) -> Self {
        RunId(id)
    }
}

/// Lifecycle status of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    /// The agent run is in progress.
    Running,
    /// The run exited normally.
    Succeeded,
    /// The run exited with an error (it will usually be retried).
    Failed,
    /// The run was stopped by Symphony (reconciliation, shutdown or restart).
    Cancelled,
    /// The run stopped because Codex needs operator input or approval.
    Blocked,
}

impl RunStatus {
    /// All statuses, in declaration order.
    pub const ALL: [RunStatus; 5] = [
        RunStatus::Running,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::Cancelled,
        RunStatus::Blocked,
    ];

    /// The lowercase wire/database spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Blocked => "blocked",
        }
    }

    /// `true` for every status except [`RunStatus::Running`].
    pub fn is_terminal(self) -> bool {
        self != RunStatus::Running
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RunStatus {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RunStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| StoreError::InvalidArgument(format!("unknown run status {s:?}")))
    }
}

/// Token counters for a run (cumulative for the run) or an aggregate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Input (prompt) tokens.
    pub input: u64,
    /// Output (completion) tokens.
    pub output: u64,
    /// Total tokens as reported by Codex.
    pub total: u64,
}

/// One queued retry, as kept across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryRecord {
    /// Tracker issue id (the queue holds at most one retry per issue).
    pub issue_id: String,
    /// Attempt number the retry will run as.
    pub attempt: u32,
    /// When the retry is due. Stored with millisecond precision.
    pub due_at: DateTime<Utc>,
    /// Human issue identifier.
    pub identifier: String,
    /// Tracker URL of the issue, if known.
    pub issue_url: Option<String>,
    /// Why the previous attempt ended, if it failed.
    pub error: Option<String>,
    /// SSH worker host of the previous attempt, `None` for local runs.
    pub worker_host: Option<String>,
    /// Workspace directory of the previous attempt, if known.
    pub workspace_path: Option<String>,
}

/// Arguments for `Store::start_run`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRun {
    /// Tracker issue id (required, non-empty).
    pub issue_id: String,
    /// Human identifier such as `MT-123`; falls back to `issue_id` when empty.
    pub issue_identifier: String,
    /// Issue title, if known.
    pub issue_title: Option<String>,
    /// Retry attempt (0 for the first dispatch).
    pub attempt: u32,
    /// SSH worker host, `None` for local runs.
    pub worker_host: Option<String>,
    /// Workspace directory, if already known.
    pub workspace_path: Option<String>,
    /// Start time; `None` means "now". Stored with millisecond precision.
    pub started_at: Option<DateTime<Utc>>,
}

impl NewRun {
    /// A run for `issue_id` / `issue_identifier` with every optional field unset.
    pub fn new(issue_id: impl Into<String>, issue_identifier: impl Into<String>) -> Self {
        NewRun {
            issue_id: issue_id.into(),
            issue_identifier: issue_identifier.into(),
            ..NewRun::default()
        }
    }
}

/// One persisted agent run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Database id (monotonically increasing).
    pub id: RunId,
    /// Tracker issue id.
    pub issue_id: String,
    /// Human issue identifier.
    pub issue_identifier: String,
    /// Issue title, if known.
    pub issue_title: Option<String>,
    /// Retry attempt (0 for the first dispatch).
    pub attempt: u32,
    /// SSH worker host, `None` for local runs.
    pub worker_host: Option<String>,
    /// Workspace directory.
    pub workspace_path: Option<String>,
    /// Current status.
    pub status: RunStatus,
    /// Error text for failed/cancelled/blocked runs.
    pub error: Option<String>,
    /// Number of Codex turns started.
    pub turns: u32,
    /// Start time (millisecond precision).
    pub started_at: DateTime<Utc>,
    /// End time, set by `finish_run` / `mark_interrupted_runs`.
    pub finished_at: Option<DateTime<Utc>>,
    /// `finished_at - started_at` in milliseconds (never negative).
    pub duration_ms: Option<i64>,
    /// Latest cumulative token usage for the run.
    pub tokens: TokenUsage,
}

/// One entry of a run's event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvent {
    /// Run the event belongs to.
    pub run_id: RunId,
    /// Per-run sequence number, starting at 1, gap-free and strictly increasing.
    pub seq: i64,
    /// When the event was submitted (millisecond precision).
    pub at: DateTime<Utc>,
    /// Event kind, e.g. `session_started`, `notification`, `turn_completed`.
    pub kind: String,
    /// Optional human-readable message.
    pub message: Option<String>,
    /// Optional JSON payload.
    pub payload: Option<serde_json::Value>,
}

/// Aggregates over all retained runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TotalsRecord {
    /// Number of runs (any status).
    pub runs_total: u64,
    /// Runs with status `succeeded`.
    pub runs_succeeded: u64,
    /// Runs with status `failed`.
    pub runs_failed: u64,
    /// Sum of token usage over all runs (including running ones).
    pub tokens: TokenUsage,
    /// Sum of `duration_ms` over finished runs.
    pub runtime_ms: u64,
}

/// Filters and cursor for `Store::list_runs`. Deserializable from query strings
/// (`?limit=&before_id=&issue_identifier=&status=`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunQuery {
    /// Page size; default [`DEFAULT_RUN_LIMIT`], clamped to `1..=`[`MAX_RUN_LIMIT`] (0 means default).
    pub limit: Option<u32>,
    /// Only runs with `id < before_id` (pass the previous page's `next_before_id`).
    pub before_id: Option<i64>,
    /// Only runs for this issue identifier (exact match).
    pub issue_identifier: Option<String>,
    /// Only runs with this status.
    pub status: Option<RunStatus>,
}

impl RunQuery {
    /// The page size actually used.
    pub fn effective_limit(&self) -> u32 {
        match self.limit {
            None | Some(0) => DEFAULT_RUN_LIMIT,
            Some(n) => n.min(MAX_RUN_LIMIT),
        }
    }
}

/// One page of runs, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPage {
    /// Runs ordered by descending id.
    pub runs: Vec<RunRecord>,
    /// Cursor for the next (older) page; `None` when this is the last page.
    pub next_before_id: Option<i64>,
}

/// Result of a retention pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneStats {
    /// Runs deleted.
    pub runs_deleted: u64,
    /// Events deleted (belonging to the deleted runs).
    pub events_deleted: u64,
}

/// Clamp an event page size to `1..=MAX_EVENT_LIMIT` (`None`/0 means default).
pub(crate) fn effective_event_limit(limit: Option<u32>) -> u32 {
    match limit {
        None | Some(0) => DEFAULT_EVENT_LIMIT,
        Some(n) => n.min(MAX_EVENT_LIMIT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_through_str_and_json() {
        for status in RunStatus::ALL {
            assert_eq!(status.as_str().parse::<RunStatus>(), Ok(status));
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("\"{}\"", status.as_str()));
        }
        assert!("RUNNING".parse::<RunStatus>().is_err());
    }

    #[test]
    fn run_query_limit_is_clamped() {
        assert_eq!(RunQuery::default().effective_limit(), 50);
        let q = |limit| RunQuery {
            limit: Some(limit),
            ..RunQuery::default()
        };
        assert_eq!(q(0).effective_limit(), 50);
        assert_eq!(q(7).effective_limit(), 7);
        assert_eq!(q(5000).effective_limit(), 200);
        assert_eq!(effective_event_limit(None), 200);
        assert_eq!(effective_event_limit(Some(9999)), 1000);
    }

    #[test]
    fn records_use_contract_field_names() {
        let record = RunRecord {
            id: RunId(7),
            issue_id: "issue-1".into(),
            issue_identifier: "MT-1".into(),
            issue_title: None,
            attempt: 2,
            worker_host: None,
            workspace_path: Some("/w/MT-1".into()),
            status: RunStatus::Blocked,
            error: Some("codex turn requires operator input".into()),
            turns: 3,
            started_at: DateTime::from_timestamp(1_771_964_112, 0).unwrap(),
            finished_at: None,
            duration_ms: None,
            tokens: TokenUsage {
                input: 4,
                output: 8,
                total: 12,
            },
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["id"], 7);
        assert_eq!(value["status"], "blocked");
        assert_eq!(value["started_at"], "2026-02-24T20:15:12Z");
        assert_eq!(
            value["tokens"],
            serde_json::json!({"input": 4, "output": 8, "total": 12})
        );
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        for key in [
            "id",
            "issue_id",
            "issue_identifier",
            "issue_title",
            "attempt",
            "worker_host",
            "workspace_path",
            "status",
            "error",
            "turns",
            "started_at",
            "finished_at",
            "duration_ms",
            "tokens",
        ] {
            assert!(keys.iter().any(|k| *k == key), "missing {key}");
        }
        assert_eq!(keys.len(), 14);

        let query: RunQuery =
            serde_json::from_value(serde_json::json!({"status": "failed", "limit": 10})).unwrap();
        assert_eq!(query.status, Some(RunStatus::Failed));
        assert_eq!(query.before_id, None);
    }
}
