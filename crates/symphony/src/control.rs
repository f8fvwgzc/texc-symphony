//! [`symphony_server::ControlPlane`] over the runtime's [`RuntimeHandle`] (the binary is the only
//! crate that knows both; the server never depends on the runtime).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use symphony_core::WorkflowStore;
use symphony_runtime::{
    BlockedSnapshot, RefreshError, RetrySnapshot, RunningSnapshot, RuntimeHandle, Snapshot,
    SnapshotError, humanize_codex_message,
};
use symphony_server::{
    BlockedEntry, CodexTotals, ControlPlane, RefreshAccepted, RetryEntry, RunningEntry, StateError,
    StateView, TokenCounts, Unavailable, presenter,
};
use tokio::sync::watch;

/// Serves the HTTP API from the live orchestrator.
#[derive(Debug, Clone)]
pub struct RuntimeControlPlane {
    handle: RuntimeHandle,
    workflow: Arc<WorkflowStore>,
}

impl RuntimeControlPlane {
    /// Adapter over `handle`, reading `workspace.root` from `workflow`.
    pub fn new(handle: RuntimeHandle, workflow: Arc<WorkflowStore>) -> Self {
        Self { handle, workflow }
    }
}

fn running_entry(row: &RunningSnapshot) -> RunningEntry {
    RunningEntry {
        issue_id: row.issue_id.clone(),
        issue_identifier: row.identifier.clone(),
        issue_url: row.issue_url.clone(),
        state: row.state.clone(),
        worker_host: row.worker_host.clone(),
        workspace_path: row.workspace_path.clone(),
        session_id: row.session_id.clone(),
        turn_count: row.turn_count,
        last_event: row.last_codex_event.map(|e| e.as_str().to_owned()),
        last_message: row
            .last_codex_message
            .as_ref()
            .map(|m| humanize_codex_message(Some(m))),
        started_at: Some(row.started_at),
        last_event_at: row.last_codex_timestamp,
        tokens: TokenCounts {
            input_tokens: row.codex_input_tokens,
            output_tokens: row.codex_output_tokens,
            total_tokens: row.codex_total_tokens,
        },
    }
}

fn retry_entry(row: &RetrySnapshot, now: DateTime<Utc>) -> RetryEntry {
    RetryEntry {
        issue_id: row.issue_id.clone(),
        issue_identifier: row.identifier.clone(),
        issue_url: row.issue_url.clone(),
        attempt: Some(row.attempt),
        due_at: presenter::due_at(now, Some(row.due_in_ms)),
        error: row.error.clone(),
        worker_host: row.worker_host.clone(),
        workspace_path: row.workspace_path.clone(),
    }
}

fn blocked_entry(row: &BlockedSnapshot) -> BlockedEntry {
    BlockedEntry {
        issue_id: row.issue_id.clone(),
        issue_identifier: row.identifier.clone(),
        issue_url: row.issue_url.clone(),
        state: row.state.clone(),
        error: Some(row.error.clone()),
        worker_host: row.worker_host.clone(),
        workspace_path: row.workspace_path.clone(),
        session_id: row.session_id.clone(),
        blocked_at: Some(row.blocked_at),
        last_event: row.last_codex_event.map(|e| e.as_str().to_owned()),
        last_message: row
            .last_codex_message
            .as_ref()
            .map(|m| humanize_codex_message(Some(m))),
        last_event_at: row.last_codex_timestamp,
    }
}

/// Projects a runtime [`Snapshot`] onto the API's [`StateView`] (`due_at` relative to `now`).
pub fn state_view(snapshot: &Snapshot, now: DateTime<Utc>) -> StateView {
    StateView {
        running: snapshot.running.iter().map(running_entry).collect(),
        retrying: snapshot
            .retrying
            .iter()
            .map(|row| retry_entry(row, now))
            .collect(),
        blocked: snapshot.blocked.iter().map(blocked_entry).collect(),
        codex_totals: CodexTotals {
            input_tokens: snapshot.codex_totals.input_tokens,
            output_tokens: snapshot.codex_totals.output_tokens,
            total_tokens: snapshot.codex_totals.total_tokens,
            // Exact for any realistic runtime (< 2^53 s).
            seconds_running: snapshot.codex_totals.seconds_running as f64,
        },
        rate_limits: snapshot.rate_limits.clone(),
    }
}

#[async_trait]
impl ControlPlane for RuntimeControlPlane {
    async fn state(&self) -> Result<StateView, StateError> {
        match self.handle.snapshot().await {
            Ok(snapshot) => Ok(state_view(&snapshot, Utc::now())),
            Err(SnapshotError::Timeout) => Err(StateError::Timeout),
            Err(SnapshotError::Unavailable) => Err(StateError::Unavailable),
        }
    }

    async fn refresh(&self) -> Result<RefreshAccepted, Unavailable> {
        match self.handle.request_refresh().await {
            Ok(ack) => Ok(RefreshAccepted {
                queued: ack.queued,
                coalesced: ack.coalesced,
                requested_at: ack.requested_at,
                operations: ack.operations,
            }),
            Err(RefreshError::Unavailable | RefreshError::Timeout) => Err(Unavailable),
        }
    }

    fn workspace_root(&self) -> String {
        self.workflow.settings().workspace.root.clone()
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.handle.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use symphony_codex::CodexEventKind;
    use symphony_runtime::{CodexMessage, CodexTotals as RtTotals, PollingStatus, TrackerInfo};

    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn snapshot() -> Snapshot {
        let now = at("2026-02-24T20:15:30Z");
        Snapshot {
            generated_at: now,
            generation: 7,
            running: vec![RunningSnapshot {
                issue_id: "issue-http".into(),
                identifier: "MT-HTTP".into(),
                issue_url: Some("https://example.org/issues/MT-HTTP".into()),
                state: Some("In Progress".into()),
                worker_host: None,
                workspace_path: Some("/tmp/ws/MT-HTTP".into()),
                session_id: Some("thread-http".into()),
                codex_app_server_pid: Some("4242".into()),
                codex_input_tokens: 4,
                codex_output_tokens: 8,
                codex_total_tokens: 12,
                turn_count: 7,
                retry_attempt: 0,
                started_at: now,
                last_codex_timestamp: Some(now),
                last_codex_message: Some(CodexMessage {
                    event: CodexEventKind::Notification,
                    message: Some(
                        json!({"method": "turn/completed", "params": {"turn": {"status": "completed"}}}),
                    ),
                    timestamp: now,
                }),
                last_codex_event: Some(CodexEventKind::Notification),
                runtime_seconds: 3,
            }],
            retrying: vec![RetrySnapshot {
                issue_id: "issue-retry".into(),
                identifier: "MT-RETRY".into(),
                attempt: 2,
                due_in_ms: 2_500,
                due_at: now,
                issue_url: None,
                error: Some("boom".into()),
                worker_host: None,
                workspace_path: None,
            }],
            blocked: vec![BlockedSnapshot {
                issue_id: "issue-blocked".into(),
                identifier: "MT-BLOCKED".into(),
                issue_url: None,
                state: Some("In Progress".into()),
                worker_host: Some("worker-1".into()),
                workspace_path: None,
                session_id: None,
                error: "codex turn requires operator input".into(),
                blocked_at: now,
                last_codex_timestamp: None,
                last_codex_message: Some(CodexMessage {
                    event: CodexEventKind::TurnInputRequired,
                    message: None,
                    timestamp: now,
                }),
                last_codex_event: Some(CodexEventKind::TurnInputRequired),
            }],
            codex_totals: RtTotals {
                input_tokens: 4,
                output_tokens: 8,
                total_tokens: 12,
                seconds_running: 42,
            },
            rate_limits: Some(json!({"limit_id": "codex"})),
            polling: PollingStatus {
                checking: false,
                next_poll_in_ms: Some(1_000),
                poll_interval_ms: 30_000,
            },
            max_concurrent_agents: 10,
            workspace_root: "/tmp/ws".into(),
            tracker: TrackerInfo::default(),
        }
    }

    #[test]
    fn maps_every_list_with_humanized_messages() {
        let view = state_view(&snapshot(), at("2026-02-24T20:15:30.900Z"));
        let running = &view.running[0];
        assert_eq!(running.issue_identifier, "MT-HTTP");
        assert_eq!(running.last_event.as_deref(), Some("notification"));
        assert_eq!(
            running.last_message.as_deref(),
            Some("turn completed (completed)")
        );
        assert_eq!(running.tokens.total_tokens, 12);
        let retry = &view.retrying[0];
        assert_eq!(retry.attempt, Some(2));
        assert_eq!(retry.due_at, Some(at("2026-02-24T20:15:32Z")));
        let blocked = &view.blocked[0];
        assert_eq!(
            blocked.error.as_deref(),
            Some("codex turn requires operator input")
        );
        assert_eq!(
            blocked.last_message.as_deref(),
            Some("turn blocked: waiting for user input")
        );
        assert_eq!(view.codex_totals.seconds_running, 42.0);
        assert_eq!(view.rate_limits, Some(json!({"limit_id": "codex"})));
    }

    #[tokio::test]
    async fn adapter_serves_snapshots_refreshes_and_reports_unavailability() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        std::fs::write(
            &path,
            "---\ntracker:\n  kind: memory\nworkspace:\n  root: /srv/ws\n---\nprompt\n",
        )
        .unwrap();
        let workflow = WorkflowStore::start(Some(path)).unwrap();
        let control = RuntimeControlPlane::new(
            RuntimeHandle::with_static_snapshot(snapshot()),
            Arc::clone(&workflow),
        );
        let view = control.state().await.unwrap();
        assert_eq!(view.running.len(), 1);
        let ack = control.refresh().await.unwrap();
        assert!(ack.queued);
        assert_eq!(
            ack.operations,
            vec!["poll".to_owned(), "reconcile".to_owned()]
        );
        assert_eq!(control.workspace_root(), "/srv/ws");
        assert_eq!(*control.changes().borrow(), 7);
        let issue = control.issue("MT-RETRY").await.unwrap();
        assert_eq!(issue.issue_id, "issue-retry");

        let down = RuntimeControlPlane::new(RuntimeHandle::unavailable(), workflow);
        assert_eq!(down.state().await, Err(StateError::Unavailable));
        assert_eq!(down.refresh().await, Err(Unavailable));
        assert!(down.issue("MT-RETRY").await.is_none());
    }
}
