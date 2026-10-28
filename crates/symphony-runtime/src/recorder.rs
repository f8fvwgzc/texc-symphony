//! Store integration: one [`RunRecorder`] per dispatched run writes the run's history to
//! [`symphony_store::Store`] without ever blocking or failing orchestration.
//!
//! Operations are queued to a per-run task that first awaits `start_run` and then applies them in
//! order, awaiting each store write (so a slow database pushes back on this queue instead of
//! piling up in the store). Streaming deltas and token-count notifications are throttled to one
//! event per method per [`DELTA_EVENT_INTERVAL`]. A recorder that is dropped without
//! [`RunRecorder::finish`] (orchestrator crash) closes its run as `cancelled`.
//!
//! The queue is bounded ([`RECORDER_QUEUE_CAPACITY`]). When it is full, events are dropped and
//! counted (an `events_dropped` event records how many when the run finishes); usage, turn,
//! runtime and finish updates are never dropped: they wait in a small local overflow that is
//! flushed first, and usage updates in it coalesce to the latest value.
//!
//! Events are redacted ([`crate::redact::Redactor`]) before they are queued.

// Modified 2026-10-25: bounded queue, redaction and cost recording (audit A1/A4).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use symphony_codex::{CodexEvent, CodexEventKind, TokenCounts};
use symphony_store::{NewRun, RunStatus, Store, TokenUsage};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Instant;

use crate::redact::Redactor;

/// Minimum spacing between two recorded events of the same noisy method within one run.
pub const DELTA_EVENT_INTERVAL: Duration = Duration::from_secs(1);

/// Error recorded when a run's recorder disappears without a final status.
pub const RECORDER_DROPPED_ERROR: &str = "interrupted: orchestrator stopped";

/// Operations a run's queue holds at most.
pub const RECORDER_QUEUE_CAPACITY: usize = 512;

/// Event kind recorded at finish when events were dropped on a full queue.
pub const EVENTS_DROPPED_KIND: &str = "events_dropped";

#[derive(Debug)]
enum RecordOp {
    Event {
        kind: String,
        message: Option<String>,
        payload: Option<Value>,
    },
    Usage(TokenUsage, u64),
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

impl RecordOp {
    fn is_event(&self) -> bool {
        matches!(self, Self::Event { .. })
    }
}

/// Per-run history writer (a no-op for a disabled store).
#[derive(Debug, Default)]
pub(crate) struct RunRecorder {
    tx: Option<mpsc::Sender<RecordOp>>,
    /// Non-event operations that did not fit into the queue, in order.
    overflow: VecDeque<RecordOp>,
    redactor: Arc<Redactor>,
    throttle: HashMap<String, Instant>,
    last_usage: (TokenCounts, u64),
    dropped_events: u64,
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

async fn apply(store: &Store, run_id: symphony_store::RunId, op: RecordOp) -> bool {
    let result = match op {
        RecordOp::Event {
            kind,
            message,
            payload,
        } => store
            .append_event(run_id, kind, message, payload)
            .await
            .map(drop),
        RecordOp::Usage(tokens, cost) => store.update_usage(run_id, tokens, cost).await,
        RecordOp::Turn => store.increment_turns(run_id).await.map(drop),
        RecordOp::RuntimeInfo {
            worker_host,
            workspace_path,
        } => {
            store
                .update_runtime_info(run_id, worker_host, workspace_path)
                .await
        }
        RecordOp::Finish { status, error } => {
            if let Err(err) = store.finish_run(run_id, status, error).await {
                tracing::warn!("Failed to record agent run finish: {err}");
            }
            return false;
        }
    };
    if let Err(err) = result {
        tracing::warn!("Failed to record agent run history: {err}");
    }
    true
}

impl RunRecorder {
    /// Starts recording `run` (nothing happens when the store is disabled).
    pub(crate) fn start(store: &Store, run: NewRun, redactor: Arc<Redactor>) -> Self {
        if !store.is_enabled() {
            return Self::default();
        }
        let (tx, mut rx) = mpsc::channel::<RecordOp>(RECORDER_QUEUE_CAPACITY);
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
                if !apply(&store, run_id, op).await {
                    return;
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
            redactor,
            ..Self::default()
        }
    }

    /// Queues `op`: control operations first flush (or join) the overflow; events are dropped
    /// while the queue is full or control operations are waiting.
    fn send(&mut self, op: RecordOp) {
        let Some(tx) = &self.tx else {
            return;
        };
        while let Some(front) = self.overflow.pop_front() {
            match tx.try_send(front) {
                Ok(()) => {}
                Err(TrySendError::Full(front)) => {
                    self.overflow.push_front(front);
                    break;
                }
                Err(TrySendError::Closed(_)) => {
                    self.overflow.clear();
                    return;
                }
            }
        }
        if op.is_event() {
            if !self.overflow.is_empty() || tx.try_send(op).is_err() {
                self.dropped_events += 1;
            }
            return;
        }
        if self.overflow.is_empty() {
            match tx.try_send(op) {
                Ok(()) | Err(TrySendError::Closed(_)) => return,
                Err(TrySendError::Full(op)) => self.overflow.push_back(op),
            }
            return;
        }
        if let RecordOp::Usage(..) = op
            && let Some(last @ RecordOp::Usage(..)) = self.overflow.back_mut()
        {
            *last = op;
            return;
        }
        self.overflow.push_back(op);
    }

    /// Records a Codex event (throttled for noisy methods, redacted).
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
        let mut payload = event.to_json();
        // `raw` is the same message as one JSON string: key-based redaction cannot see inside it
        // and escaped quotes hide assignments from the patterns, so it is not stored.
        if let Some(object) = payload.as_object_mut() {
            object.remove("raw");
        }
        self.redactor.redact_value(&mut payload);
        let message = summary(event).map(|m| self.redactor.redact_str(&m).into_owned());
        self.send(RecordOp::Event {
            kind: event.kind().as_str().to_owned(),
            message,
            payload: Some(payload),
        });
    }

    /// Records the run's cumulative token counters and cost (only when they changed).
    pub(crate) fn usage(&mut self, totals: TokenCounts, cost_micro_usd: u64) {
        if (totals, cost_micro_usd) == self.last_usage {
            return;
        }
        self.last_usage = (totals, cost_micro_usd);
        self.send(RecordOp::Usage(
            TokenUsage {
                input: totals.input_tokens,
                cached: totals.cached_input_tokens,
                output: totals.output_tokens,
                total: totals.total_tokens,
            },
            cost_micro_usd,
        ));
    }

    /// Records a new turn.
    pub(crate) fn turn(&mut self) {
        self.send(RecordOp::Turn);
    }

    /// Records where the run executes.
    pub(crate) fn runtime_info(&mut self, worker_host: Option<&str>, workspace_path: Option<&str>) {
        self.send(RecordOp::RuntimeInfo {
            worker_host: worker_host.map(str::to_owned),
            workspace_path: workspace_path.map(str::to_owned),
        });
    }

    /// Closes the run with a final status; later calls are no-ops. Whatever is still waiting in
    /// the overflow is delivered (in order) before the finish.
    pub(crate) fn finish(&mut self, status: RunStatus, error: Option<String>) {
        let Some(tx) = self.tx.take() else {
            return;
        };
        let mut tail: Vec<RecordOp> = self.overflow.drain(..).collect();
        if self.dropped_events > 0 {
            tracing::warn!(
                dropped = self.dropped_events,
                "Run history queue was full; some agent events were not recorded"
            );
            tail.push(RecordOp::Event {
                kind: EVENTS_DROPPED_KIND.to_owned(),
                message: Some(format!(
                    "{} events were not recorded (history queue full)",
                    self.dropped_events
                )),
                payload: None,
            });
        }
        tail.push(RecordOp::Finish { status, error });
        // Fast path: everything fits now. Otherwise hand the tail to a task that waits for room;
        // the per-run queue keeps draining, so this always completes.
        let mut pending = tail.into_iter();
        while let Some(op) = pending.next() {
            match tx.try_send(op) {
                Ok(()) => {}
                Err(TrySendError::Closed(_)) => return,
                Err(TrySendError::Full(op)) => {
                    let rest: Vec<RecordOp> = std::iter::once(op).chain(pending).collect();
                    tokio::spawn(async move {
                        for op in rest {
                            if tx.send(op).await.is_err() {
                                return;
                            }
                        }
                    });
                    return;
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn dropped_events(&self) -> u64 {
        self.dropped_events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use symphony_codex::{CodexEventData, StreamMessage};
    use symphony_store::RunQuery;

    #[test]
    fn noisy_methods_are_detected() {
        assert!(is_noisy("item/agentMessage/delta"));
        assert!(is_noisy("item/reasoning/summaryTextDelta"));
        assert!(is_noisy("codex/event/agent_message_delta"));
        assert!(is_noisy("thread/tokenUsage/updated"));
        assert!(!is_noisy("turn/completed"));
        assert!(!is_noisy("item/completed"));
    }

    fn notification(payload: Value) -> CodexEvent {
        let raw = payload.to_string();
        CodexEvent::new(
            CodexEventData::Notification(StreamMessage { payload, raw }),
            None,
            None,
        )
    }

    /// The single run, once its recorder task has written the finish (ops are applied one at a
    /// time, so a store flush alone does not wait for them).
    async fn only_run(store: &Store) -> symphony_store::RunRecord {
        for _ in 0..500 {
            let page = store.list_runs(RunQuery::default()).await.unwrap();
            if let [run] = page.runs.as_slice()
                && run.status != RunStatus::Running
            {
                return run.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the run was never finished");
    }

    #[tokio::test]
    async fn events_are_redacted_before_they_are_stored() {
        let store = Store::open_in_memory().unwrap();
        let redactor = Arc::new(Redactor::new(["operator-tracker-secret".to_owned()]));
        let mut recorder = RunRecorder::start(&store, NewRun::new("i-1", "MT-1"), redactor);
        recorder.event(&notification(json!({
            "method": "item/completed",
            "params": {"item": {"type": "commandExecution",
                "command": "echo operator-tracker-secret",
                "aggregatedOutput": "GITHUB_TOKEN=ghp_0123456789abcdefghijABCDEFGHIJ"}},
        })));
        recorder.usage(
            TokenCounts {
                input_tokens: 3,
                cached_input_tokens: 0,
                output_tokens: 4,
                total_tokens: 7,
            },
            42,
        );
        recorder.finish(RunStatus::Succeeded, None);
        let run = only_run(&store).await;
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.tokens.total, 7);
        assert_eq!(run.cost_micro_usd, 42);
        let events = store.list_events(run.id, None, None).await.unwrap();
        let text = serde_json::to_string(&events).unwrap();
        assert!(!text.contains("operator-tracker-secret"), "{text}");
        assert!(!text.contains("ghp_0123456789"), "{text}");
        assert!(text.contains("[REDACTED]"), "{text}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quoted_and_key_named_secrets_never_reach_the_store() {
        let store = Store::open_in_memory().unwrap();
        let mut recorder = RunRecorder::start(&store, NewRun::new("i-1", "MT-1"), Arc::default());
        recorder.event(&notification(json!({
            "method": "item/completed",
            "params": {"item": {"type": "commandExecution",
                "aggregatedOutput": "login password=\"hunter2hunter2\" ok",
                "headers": {"cookie": "session=cookie-value-123456", "refresh_token": "rt-0123456789"}}},
        })));
        recorder.event(&CodexEvent::new(
            CodexEventData::Malformed {
                raw: r#"{"broken": "client_secret='malformed-secret-99'"#.into(),
            },
            None,
            None,
        ));
        recorder.finish(RunStatus::Succeeded, None);
        let run = only_run(&store).await;
        let events = store.list_events(run.id, None, None).await.unwrap();
        let text = serde_json::to_string(&events).unwrap();
        for secret in [
            "hunter2hunter2",
            "cookie-value-123456",
            "rt-0123456789",
            "malformed-secret-99",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert!(
            events
                .iter()
                .all(|e| e.payload.as_ref().is_none_or(|p| p.get("raw").is_none())),
            "raw copies are not stored"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_full_queue_drops_events_but_keeps_usage_and_the_finish() {
        let store = Store::open_in_memory().unwrap();
        let mut recorder = RunRecorder::start(&store, NewRun::new("i-1", "MT-1"), Arc::default());
        // The recorder task cannot run while this test does not yield, so the queue fills up.
        let total = RECORDER_QUEUE_CAPACITY + 100;
        for n in 0..total {
            recorder.event(&notification(json!({"method": "item/completed", "n": n})));
        }
        for n in 1..=3u64 {
            recorder.usage(
                TokenCounts {
                    input_tokens: n,
                    cached_input_tokens: 0,
                    output_tokens: n,
                    total_tokens: 2 * n,
                },
                n,
            );
        }
        recorder.turn();
        assert_eq!(recorder.dropped_events(), 100);
        assert_eq!(recorder.overflow.len(), 2, "usage coalesced, then the turn");
        recorder.finish(RunStatus::Failed, Some("boom".into()));
        let run = only_run(&store).await;
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.tokens.total, 6);
        assert_eq!(run.cost_micro_usd, 3);
        assert_eq!(run.turns, 1);
        let mut events = Vec::new();
        let mut after = None;
        loop {
            let page = store.list_events(run.id, after, Some(1000)).await.unwrap();
            let Some(last) = page.last() else { break };
            after = Some(last.seq);
            events.extend(page);
        }
        assert_eq!(events.len(), RECORDER_QUEUE_CAPACITY + 1);
        let last = events.last().unwrap();
        assert_eq!(last.kind, EVENTS_DROPPED_KIND);
        assert_eq!(
            last.message.as_deref(),
            Some("100 events were not recorded (history queue full)")
        );
    }
}
