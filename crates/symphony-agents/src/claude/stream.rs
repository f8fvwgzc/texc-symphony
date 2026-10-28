//! Claude Code `--output-format stream-json` messages → Codex-shaped events.
//!
//! One `claude -p` process is one Symphony turn. Its stdout is one JSON object per line:
//!
//! | Line | Events |
//! |---|---|
//! | `{"type":"system","subtype":"init","session_id":...}` | none (the id is kept for `--resume`) |
//! | `{"type":"assistant","message":{"content":[...]}}` | `text` → `agentMessage` item; `tool_use` `Bash` → `commandExecution` started; other `tool_use` → `toolCall` started |
//! | `{"type":"user","message":{"content":[{"type":"tool_result",...}]}}` | `Bash` → `commandExecution` completed (output, exit 0/1); others → `tool_call_completed` / `tool_call_failed` |
//! | `{"type":"result",...}` | cumulative `thread/tokenUsage/updated`, then the turn ends |
//!
//! Usage counts `input_tokens + cache_creation_input_tokens + cache_read_input_tokens` as input
//! and `output_tokens` as output, summed over the session's turns.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::events::{Emitter, Usage};

/// How one turn's stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Finished {
    /// `result` with `subtype: success` and `is_error: false`; carries `result`.
    Success(Option<String>),
    /// `result` reporting an error: `subtype` and the `result` text (if any).
    Error {
        subtype: String,
        message: Option<String>,
    },
}

/// A tool call waiting for its result.
#[derive(Debug, Clone)]
struct PendingTool {
    name: String,
    input: Value,
}

/// Per-turn stream state.
#[derive(Debug)]
pub(crate) struct TurnStream<'a> {
    thread_id: &'a str,
    turn_id: &'a str,
    pending: HashMap<String, PendingTool>,
    /// Text of the last assistant text block (fallback final message).
    pub(crate) last_text: Option<String>,
    /// Claude's session id, once a message carried one.
    pub(crate) session_id: Option<String>,
    item_counter: u64,
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                Value::String(text) => Some(text.clone()),
                other => other.get("text").and_then(Value::as_str).map(str::to_owned),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn count(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// The usage of one `result` message.
pub(crate) fn result_usage(result: &Value) -> Usage {
    let Some(usage) = result.get("usage") else {
        return Usage::default();
    };
    Usage {
        input_tokens: count(usage, "input_tokens")
            .saturating_add(count(usage, "cache_creation_input_tokens"))
            .saturating_add(count(usage, "cache_read_input_tokens")),
        cached_input_tokens: count(usage, "cache_read_input_tokens"),
        output_tokens: count(usage, "output_tokens"),
    }
}

impl<'a> TurnStream<'a> {
    pub(crate) fn new(thread_id: &'a str, turn_id: &'a str) -> Self {
        Self {
            thread_id,
            turn_id,
            pending: HashMap::new(),
            last_text: None,
            session_id: None,
            item_counter: 0,
        }
    }

    fn next_item_id(&mut self) -> String {
        self.item_counter += 1;
        format!("{}-item-{}", self.turn_id, self.item_counter)
    }

    /// Handles one decoded line. `usage` is the session total so far; a `result` adds to it and
    /// is reported. Returns `Some` once the turn's `result` arrived.
    pub(crate) fn handle(
        &mut self,
        line: &Value,
        emitter: &Emitter<'_>,
        usage: &mut Usage,
    ) -> Option<Finished> {
        if let Some(id) = line.get("session_id").and_then(Value::as_str)
            && !id.is_empty()
        {
            self.session_id = Some(id.to_owned());
        }
        match line.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                self.assistant(line, emitter);
                None
            }
            Some("user") => {
                self.user(line, emitter);
                None
            }
            Some("result") => Some(self.result(line, emitter, usage)),
            _ => None,
        }
    }

    fn content(line: &Value) -> &[Value] {
        line.pointer("/message/content")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice)
    }

    fn assistant(&mut self, line: &Value, emitter: &Emitter<'_>) {
        for block in Self::content(line) {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.trim().is_empty() {
                        continue;
                    }
                    let id = self.next_item_id();
                    emitter.agent_message(&id, text);
                    self.last_text = Some(text.to_owned());
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .map_or_else(|| self.next_item_id(), str::to_owned);
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    if name == "Bash" {
                        let command = input.get("command").and_then(Value::as_str).unwrap_or("");
                        emitter.command_started(&id, command);
                    } else {
                        emitter.notification(
                            "item/started",
                            json!({"item": {"type": "toolCall", "id": id, "tool": name,
                                "status": "inProgress"}}),
                        );
                    }
                    self.pending.insert(id, PendingTool { name, input });
                }
                _ => {}
            }
        }
    }

    fn user(&mut self, line: &Value, emitter: &Emitter<'_>) {
        for block in Self::content(line) {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let failed = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output = text_of(block.get("content").unwrap_or(&Value::Null));
            let Some(tool) = self.pending.remove(&id) else {
                continue;
            };
            if tool.name == "Bash" {
                let command = tool
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                emitter.command_completed(&id, command, &output, Some(i32::from(failed)));
            } else {
                emitter.tool_call(&id, &tool.name, &tool.input, !failed);
            }
        }
    }

    fn result(&mut self, line: &Value, emitter: &Emitter<'_>, usage: &mut Usage) -> Finished {
        usage.add(result_usage(line));
        emitter.usage(self.thread_id, self.turn_id, *usage);
        let subtype = line
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("success")
            .to_owned();
        let is_error = line
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = line
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if is_error || subtype != "success" {
            Finished::Error {
                subtype,
                message: text,
            }
        } else {
            Finished::Success(text.or_else(|| self.last_text.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphony_codex::{CodexEvent, CodexEventKind, EventSink};
    use tokio::sync::mpsc;

    fn drain(rx: &mut mpsc::UnboundedReceiver<CodexEvent>) -> Vec<CodexEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn a_turn_maps_text_tools_and_usage() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(tx);
        let emitter = Emitter::new(&sink, None);
        let mut stream = TurnStream::new("thread", "turn-1");
        let mut usage = Usage {
            input_tokens: 100,
            cached_input_tokens: 0,
            output_tokens: 10,
        };
        let lines = [
            json!({"type": "system", "subtype": "init", "session_id": "abc"}),
            json!({"type": "assistant", "message": {"content": [
                {"type": "text", "text": "Looking."},
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
                {"type": "tool_use", "id": "t2", "name": "Read", "input": {"file_path": "a"}},
            ]}}),
            json!({"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "a\nb"},
                {"type": "tool_result", "tool_use_id": "t2", "is_error": true,
                    "content": [{"type": "text", "text": "missing"}]},
            ]}}),
        ];
        for line in &lines {
            assert_eq!(stream.handle(line, &emitter, &mut usage), None);
        }
        let done = stream.handle(
            &json!({"type": "result", "subtype": "success", "is_error": false,
                "result": "Done.", "session_id": "abc",
                "usage": {"input_tokens": 5, "cache_creation_input_tokens": 2,
                    "cache_read_input_tokens": 3, "output_tokens": 4}}),
            &emitter,
            &mut usage,
        );
        assert_eq!(done, Some(Finished::Success(Some("Done.".into()))));
        assert_eq!(stream.session_id.as_deref(), Some("abc"));
        assert_eq!(
            usage,
            Usage {
                input_tokens: 110,
                cached_input_tokens: 3,
                output_tokens: 14
            }
        );

        let events = drain(&mut rx);
        let kinds: Vec<(CodexEventKind, Option<&str>)> =
            events.iter().map(|e| (e.kind(), e.method())).collect();
        assert_eq!(
            kinds,
            vec![
                (CodexEventKind::Notification, Some("item/completed")),
                (CodexEventKind::Notification, Some("item/started")),
                (CodexEventKind::Notification, Some("item/started")),
                (CodexEventKind::Notification, Some("item/completed")),
                (CodexEventKind::ToolCallFailed, Some("item/tool/call")),
                (
                    CodexEventKind::Notification,
                    Some("thread/tokenUsage/updated")
                ),
            ]
        );
        let command = events[3].payload().unwrap();
        assert_eq!(command["params"]["item"]["aggregatedOutput"], "a\nb");
        assert_eq!(command["params"]["item"]["exitCode"], 0);
        assert_eq!(events[5].token_usage.unwrap().total_tokens, Some(124));
    }

    #[test]
    fn error_results_and_fallback_text() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(tx);
        let emitter = Emitter::new(&sink, None);
        let mut usage = Usage::default();
        let mut stream = TurnStream::new("t", "u");
        assert_eq!(
            stream.handle(
                &json!({"type": "result", "subtype": "error_max_turns", "is_error": true}),
                &emitter,
                &mut usage
            ),
            Some(Finished::Error {
                subtype: "error_max_turns".into(),
                message: None
            })
        );
        let mut stream = TurnStream::new("t", "u");
        stream.handle(
            &json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "hi"}]}}),
            &emitter,
            &mut usage,
        );
        assert_eq!(
            stream.handle(
                &json!({"type": "result", "subtype": "success"}),
                &emitter,
                &mut usage
            ),
            Some(Finished::Success(Some("hi".into())))
        );
    }
}
