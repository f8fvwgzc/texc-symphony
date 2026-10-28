//! Anthropic Messages API wire format (`POST /v1/messages`, `anthropic-version: 2023-06-01`).
//!
//! Input usage counts `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`.
//!
//! Prompt caching ([`Request::prompt_cache`]): an agent loop re-sends its whole conversation on
//! every model call, so each request marks three cache breakpoints (of the four allowed). The last
//! tool definition caches the tools, which are the same for every session of a workflow; the
//! system prompt caches tools and system, fixed for the session; and the last block of the newest
//! message moves forward with the conversation, so each call reads the previous ones from the cache.
//! A call appends at most three positions (text, a `tool_use` run, a `tool_result` run), well
//! inside the 20 positions a breakpoint looks back for an earlier entry. Prefixes below the model's
//! minimum cacheable length are simply not cached.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::sse::SseEvent;
use super::wire::{Message, Reply, Request, ToolCall, fallback_call_id, parse_arguments};
use crate::events::Usage;

/// `anthropic-version` sent with every request.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub(super) fn headers(key: Option<&str>) -> Vec<(String, String)> {
    let mut headers = vec![("anthropic-version".to_owned(), ANTHROPIC_VERSION.to_owned())];
    if let Some(key) = key {
        headers.push(("x-api-key".to_owned(), key.to_owned()));
    }
    headers
}

fn message_json(message: &Message) -> Option<Value> {
    match message {
        Message::User(text) => Some(json!({"role": "user", "content": text})),
        Message::Assistant { text, tool_calls } => {
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(json!({"type": "text", "text": text}));
            }
            for call in tool_calls {
                let input = call
                    .arguments
                    .clone()
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| json!({}));
                content.push(json!({"type": "tool_use", "id": call.id, "name": call.name,
                    "input": input}));
            }
            // An empty assistant message is invalid; the model said nothing worth replaying.
            (!content.is_empty()).then(|| json!({"role": "assistant", "content": content}))
        }
        Message::ToolResults(results) => Some(json!({
            "role": "user",
            "content": results.iter().map(|r| json!({
                "type": "tool_result", "tool_use_id": r.id, "content": r.content,
                "is_error": r.is_error,
            })).collect::<Vec<_>>(),
        })),
    }
}

fn cache_control() -> Value {
    json!({"type": "ephemeral"})
}

/// Marks the last content block of `message` as a cache breakpoint (a string content becomes one
/// text block).
fn mark_last_block(message: &mut Value) {
    let Some(content) = message.get_mut("content") else {
        return;
    };
    if let Value::String(text) = content {
        *content = json!([{"type": "text", "text": text}]);
    }
    if let Some(Value::Object(block)) = content.as_array_mut().and_then(|blocks| blocks.last_mut())
    {
        block.insert("cache_control".into(), cache_control());
    }
}

pub(super) fn body(request: &Request<'_>) -> Value {
    let cache = request.prompt_cache;
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert("max_tokens".into(), json!(request.max_tokens));
    // An empty text block is invalid, so an empty system prompt stays a plain string.
    let system = if cache && !request.system.is_empty() {
        json!([{"type": "text", "text": request.system, "cache_control": cache_control()}])
    } else {
        json!(request.system)
    };
    body.insert("system".into(), system);
    let mut messages: Vec<Value> = request.messages.iter().filter_map(message_json).collect();
    if cache && let Some(last) = messages.last_mut() {
        mark_last_block(last);
    }
    body.insert("messages".into(), Value::Array(messages));
    if !request.tools.is_empty() {
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({"name": t.name, "description": t.description,
                "input_schema": t.schema})
            })
            .collect();
        if cache && let Some(Value::Object(last)) = tools.last_mut() {
            last.insert("cache_control".into(), cache_control());
        }
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    body.insert("stream".into(), json!(request.stream));
    Value::Object(body)
}

fn count(usage: &Value, key: &str) -> u64 {
    usage.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn input_tokens(usage: &Value) -> u64 {
    count(usage, "input_tokens")
        + count(usage, "cache_creation_input_tokens")
        + count(usage, "cache_read_input_tokens")
}

fn error_text(body: &Value) -> String {
    body.pointer("/error/message")
        .and_then(Value::as_str)
        .map_or_else(|| body.to_string(), str::to_owned)
}

pub(super) fn parse(body: &Value) -> Result<Reply, String> {
    if body.get("type").and_then(Value::as_str) == Some("error") {
        return Err(error_text(body));
    }
    let content = body
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| "response has no `content` array".to_owned())?;
    let mut reply = Reply::default();
    for (index, block) in content.iter().enumerate() {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => reply
                .text
                .push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => reply.tool_calls.push(ToolCall {
                id: block
                    .get("id")
                    .and_then(Value::as_str)
                    .map_or_else(|| fallback_call_id(index), str::to_owned),
                name: block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                arguments: Ok(block.get("input").cloned().unwrap_or_else(|| json!({}))),
            }),
            _ => {}
        }
    }
    if let Some(usage) = body.get("usage") {
        reply.usage = Usage {
            input_tokens: input_tokens(usage),
            output_tokens: count(usage, "output_tokens"),
        };
    }
    reply.stop_reason = body
        .get("stop_reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(reply)
}

#[derive(Debug)]
enum Block {
    Text(String),
    Tool {
        id: String,
        name: String,
        json: String,
    },
    Other,
}

/// Accumulates a streamed Messages response.
#[derive(Debug, Default)]
pub struct Stream {
    blocks: BTreeMap<u64, Block>,
    usage: Usage,
    stop_reason: Option<String>,
    done: bool,
}

impl Stream {
    pub(super) fn apply(
        &mut self,
        event: &SseEvent,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<bool, String> {
        let data: Value = serde_json::from_str(&event.data)
            .map_err(|err| format!("undecodable stream event {:?}: {err}", event.event))?;
        let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
        match data.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = data.pointer("/message/usage") {
                    self.usage.input_tokens = input_tokens(usage);
                    self.usage.output_tokens = count(usage, "output_tokens");
                }
            }
            Some("content_block_start") => {
                let block = data.get("content_block").cloned().unwrap_or_default();
                let block = match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.is_empty() {
                            on_text(text);
                        }
                        Block::Text(text.to_owned())
                    }
                    Some("tool_use") => Block::Tool {
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .map_or_else(|| fallback_call_id(index as usize), str::to_owned),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        json: String::new(),
                    },
                    _ => Block::Other,
                };
                self.blocks.insert(index, block);
            }
            Some("content_block_delta") => {
                let delta = data.get("delta").cloned().unwrap_or_default();
                match (
                    self.blocks.get_mut(&index),
                    delta.get("type").and_then(Value::as_str),
                ) {
                    (Some(Block::Text(text)), Some("text_delta")) => {
                        let piece = delta.get("text").and_then(Value::as_str).unwrap_or("");
                        text.push_str(piece);
                        if !piece.is_empty() {
                            on_text(piece);
                        }
                    }
                    (Some(Block::Tool { json, .. }), Some("input_json_delta")) => {
                        json.push_str(
                            delta
                                .get("partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or(""),
                        );
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(reason) = data.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(usage) = data.get("usage") {
                    if let Some(output) = usage.get("output_tokens").and_then(Value::as_u64) {
                        self.usage.output_tokens = output;
                    }
                    let input = input_tokens(usage);
                    if input > 0 {
                        self.usage.input_tokens = input;
                    }
                }
            }
            Some("message_stop") => {
                self.done = true;
                return Ok(true);
            }
            Some("error") => return Err(error_text(&data)),
            _ => {}
        }
        Ok(false)
    }

    pub(super) fn finish(self) -> Result<Reply, String> {
        if !self.done && self.blocks.is_empty() {
            return Err("the stream ended before any content".to_owned());
        }
        let mut reply = Reply {
            usage: self.usage,
            stop_reason: self.stop_reason,
            ..Reply::default()
        };
        for block in self.blocks.into_values() {
            match block {
                Block::Text(text) => reply.text.push_str(&text),
                Block::Tool { id, name, json } => reply.tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: parse_arguments(&json),
                }),
                Block::Other => {}
            }
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::wire::{ToolDef, ToolOutcome};

    fn event(data: Value) -> SseEvent {
        SseEvent {
            event: data.get("type").and_then(Value::as_str).map(str::to_owned),
            data: data.to_string(),
        }
    }

    #[test]
    fn requests_carry_system_tools_and_tool_results() {
        let messages = vec![
            Message::User("do it".into()),
            Message::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "toolu_1".into(),
                    name: "read_file".into(),
                    arguments: Ok(json!({"path": "a"})),
                }],
            },
            Message::ToolResults(vec![ToolOutcome {
                id: "toolu_1".into(),
                content: "hello".into(),
                is_error: false,
            }]),
            Message::Assistant {
                text: String::new(),
                tool_calls: Vec::new(),
            },
        ];
        let tools = vec![ToolDef {
            name: "read_file".into(),
            description: "Read".into(),
            schema: json!({"type": "object"}),
        }];
        let body = body(&Request {
            model: "claude-x",
            system: "sys",
            messages: &messages,
            tools: &tools,
            max_tokens: 100,
            temperature: Some(0.5),
            stream: true,
            prompt_cache: false,
        });
        assert_eq!(body["system"], "sys");
        assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
        let sent = body["messages"].as_array().unwrap();
        assert_eq!(sent.len(), 3, "the empty assistant message is dropped");
        assert_eq!(sent[1]["content"][0]["type"], "tool_use");
        assert_eq!(
            sent[2]["content"][0],
            json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": "hello",
                "is_error": false})
        );
    }

    #[test]
    fn prompt_cache_breakpoints_mark_tools_system_and_the_newest_block() {
        let tools = vec![
            ToolDef {
                name: "read_file".into(),
                description: "Read".into(),
                schema: json!({"type": "object"}),
            },
            ToolDef {
                name: "shell".into(),
                description: "Run".into(),
                schema: json!({"type": "object"}),
            },
        ];
        let request = |messages: &[Message], system: &'static str| {
            body(&Request {
                model: "claude-x",
                system,
                messages,
                tools: &tools,
                max_tokens: 100,
                temperature: None,
                stream: true,
                prompt_cache: true,
            })
        };
        let marker = json!({"type": "ephemeral"});
        let prompt = vec![Message::User("do it".into())];
        let sent = request(&prompt, "sys");
        assert_eq!(sent["tools"][0].get("cache_control"), None);
        assert_eq!(sent["tools"][1]["cache_control"], marker);
        assert_eq!(
            sent["system"],
            json!([{"type": "text", "text": "sys", "cache_control": marker}])
        );
        assert_eq!(
            sent["messages"][0]["content"],
            json!([{"type": "text", "text": "do it", "cache_control": marker}])
        );
        assert_eq!(request(&prompt, "")["system"], "", "no empty text block");

        // The breakpoint moves to the newest message; earlier ones are sent unmarked.
        let mut history = prompt.clone();
        history.push(Message::Assistant {
            text: "Looking.".into(),
            tool_calls: vec![ToolCall {
                id: "toolu_1".into(),
                name: "shell".into(),
                arguments: Ok(json!({"command": "ls"})),
            }],
        });
        history.push(Message::ToolResults(vec![
            ToolOutcome {
                id: "toolu_1".into(),
                content: "a".into(),
                is_error: false,
            },
            ToolOutcome {
                id: "toolu_2".into(),
                content: "b".into(),
                is_error: false,
            },
        ]));
        let sent = request(&history, "sys");
        let messages = sent["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"], "do it");
        assert_eq!(messages[1]["content"][1].get("cache_control"), None);
        assert_eq!(messages[2]["content"][0].get("cache_control"), None);
        assert_eq!(messages[2]["content"][1]["cache_control"], marker);
        let marks = sent.to_string().matches("cache_control").count();
        assert_eq!(marks, 3, "at most four breakpoints are allowed");
    }

    #[test]
    fn streams_assemble_text_tool_calls_and_usage() {
        let mut stream = Stream::default();
        let mut deltas = Vec::new();
        let mut on_text = |t: &str| deltas.push(t.to_owned());
        for data in [
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10,
                "cache_read_input_tokens": 5, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "Hel"}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_start", "index": 1,
                "content_block": {"type": "tool_use", "id": "toolu_9", "name": "shell", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "{\"command\": "}}),
            json!({"type": "content_block_delta", "index": 1,
                "delta": {"type": "input_json_delta", "partial_json": "\"ls\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 42}}),
        ] {
            assert!(!stream.apply(&event(data), &mut on_text).unwrap());
        }
        assert!(
            stream
                .apply(&event(json!({"type": "message_stop"})), &mut on_text)
                .unwrap()
        );
        let reply = stream.finish().unwrap();
        assert_eq!(deltas, ["Hel", "lo"]);
        assert_eq!(reply.text, "Hello");
        assert_eq!(reply.tool_calls[0].arguments, Ok(json!({"command": "ls"})));
        assert_eq!(reply.usage.input_tokens, 15);
        assert_eq!(reply.usage.output_tokens, 42);
        assert_eq!(reply.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn errors_are_reported() {
        let mut stream = Stream::default();
        let err = stream
            .apply(
                &event(
                    json!({"type": "error", "error": {"type": "overloaded_error",
                    "message": "Overloaded"}}),
                ),
                &mut |_| {},
            )
            .unwrap_err();
        assert_eq!(err, "Overloaded");
        assert!(parse(&json!({"type": "error", "error": {"message": "bad"}})).is_err());
        assert!(Stream::default().finish().is_err());
    }
}
