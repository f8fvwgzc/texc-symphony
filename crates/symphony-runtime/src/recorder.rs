//! Store integration: one [`RunRecorder`] per dispatched run writes the run's history to
//! [`symphony_store::Store`] without ever blocking or failing orchestration.
//!
//! Operations are queued to a per-run task that first awaits `start_run` and then applies them in
//! order (fire-and-forget; store failures are logged by the store). Streaming deltas and token-count
//! notifications are throttled to one event per method per [`DELTA_EVENT_INTERVAL`]. A recorder that
//! is dropped without [`RunRecorder::finish`] (orchestrator crash) closes its run as `cancelled`.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use symphony_codex::{CodexEvent, CodexEventKind, TokenCounts};
use symphony_store::{NewRun, RunStatus, Store, TokenUsage};
use tokio::sync::mpsc;
use tokio::time::Instant;

/// Minimum spacing between two recorded events of the same noisy method within one run.
pub const DELTA_EVENT_INTERVAL: Duration = Duration::from_secs(1);

/// Error recorded when a run's recorder disappears without a final status.
pub const RECORDER_DROPPED_ERROR: &str = "interrupted: orchestrator stopped";

#[derive(Debug)]
enum RecordOp {
    Event {
        kind: String,
        message: Option<String>,
        payload: Option<Value>,
    },
    Tokens(TokenUsage),
    Turn,
    RuntimeInfo {
        worker_host: Option<String>,
        workspace_path: Option<String>,
    },
    Finish {
        status: RunStatus,
        error: Option<String>,
    },
}

/// Per-run history writer (a no-op for a disabled store).
#[derive(Debug, Default)]
pub(crate) struct RunRecorder {
    tx: Option<mpsc::UnboundedSender<RecordOp>>,
    throttle: HashMap<String, Instant>,
    last_tokens: TokenCounts,
}

/// `true` for high-frequency notifications whose every occurrence is not worth persisting.
fn is_noisy(method: &str) -> bool {
    method.ends_with("Delta")
        || method.ends_with("_delta")
        || method.ends_with("/delta")
        || method == "thread/tokenUsage/updated"
        || method == "codex/event/token_count"
        || method == "account/rateLimits/updated"
}

/// Short human summary stored as the event message.
fn summary(event: &CodexEvent) -> Option<String> {
    if let Some(reason) = event.reason() {
        return Some(reason.to_string());
    }
    match event.kind() {
        CodexEventKind::SessionStarted => event
            .session_id()
            .map(|sid| format!("session started ({sid})")),
        CodexEventKind::Malformed => Some("malformed JSON event from codex".into()),
        _ => event.method().map(str::to_owned),
    }
}

impl RunRecorder {
    /// Starts recording `run` (nothing happens when the store is disabled).
    pub(crate) fn start(store: &Store, run: NewRun) -> Self {
        if !store.is_enabled() {
            return Self::default();
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<RecordOp>();
        let store = store.clone();
        tokio::spawn(async move {
            let run_id = match store.start_run(run).await {
                Ok(id) => id,
                Err(err) => {
                    tracing::warn!("Failed to record agent run start: {err}");
                    return;
                }
            };
            while let Some(op) = rx.recv().await {
                match op {
                    RecordOp::Event {
                        kind,
                        message,
                        payload,
                    } => store.append_event(run_id, kind, message, payload).detach(),
                    RecordOp::Tokens(tokens) => store.update_tokens(run_id, tokens).detach(),
                    RecordOp::Turn => store.increment_turns(run_id).detach(),
                    RecordOp::RuntimeInfo {
                        worker_host,
                        workspace_path,
                    } => store
                        .update_runtime_info(run_id, worker_host, workspace_path)
                        .detach(),
                    RecordOp::Finish { status, error } => {
                        store.finish_run(run_id, status, error).detach();
                        return;
                    }
                }
            }
            store
                .finish_run(
                    run_id,
                    RunStatus::Cancelled,
                    Some(RECORDER_DROPPED_ERROR.to_owned()),
                )
                .detach();
        });
        Self {
            tx: Some(tx),
            ..Self::default()
        }
    }

    fn send(&self, op: RecordOp) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(op);
        }
    }

    /// Records a Codex event (throttled for noisy methods).
    pub(crate) fn event(&mut self, event: &CodexEvent) {
        if self.tx.is_none() {
            return;
        }
        if event.kind() == CodexEventKind::Notification
            && let Some(method) = event.method().filter(|m| is_noisy(m))
        {
            let now = Instant::now();
            match self.throttle.get(method) {
                Some(last) if now.duration_since(*last) < DELTA_EVENT_INTERVAL => return,
                _ => {
                    self.throttle.insert(method.to_owned(), now);
                }
            }
        }
        self.send(RecordOp::Event {
            kind: event.kind().as_str().to_owned(),
            message: summary(event),
            payload: Some(event.to_json()),
        });
    }

    /// Records the run's cumulative token counters (only when they changed).
    pub(crate) fn tokens(&mut self, totals: TokenCounts) {
        if totals == self.last_tokens {
            return;
        }
        self.last_tokens = totals;
        self.send(RecordOp::Tokens(TokenUsage {
            input: totals.input_tokens,
            output: totals.output_tokens,
            total: totals.total_tokens,
        }));
    }

    /// Records a new turn.
    pub(crate) fn turn(&self) {
        self.send(RecordOp::Turn);
    }

    /// Records where the run executes.
    pub(crate) fn runtime_info(&self, worker_host: Option<&str>, workspace_path: Option<&str>) {
        self.send(RecordOp::RuntimeInfo {
            worker_host: worker_host.map(str::to_owned),
            workspace_path: workspace_path.map(str::to_owned),
        });
    }

    /// Closes the run with a final status; later calls are no-ops.
    pub(crate) fn finish(&mut self, status: RunStatus, error: Option<String>) {
        self.send(RecordOp::Finish { status, error });
        self.tx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noisy_methods_are_detected() {
        assert!(is_noisy("item/agentMessage/delta"));
        assert!(is_noisy("item/reasoning/summaryTextDelta"));
        assert!(is_noisy("codex/event/agent_message_delta"));
        assert!(is_noisy("thread/tokenUsage/updated"));
        assert!(!is_noisy("turn/completed"));
        assert!(!is_noisy("item/completed"));
    }
}
