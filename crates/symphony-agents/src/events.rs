//! Codex-shaped events for backends that are not Codex (see the crate docs).
//!
//! [`Emitter`] builds the same messages a Codex app-server would send, so token accounting
//! ([`symphony_codex::tokens`]), the humanizer, run history and the dashboards need no
//! backend-specific code.

use serde_json::{Value, json};
use symphony_codex::{CodexError, CodexEvent, CodexEventData, EventSink, StreamMessage};

/// Cumulative token counters of a session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Input tokens, including cached and cache-creation input.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

impl Usage {
    /// Adds another response's usage.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }

    /// Input + output.
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Emits one session's events to an [`EventSink`].
#[derive(Debug, Clone)]
pub struct Emitter<'a> {
    sink: &'a EventSink,
    pid: Option<String>,
}

fn message(payload: Value) -> StreamMessage {
    let raw = payload.to_string();
    StreamMessage { payload, raw }
}

impl<'a> Emitter<'a> {
    /// Events for `sink`, tagged with `pid` (the agent process, when there is one).
    pub fn new(sink: &'a EventSink, pid: Option<u32>) -> Self {
        Self {
            sink,
            pid: pid.map(|p| p.to_string()),
        }
    }

    fn emit(&self, data: CodexEventData) {
        self.sink
            .emit(CodexEvent::new(data, self.pid.clone(), None));
    }

    /// A notification `{"method": method, "params": params}`.
    pub fn notification(&self, method: &str, params: Value) {
        self.emit(CodexEventData::Notification(message(
            json!({"method": method, "params": params}),
        )));
    }

    /// `session_started` (`session_id` = `"<thread_id>-<turn_id>"`).
    pub fn session_started(&self, thread_id: &str, turn_id: &str) -> String {
        let session_id = format!("{thread_id}-{turn_id}");
        self.emit(CodexEventData::SessionStarted {
            session_id: session_id.clone(),
            thread_id: thread_id.to_owned(),
            turn_id: turn_id.to_owned(),
        });
        session_id
    }

    /// A streamed piece of the agent's reply.
    pub fn agent_delta(&self, delta: &str) {
        self.notification("item/agentMessage/delta", json!({"delta": delta}));
    }

    /// A complete agent message.
    pub fn agent_message(&self, id: &str, text: &str) {
        self.notification(
            "item/completed",
            json!({"item": {"type": "agentMessage", "id": id, "text": text}}),
        );
    }

    /// A shell command started.
    pub fn command_started(&self, id: &str, command: &str) {
        self.notification(
            "item/started",
            json!({"item": {"type": "commandExecution", "id": id, "command": command,
                "status": "inProgress"}}),
        );
    }

    /// A shell command finished.
    pub fn command_completed(&self, id: &str, command: &str, output: &str, exit_code: Option<i32>) {
        let status = if exit_code == Some(0) {
            "completed"
        } else {
            "failed"
        };
        self.notification(
            "item/completed",
            json!({"item": {"type": "commandExecution", "id": id, "command": command,
                "aggregatedOutput": output, "exitCode": exit_code, "status": status}}),
        );
    }

    /// A file the agent changed.
    pub fn file_changed(&self, id: &str, path: &str, kind: &str) {
        self.notification(
            "item/completed",
            json!({"item": {"type": "fileChange", "id": id, "status": "completed",
                "changes": [{"path": path, "kind": kind}]}}),
        );
    }

    /// A non-shell tool call and whether it succeeded (`tool_call_completed` / `tool_call_failed`).
    pub fn tool_call(&self, id: &str, tool: &str, arguments: &Value, success: bool) {
        let payload = message(json!({"method": "item/tool/call", "id": id,
            "params": {"tool": tool, "arguments": arguments}}));
        self.emit(if success {
            CodexEventData::ToolCallCompleted(payload)
        } else {
            CodexEventData::ToolCallFailed(payload)
        });
    }

    /// Cumulative session usage (`thread/tokenUsage/updated`).
    pub fn usage(&self, thread_id: &str, turn_id: &str, usage: Usage) {
        self.notification(
            "thread/tokenUsage/updated",
            json!({"threadId": thread_id, "turnId": turn_id, "tokenUsage": {"total": {
                "inputTokens": usage.input_tokens,
                "outputTokens": usage.output_tokens,
                "totalTokens": usage.total(),
            }}}),
        );
    }

    /// `turn/completed` with status `completed`.
    pub fn turn_completed(&self, thread_id: &str, turn_id: &str) {
        self.emit(CodexEventData::TurnCompleted(message(json!({
            "method": "turn/completed",
            "params": {"threadId": thread_id, "turn": {"id": turn_id, "status": "completed"}},
        }))));
    }

    /// `turn_ended_with_error` for a failed turn.
    pub fn turn_failed(&self, session_id: &str, reason: &CodexError) {
        self.emit(CodexEventData::TurnEndedWithError {
            session_id: session_id.to_owned(),
            reason: reason.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_codex::CodexEventKind;

    #[test]
    fn usage_events_feed_codex_token_accounting() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = EventSink::new(tx);
        let emitter = Emitter::new(&sink, Some(7));
        emitter.usage(
            "t",
            "u",
            Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        );
        emitter.tool_call("1", "github_api", &json!({"method": "GET"}), false);
        let usage = rx.try_recv().unwrap();
        let tokens = usage.token_usage.unwrap();
        assert_eq!(
            (
                tokens.input_tokens,
                tokens.output_tokens,
                tokens.total_tokens
            ),
            (Some(10), Some(5), Some(15))
        );
        assert_eq!(usage.codex_app_server_pid.as_deref(), Some("7"));
        let call = rx.try_recv().unwrap();
        assert_eq!(call.kind(), CodexEventKind::ToolCallFailed);
        assert_eq!(call.method(), Some("item/tool/call"));
    }
}
