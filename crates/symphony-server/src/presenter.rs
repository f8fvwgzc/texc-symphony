//! Pure projections shared by the JSON API and SSE (Elixir `SymphonyElixirWeb.Presenter`).
//!
//! Everything here is deterministic given its inputs (`now` is always a parameter), so the CLI
//! adapter and the tests can reuse it.

use std::path::Path;

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::Map;

use crate::control::StateError;
use crate::view::{
    Counts, ErrorBody, IssueAttempts, IssueBlocked, IssueLogs, IssueRetry, IssueRunning,
    IssueStatus, IssueView, IssueWorkspace, RecentEvent, StateFailure, StatePayload, StateSnapshot,
    StateView,
};

/// RFC 3339, truncated to whole seconds, `Z` suffix: `2026-02-24T20:15:30Z`
/// (Elixir `DateTime.truncate(:second) |> DateTime.to_iso8601()`).
pub fn format_timestamp(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// RFC 3339 with microseconds and `Z`: `2026-02-24T20:15:30.123456Z` (refresh `requested_at`).
pub fn format_precise_timestamp(at: &DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// `at` without its sub-second part.
pub fn truncate_to_seconds(at: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp(at.timestamp(), 0).unwrap_or(at)
}

/// Retry `due_at` from the orchestrator's `due_in_ms`: `now + div(due_in_ms, 1000)` seconds,
/// truncated to seconds. `None` stays `None` (Elixir: non-integer `due_in_ms` gives `null`).
/// Recomputed per request, so it may jitter by one second between calls (Elixir parity).
pub fn due_at(now: DateTime<Utc>, due_in_ms: Option<u64>) -> Option<DateTime<Utc>> {
    let seconds = i64::try_from(due_in_ms? / 1000).ok()?;
    let at = now.checked_add_signed(TimeDelta::try_seconds(seconds)?)?;
    Some(truncate_to_seconds(at))
}

/// Sort every list of `view` by `issue_id` (stable), the order Elixir produced.
pub fn sort_view(view: &mut StateView) {
    view.running.sort_by(|a, b| a.issue_id.cmp(&b.issue_id));
    view.retrying.sort_by(|a, b| a.issue_id.cmp(&b.issue_id));
    view.blocked.sort_by(|a, b| a.issue_id.cmp(&b.issue_id));
}

/// `counts` of `view`.
pub fn counts(view: &StateView) -> Counts {
    Counts {
        running: view.running.len(),
        retrying: view.retrying.len(),
        blocked: view.blocked.len(),
    }
}

/// The body of `GET /api/v1/state` for a snapshot outcome taken at `generated_at`
/// (lists sorted, counts computed, timestamp truncated).
pub fn state_payload(
    generated_at: DateTime<Utc>,
    outcome: Result<StateView, StateError>,
) -> StatePayload {
    let generated_at = truncate_to_seconds(generated_at);
    match outcome {
        Ok(mut view) => {
            sort_view(&mut view);
            StatePayload::Snapshot(StateSnapshot {
                generated_at,
                counts: counts(&view),
                view,
            })
        }
        Err(error) => StatePayload::Failure(StateFailure {
            generated_at,
            error: ErrorBody {
                code: error.code().to_owned(),
                message: error.message().to_owned(),
            },
        }),
    }
}

/// Fallback workspace path: `Path.join(workspace_root, workspace_key(identifier))`. Uses the
/// configured `workspace.root` verbatim (not the expanded root), like Elixir.
pub fn workspace_fallback_path(workspace_root: &str, identifier: &str) -> String {
    Path::new(workspace_root)
        .join(symphony_core::workspace_key(Some(identifier)))
        .to_string_lossy()
        .into_owned()
}

/// Detail of one issue (Elixir `Presenter.issue_payload`): looks `identifier` up (exact,
/// case-sensitive) in `running`, `retrying` and `blocked`; `None` when it is in none of them.
pub fn issue_view(identifier: &str, view: &StateView, workspace_root: &str) -> Option<IssueView> {
    let running = view
        .running
        .iter()
        .find(|entry| entry.issue_identifier == identifier);
    let retry = view
        .retrying
        .iter()
        .find(|entry| entry.issue_identifier == identifier);
    let blocked = view
        .blocked
        .iter()
        .find(|entry| entry.issue_identifier == identifier);

    let (issue_id, status) = match (running, retry, blocked) {
        (Some(entry), _, _) => (entry.issue_id.clone(), IssueStatus::Running),
        (None, Some(entry), _) => (entry.issue_id.clone(), IssueStatus::Retrying),
        (None, None, Some(entry)) => (entry.issue_id.clone(), IssueStatus::Blocked),
        (None, None, None) => return None,
    };

    let workspace_path = running
        .and_then(|entry| entry.workspace_path.clone())
        .or_else(|| retry.and_then(|entry| entry.workspace_path.clone()))
        .or_else(|| blocked.and_then(|entry| entry.workspace_path.clone()))
        .unwrap_or_else(|| workspace_fallback_path(workspace_root, identifier));
    let host = running
        .and_then(|entry| entry.worker_host.clone())
        .or_else(|| retry.and_then(|entry| entry.worker_host.clone()))
        .or_else(|| blocked.and_then(|entry| entry.worker_host.clone()));

    let current_retry_attempt = retry.and_then(|entry| entry.attempt).unwrap_or(0);

    // Elixir: `recent_events_payload(running || blocked)`, dropped when its timestamp is nil.
    let recent_event = match (running, blocked) {
        (Some(entry), _) => entry.last_event_at.map(|at| RecentEvent {
            at,
            event: entry.last_event.clone(),
            message: entry.last_message.clone(),
        }),
        (None, Some(entry)) => entry.last_event_at.map(|at| RecentEvent {
            at,
            event: entry.last_event.clone(),
            message: entry.last_message.clone(),
        }),
        (None, None) => None,
    };

    let last_error = blocked
        .and_then(|entry| entry.error.clone())
        .or_else(|| retry.and_then(|entry| entry.error.clone()));

    Some(IssueView {
        issue_identifier: identifier.to_owned(),
        issue_id,
        status,
        workspace: IssueWorkspace {
            path: workspace_path,
            host,
        },
        attempts: IssueAttempts {
            restart_count: current_retry_attempt.saturating_sub(1),
            current_retry_attempt,
        },
        running: running.map(|entry| IssueRunning {
            worker_host: entry.worker_host.clone(),
            workspace_path: entry.workspace_path.clone(),
            session_id: entry.session_id.clone(),
            turn_count: entry.turn_count,
            state: entry.state.clone(),
            started_at: entry.started_at,
            last_event: entry.last_event.clone(),
            last_message: entry.last_message.clone(),
            last_event_at: entry.last_event_at,
            tokens: entry.tokens,
        }),
        retry: retry.map(|entry| IssueRetry {
            attempt: entry.attempt,
            due_at: entry.due_at,
            error: entry.error.clone(),
            worker_host: entry.worker_host.clone(),
            workspace_path: entry.workspace_path.clone(),
        }),
        blocked: blocked.map(|entry| IssueBlocked {
            worker_host: entry.worker_host.clone(),
            workspace_path: entry.workspace_path.clone(),
            session_id: entry.session_id.clone(),
            state: entry.state.clone(),
            error: entry.error.clone(),
            blocked_at: entry.blocked_at,
            last_event: entry.last_event.clone(),
            last_message: entry.last_message.clone(),
            last_event_at: entry.last_event_at,
        }),
        logs: IssueLogs::default(),
        recent_events: recent_event.into_iter().collect(),
        last_error,
        tracked: Map::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{BlockedEntry, CodexTotals, RetryEntry, RunningEntry, TokenCounts};
    use serde_json::json;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn running(id: &str, identifier: &str) -> RunningEntry {
        RunningEntry {
            issue_id: id.into(),
            issue_identifier: identifier.into(),
            issue_url: None,
            state: Some("In Progress".into()),
            worker_host: None,
            workspace_path: None,
            session_id: Some("thread-1".into()),
            turn_count: 3,
            last_event: Some("notification".into()),
            last_message: Some("rendered".into()),
            started_at: Some(at("2026-02-24T20:10:12.987654Z")),
            last_event_at: None,
            tokens: TokenCounts::default(),
        }
    }

    fn retry(id: &str, identifier: &str, attempt: Option<u32>) -> RetryEntry {
        RetryEntry {
            issue_id: id.into(),
            issue_identifier: identifier.into(),
            issue_url: None,
            attempt,
            due_at: None,
            error: Some("boom".into()),
            worker_host: Some("retry-host".into()),
            workspace_path: Some("/w/retry".into()),
        }
    }

    fn blocked(id: &str, identifier: &str) -> BlockedEntry {
        BlockedEntry {
            issue_id: id.into(),
            issue_identifier: identifier.into(),
            issue_url: None,
            state: None,
            error: Some("codex turn requires operator input".into()),
            worker_host: Some("blocked-host".into()),
            workspace_path: Some("/w/blocked".into()),
            session_id: Some("thread-b".into()),
            blocked_at: Some(at("2026-02-24T20:14:00.5Z")),
            last_event: Some("turn_input_required".into()),
            last_message: Some("turn blocked: waiting for user input".into()),
            last_event_at: Some(at("2026-02-24T20:14:00.9Z")),
        }
    }

    #[test]
    fn timestamps_are_second_truncated_except_requested_at() {
        let t = at("2026-02-15T21:36:38.987654Z");
        assert_eq!(format_timestamp(&t), "2026-02-15T21:36:38Z");
        assert_eq!(format_precise_timestamp(&t), "2026-02-15T21:36:38.987654Z");
        assert_eq!(
            format_timestamp(&truncate_to_seconds(t)),
            "2026-02-15T21:36:38Z"
        );
        assert_eq!(truncate_to_seconds(t).timestamp_subsec_nanos(), 0);
    }

    #[test]
    fn due_at_adds_whole_seconds_of_due_in_ms() {
        let now = at("2026-02-24T20:15:30.700Z");
        assert_eq!(
            due_at(now, Some(2_999)).map(|t| format_timestamp(&t)),
            Some("2026-02-24T20:15:32Z".into())
        );
        assert_eq!(due_at(now, Some(0)), Some(at("2026-02-24T20:15:30Z")));
        assert_eq!(due_at(now, None), None);
        assert_eq!(due_at(now, Some(u64::MAX)), None);
    }

    #[test]
    fn state_payload_sorts_counts_and_reports_errors() {
        let view = StateView {
            running: vec![running("b", "MT-B"), running("a", "MT-A")],
            retrying: vec![retry("z", "MT-Z", Some(1)), retry("c", "MT-C", None)],
            blocked: vec![],
            codex_totals: CodexTotals::default(),
            rate_limits: None,
        };
        let now = at("2026-02-24T20:15:30.5Z");
        let payload = serde_json::to_value(state_payload(now, Ok(view))).unwrap();
        assert_eq!(payload["generated_at"], "2026-02-24T20:15:30Z");
        assert_eq!(
            payload["counts"],
            json!({"running": 2, "retrying": 2, "blocked": 0})
        );
        assert_eq!(payload["running"][0]["issue_id"], "a");
        assert_eq!(payload["retrying"][0]["issue_id"], "c");
        assert_eq!(payload["running"][0]["started_at"], "2026-02-24T20:10:12Z");
        assert_eq!(payload["rate_limits"], serde_json::Value::Null);

        for (error, code, message) in [
            (
                StateError::Timeout,
                "snapshot_timeout",
                "Snapshot timed out",
            ),
            (
                StateError::Unavailable,
                "snapshot_unavailable",
                "Snapshot unavailable",
            ),
        ] {
            let payload = serde_json::to_value(state_payload(now, Err(error))).unwrap();
            assert_eq!(
                payload,
                json!({"generated_at": "2026-02-24T20:15:30Z", "error": {"code": code, "message": message}})
            );
        }
    }

    #[test]
    fn seconds_running_keeps_integers_integral() {
        let mut totals = CodexTotals {
            seconds_running: 42.0,
            ..CodexTotals::default()
        };
        assert_eq!(
            serde_json::to_value(totals).unwrap()["seconds_running"],
            json!(42)
        );
        assert_eq!(
            serde_json::to_string(&totals).unwrap(),
            r#"{"input_tokens":0,"output_tokens":0,"total_tokens":0,"seconds_running":42}"#
        );
        totals.seconds_running = 42.5;
        assert_eq!(
            serde_json::to_value(totals).unwrap()["seconds_running"],
            json!(42.5)
        );
    }

    #[test]
    fn issue_view_prefers_running_then_retry_then_blocked() {
        let view = StateView {
            running: vec![running("issue-1", "MT-1")],
            retrying: vec![
                retry("issue-1", "MT-1", Some(3)),
                retry("issue-2", "MT-2", Some(2)),
            ],
            blocked: vec![blocked("issue-1", "MT-1"), blocked("issue-3", "MT-3")],
            ..StateView::default()
        };

        let both = issue_view("MT-1", &view, "/root").unwrap();
        assert_eq!(both.status, IssueStatus::Running);
        assert_eq!(both.issue_id, "issue-1");
        // running has no path/host, so retry's win (first non-nil).
        assert_eq!(both.workspace.path, "/w/retry");
        assert_eq!(both.workspace.host.as_deref(), Some("retry-host"));
        assert_eq!(
            both.attempts,
            IssueAttempts {
                restart_count: 2,
                current_retry_attempt: 3
            }
        );
        // blocked error wins over retry error.
        assert_eq!(
            both.last_error.as_deref(),
            Some("codex turn requires operator input")
        );
        // running wins for recent_events and its last_event_at is nil, so nothing is reported.
        assert!(both.recent_events.is_empty());

        let retrying = issue_view("MT-2", &view, "/root").unwrap();
        assert_eq!(retrying.status, IssueStatus::Retrying);
        assert_eq!(retrying.last_error.as_deref(), Some("boom"));
        assert_eq!(retrying.attempts.restart_count, 1);
        assert!(retrying.running.is_none() && retrying.blocked.is_none());

        let blocked_only = issue_view("MT-3", &view, "/root").unwrap();
        assert_eq!(blocked_only.status, IssueStatus::Blocked);
        assert_eq!(blocked_only.attempts, IssueAttempts::default());
        assert_eq!(blocked_only.recent_events.len(), 1);
        let event = serde_json::to_value(&blocked_only.recent_events[0]).unwrap();
        assert_eq!(
            event,
            json!({"at": "2026-02-24T20:14:00Z", "event": "turn_input_required", "message": "turn blocked: waiting for user input"})
        );

        assert!(issue_view("MT-404", &view, "/root").is_none());
        assert!(
            issue_view("mt-1", &view, "/root").is_none(),
            "case-sensitive"
        );
    }

    #[test]
    fn issue_view_falls_back_to_workspace_key_path() {
        let view = StateView {
            running: vec![running("issue-1", "MT/42")],
            ..StateView::default()
        };
        let detail = issue_view("MT/42", &view, "/tmp/symphony_workspaces/").unwrap();
        assert_eq!(
            detail.workspace.path,
            format!(
                "/tmp/symphony_workspaces/{}",
                symphony_core::workspace_key(Some("MT/42"))
            )
        );
        assert!(detail.workspace.path.contains("MT_42--"));
        assert_eq!(
            workspace_fallback_path("/tmp/w", "MT-HTTP"),
            "/tmp/w/MT-HTTP"
        );
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["logs"], json!({"codex_session_logs": []}));
        assert_eq!(json["tracked"], json!({}));
        assert_eq!(json["retry"], serde_json::Value::Null);
    }
}
