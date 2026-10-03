//! The orchestrator's observable state (`Orchestrator.snapshot/2`, consumed by the HTTP API — E.2.3,
//! E.2.4 — and the terminal dashboard — E.5).
//!
//! Field names follow the Elixir snapshot maps. Lists are sorted deterministically (running and blocked
//! by `issue_identifier`, retrying by `due_in_ms`; Elixir iterated maps in unspecified order).
//! Datetimes serialize as RFC 3339 UTC; presenters truncate to seconds where the API requires it.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use symphony_codex::CodexEventKind;
use symphony_core::workspace_key;

/// `last_codex_message`: `%{event, message: payload || raw, timestamp}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexMessage {
    /// Event name.
    pub event: CodexEventKind,
    /// Decoded JSON payload, or the raw line as a JSON string; `None` for session-level events.
    pub message: Option<Value>,
    /// When the event was emitted.
    pub timestamp: DateTime<Utc>,
}

impl CodexMessage {
    /// `codex_message_method/1`: `message.method` when it is a string.
    pub fn method(&self) -> Option<&str> {
        self.message.as_ref()?.get("method")?.as_str()
    }
}

/// One running agent (`snapshot.running[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningSnapshot {
    /// Tracker id.
    pub issue_id: String,
    /// Human identifier.
    pub identifier: String,
    /// Issue URL.
    pub issue_url: Option<String>,
    /// Tracker state as last refreshed.
    pub state: Option<String>,
    /// SSH worker host (`None` = local).
    pub worker_host: Option<String>,
    /// Workspace path, once reported by the worker.
    pub workspace_path: Option<String>,
    /// `"<thread>-<turn>"` of the latest turn.
    pub session_id: Option<String>,
    /// OS pid of the app-server (or local ssh) process.
    pub codex_app_server_pid: Option<String>,
    /// Input tokens accumulated by this run.
    pub codex_input_tokens: u64,
    /// Output tokens accumulated by this run.
    pub codex_output_tokens: u64,
    /// Total tokens accumulated by this run.
    pub codex_total_tokens: u64,
    /// Turns started in this run.
    pub turn_count: u32,
    /// Retry attempt of this run (0 = first dispatch).
    pub retry_attempt: u32,
    /// Dispatch time.
    pub started_at: DateTime<Utc>,
    /// Timestamp of the last Codex event.
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    /// Last Codex event (payload included).
    pub last_codex_message: Option<CodexMessage>,
    /// Last Codex event name.
    pub last_codex_event: Option<CodexEventKind>,
    /// Whole seconds since dispatch at snapshot time.
    pub runtime_seconds: u64,
}

/// One queued retry (`snapshot.retrying[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrySnapshot {
    /// Tracker id.
    pub issue_id: String,
    /// Human identifier (falls back to the id).
    pub identifier: String,
    /// Attempt number (>= 1).
    pub attempt: u32,
    /// Milliseconds until the retry fires (0 when overdue).
    pub due_in_ms: u64,
    /// Wall-clock due time (`generated_at + due_in_ms`).
    pub due_at: DateTime<Utc>,
    /// Issue URL.
    pub issue_url: Option<String>,
    /// Why the retry was scheduled.
    pub error: Option<String>,
    /// Preferred worker host.
    pub worker_host: Option<String>,
    /// Workspace recorded by the previous run.
    pub workspace_path: Option<String>,
}

/// One blocked issue (`snapshot.blocked[]`; not in SPEC).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockedSnapshot {
    /// Tracker id.
    pub issue_id: String,
    /// Human identifier.
    pub identifier: String,
    /// Issue URL.
    pub issue_url: Option<String>,
    /// Tracker state as last refreshed.
    pub state: Option<String>,
    /// Worker host of the blocked run.
    pub worker_host: Option<String>,
    /// Workspace of the blocked run.
    pub workspace_path: Option<String>,
    /// Session of the blocked run (Elixir rendered a missing one as `"n/a"`).
    pub session_id: Option<String>,
    /// Blocker text (`codex turn requires operator input`, ...).
    pub error: String,
    /// When the issue was blocked.
    pub blocked_at: DateTime<Utc>,
    /// Timestamp of the last Codex event.
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    /// Last Codex event.
    pub last_codex_message: Option<CodexMessage>,
    /// Last Codex event name.
    pub last_codex_event: Option<CodexEventKind>,
}

/// Aggregate Codex usage (`codex_totals`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexTotals {
    /// Input tokens (all runs of this runtime process).
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Total tokens.
    pub total_tokens: u64,
    /// Runtime of **ended** sessions only (Elixir parity, B.14 D6); see
    /// [`Snapshot::total_runtime_seconds`] for a live total.
    pub seconds_running: u64,
}

/// Poll loop status (`polling`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollingStatus {
    /// A poll cycle is in progress (Elixir key `checking?`).
    pub checking: bool,
    /// Milliseconds until the next poll; `None` while checking.
    pub next_poll_in_ms: Option<u64>,
    /// Configured interval.
    pub poll_interval_ms: u64,
}

/// Tracker facts the dashboards display.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackerInfo {
    /// `tracker.kind`.
    pub kind: Option<String>,
    /// `tracker.project_slug` (Linear project link).
    pub project_slug: Option<String>,
}

/// The full orchestrator snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// When the snapshot was taken.
    pub generated_at: DateTime<Utc>,
    /// State generation at snapshot time (see [`crate::RuntimeHandle::subscribe`]).
    pub generation: u64,
    /// Running agents, sorted by identifier.
    pub running: Vec<RunningSnapshot>,
    /// Retry queue, sorted by `due_in_ms`.
    pub retrying: Vec<RetrySnapshot>,
    /// Blocked issues, sorted by identifier.
    pub blocked: Vec<BlockedSnapshot>,
    /// Token and runtime totals.
    pub codex_totals: CodexTotals,
    /// Latest rate-limit map seen in any Codex event.
    pub rate_limits: Option<Value>,
    /// Poll status.
    pub polling: PollingStatus,
    /// `agent.max_concurrent_agents` (dashboard `Agents: n/max`).
    pub max_concurrent_agents: u32,
    /// `workspace.root` as configured (raw; used for the per-issue workspace fallback).
    pub workspace_root: String,
    /// Tracker kind and project.
    pub tracker: TrackerInfo,
}

/// Where an issue currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueStatus {
    /// In `running`.
    Running,
    /// In `retrying`.
    Retrying,
    /// In `blocked`.
    Blocked,
}

/// Per-issue view for `GET /api/v1/:issue_identifier` (E.2.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IssueSnapshot {
    /// The looked-up identifier.
    pub identifier: String,
    /// First non-empty issue id of running / retry / blocked.
    pub issue_id: Option<String>,
    /// Running wins over retrying, which wins over blocked.
    pub status: IssueStatus,
    /// First recorded workspace path, else `<workspace.root>/<workspace_key(identifier)>`.
    pub workspace_path: String,
    /// First recorded worker host.
    pub worker_host: Option<String>,
    /// `retry.attempt`, or 0.
    pub current_retry_attempt: u32,
    /// `max(current_retry_attempt - 1, 0)`.
    pub restart_count: u32,
    /// `blocked.error || retry.error`.
    pub last_error: Option<String>,
    /// The running row, if any.
    pub running: Option<RunningSnapshot>,
    /// The retry row, if any.
    pub retry: Option<RetrySnapshot>,
    /// The blocked row, if any.
    pub blocked: Option<BlockedSnapshot>,
}

impl Snapshot {
    /// Counts `(running, retrying, blocked)`.
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.running.len(), self.retrying.len(), self.blocked.len())
    }

    /// `codex_totals.seconds_running` plus the live runtime of every running session (what the
    /// LiveView showed as "Runtime").
    pub fn total_runtime_seconds(&self) -> u64 {
        self.running
            .iter()
            .fold(self.codex_totals.seconds_running, |acc, row| {
                acc.saturating_add(row.runtime_seconds)
            })
    }

    /// `Presenter.issue_payload/3` lookup: the first entry whose identifier matches exactly
    /// (case-sensitive) in running, retrying and blocked.
    pub fn issue(&self, identifier: &str) -> Option<IssueSnapshot> {
        let running = self
            .running
            .iter()
            .find(|r| r.identifier == identifier)
            .cloned();
        let retry = self
            .retrying
            .iter()
            .find(|r| r.identifier == identifier)
            .cloned();
        let blocked = self
            .blocked
            .iter()
            .find(|r| r.identifier == identifier)
            .cloned();
        let status = if running.is_some() {
            IssueStatus::Running
        } else if retry.is_some() {
            IssueStatus::Retrying
        } else if blocked.is_some() {
            IssueStatus::Blocked
        } else {
            return None;
        };
        let issue_id = running
            .as_ref()
            .map(|r| r.issue_id.clone())
            .or_else(|| retry.as_ref().map(|r| r.issue_id.clone()))
            .or_else(|| blocked.as_ref().map(|r| r.issue_id.clone()));
        let workspace_path = running
            .as_ref()
            .and_then(|r| r.workspace_path.clone())
            .or_else(|| retry.as_ref().and_then(|r| r.workspace_path.clone()))
            .or_else(|| blocked.as_ref().and_then(|r| r.workspace_path.clone()))
            .unwrap_or_else(|| {
                let root = self.workspace_root.trim_end_matches('/');
                format!("{root}/{}", workspace_key(Some(identifier)))
            });
        let worker_host = running
            .as_ref()
            .and_then(|r| r.worker_host.clone())
            .or_else(|| retry.as_ref().and_then(|r| r.worker_host.clone()))
            .or_else(|| blocked.as_ref().and_then(|r| r.worker_host.clone()));
        let current_retry_attempt = retry.as_ref().map_or(0, |r| r.attempt);
        let last_error = blocked
            .as_ref()
            .map(|b| b.error.clone())
            .or_else(|| retry.as_ref().and_then(|r| r.error.clone()));
        Some(IssueSnapshot {
            identifier: identifier.to_owned(),
            issue_id,
            status,
            workspace_path,
            worker_host,
            current_retry_attempt,
            restart_count: current_retry_attempt.saturating_sub(1),
            last_error,
            running,
            retry,
            blocked,
        })
    }
}

/// Reply to a manual refresh (`POST /api/v1/refresh`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefreshAck {
    /// Always `true`.
    pub queued: bool,
    /// `true` when a poll was already running or due (no new tick was scheduled).
    pub coalesced: bool,
    /// Request time; serialized with microsecond precision (`2026-02-24T20:15:30.123456Z`).
    #[serde(serialize_with = "micros")]
    pub requested_at: DateTime<Utc>,
    /// Always `["poll", "reconcile"]`.
    pub operations: Vec<String>,
}

fn micros<S: Serializer>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&value.to_rfc3339_opts(SecondsFormat::Micros, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        let now = DateTime::parse_from_rfc3339("2026-02-24T20:15:30Z")
            .unwrap()
            .with_timezone(&Utc);
        Snapshot {
            generated_at: now,
            generation: 1,
            running: vec![],
            retrying: vec![RetrySnapshot {
                issue_id: "issue-retry".into(),
                identifier: "MT-RETRY".into(),
                attempt: 2,
                due_in_ms: 2_000,
                due_at: now,
                issue_url: None,
                error: Some("boom".into()),
                worker_host: None,
                workspace_path: None,
            }],
            blocked: vec![],
            codex_totals: CodexTotals::default(),
            rate_limits: None,
            polling: PollingStatus {
                checking: false,
                next_poll_in_ms: Some(1),
                poll_interval_ms: 30_000,
            },
            max_concurrent_agents: 10,
            workspace_root: "/tmp/symphony_workspaces/".into(),
            tracker: TrackerInfo::default(),
        }
    }

    #[test]
    fn issue_lookup_derives_attempts_errors_and_workspace_fallback() {
        let snap = snapshot();
        let issue = snap.issue("MT-RETRY").unwrap();
        assert_eq!(issue.status, IssueStatus::Retrying);
        assert_eq!(issue.current_retry_attempt, 2);
        assert_eq!(issue.restart_count, 1);
        assert_eq!(issue.last_error.as_deref(), Some("boom"));
        assert_eq!(issue.workspace_path, "/tmp/symphony_workspaces/MT-RETRY");
        assert!(snap.issue("mt-retry").is_none());
    }

    #[test]
    fn refresh_ack_serializes_microseconds() {
        let ack = RefreshAck {
            queued: true,
            coalesced: false,
            requested_at: DateTime::parse_from_rfc3339("2026-02-24T20:15:30.123Z")
                .unwrap()
                .with_timezone(&Utc),
            operations: vec!["poll".into(), "reconcile".into()],
        };
        assert_eq!(
            serde_json::to_value(&ack).unwrap()["requested_at"],
            "2026-02-24T20:15:30.123000Z"
        );
    }
}
