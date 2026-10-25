//! OpenAI-compatible Chat Completions wire format (`POST {base_url}/chat/completions`).
//!
//! Spoken by OpenAI, Ollama, vLLM, LM Studio, OpenRouter, Groq, Together and Gemini's OpenAI
//! endpoint. Streaming asks for `stream_options.include_usage`; tool call fragments are merged by
//! their `index`. Servers that ignore `stream` and answer with one JSON body are handled by the
//! caller.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::sse::SseEvent;
use super::wire::{Message, Reply, Request, ToolCall, fallback_call_id, parse_arguments};
use crate::events::Usage;

pub(super) fn headers(key: Option<&str>) -> Vec<(String, String)> {
    key.map(|key| vec![("authorization".to_owned(), format!("Bearer {key}"))])
        .unwrap_or_default()
}

fn messages_json(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];
    for message in messages {
        match message {
            Message::User(text) => out.push(json!({"role": "user", "content": text})),
            Message::Assistant { text, tool_calls } => {
                if text.is_empty() && tool_calls.is_empty() {
                    continue;
                }
                let mut entry = Map::new();
                entry.insert("role".into(), json!("assistant"));
                entry.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        json!(text)
                    },
                );
                if !tool_calls.is_empty() {
                    entry.insert(
                        "tool_calls".into(),
                        Value::Array(
                            tool_calls
                                .iter()
                                .map(|call| {
                                    let arguments = match &call.arguments {
                                        Ok(value) => value.to_string(),
                                        Err(raw) => raw.clone(),
                                    };
                                    json!({"id": call.id, "type": "function",
                                        "function": {"name": call.name, "arguments": arguments}})
                                })
                                .collect(),
                        ),
                    );
                }
                out.push(Value::Object(entry));
            }
            Message::ToolResults(results) => {
                for result in results {
                    out.push(json!({"role": "tool", "tool_call_id": result.id,
                        "content": result.content}));
                }
            }
        }
    }
    out
}

pub(super) fn body(request: &Request<'_>, completion_tokens_param: bool) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(request.model));
    body.insert(
        "messages".into(),
        Value::Array(messages_json(request.system, request.messages)),
    );
    if !request.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(
                request
                    .tools
                    .iter()
                    .map(|t| {
                        json!({"type": "function", "function": {"name": t.name,
                        "description": t.description, "parameters": t.schema}})
                    })
                    .collect(),
            ),
        );
    }
    let max_key = if completion_tokens_param {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    body.insert(max_key.into(), json!(request.max_tokens));
    if let Some(temperature) = request.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    body.insert("stream".into(), json!(request.stream));
    if request.stream {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    Value::Object(body)
}

fn usage(value: &Value) -> Option<Usage> {
    let usage = value.get("usage").filter(|u| u.is_object())?;
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        input_tokens: count("prompt_tokens"),
        output_tokens: count("completion_tokens"),
    })
}

fn error_text(body: &Value) -> Option<String> {
    let error = body.get("error")?;
    Some(
        error
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| error.to_string(), str::to_owned),
    )
}

/// Message `content` as text (a string, or an array of `{type: text, text}` parts).
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

pub(super) fn parse(body: &Value) -> Result<Reply, String> {
    if let Some(error) = error_text(body) {
        return Err(error);
    }
    let choice = body
        .pointer("/choices/0")
        .ok_or_else(|| "response has no choices".to_owned())?;
    let message = choice
        .get("message")
        .ok_or_else(|| "response choice has no message".to_owned())?;
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .enumerate()
                .map(|(index, call)| ToolCall {
                    id: call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map_or_else(|| fallback_call_id(index), str::to_owned),
                    name: call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    arguments: match call.pointer("/function/arguments") {
                        Some(Value::String(raw)) => parse_arguments(raw),
                        Some(value @ Value::Object(_)) => Ok(value.clone()),
                        _ => Ok(json!({})),
                    },
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Reply {
        text: content_text(message.get("content")),
        tool_calls,
        usage: usage(body).unwrap_or_default(),
        stop_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

#[derive(Debug, Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates a streamed Chat Completions response.
#[derive(Debug, Default)]
pub struct Stream {
    text: String,
    calls: BTreeMap<u64, PartialCall>,
    usage: Option<Usage>,
    stop_reason: Option<String>,
    chunks: usize,
}

impl Stream {
    pub(super) fn apply(
        &mut self,
        event: &SseEvent,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<bool, String> {
        let data = event.data.trim();
        if data == "[DONE]" {
            return Ok(true);
        }
        let chunk: Value =
            serde_json::from_str(data).map_err(|err| format!("undecodable stream chunk: {err}"))?;
        if let Some(error) = error_text(&chunk) {
            return Err(error);
        }
        self.chunks += 1;
        if let Some(usage) = usage(&chunk) {
            self.usage = Some(usage);
        }
        let Some(choice) = chunk.pointer("/choices/0") else {
            return Ok(false);
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop_reason = Some(reason.to_owned());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(false);
        };
        let text = content_text(delta.get("content"));
        if !text.is_empty() {
            on_text(&text);
            self.text.push_str(&text);
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(position as u64);
                let partial = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    partial.id.push_str(id);
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    partial.name.push_str(name);
                }
                match call.pointer("/function/arguments") {
                    Some(Value::String(piece)) => partial.arguments.push_str(piece),
                    Some(value @ Value::Object(_)) => partial.arguments = value.to_string(),
                    _ => {}
                }
            }
        }
        Ok(false)
    }

    pub(super) fn finish(self) -> Result<Reply, String> {
        if self.chunks == 0 {
            return Err("the stream ended before any chunk".to_owned());
        }
        Ok(Reply {
            text: self.text,
            tool_calls: self
                .calls
                .into_values()
                .enumerate()
                .map(|(index, call)| ToolCall {
                    id: if call.id.is_empty() {
                        fallback_call_id(index)
                    } else {
                        call.id
                    },
                    name: call.name,
                    arguments: parse_arguments(&call.arguments),
                })
                .collect(),
            usage: self.usage.unwrap_or_default(),
            stop_reason: self.stop_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::wire::{ToolDef, ToolOutcome};

    fn chunk(data: Value) -> SseEvent {
        SseEvent {
            event: None,
            data: data.to_string(),
        }
    }

    #[test]
    fn requests_use_the_chat_completions_shape() {
        let messages = vec![
            Message::User("go".into()),
            Message::Assistant {
                text: "Looking".into(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "shell".into(),
                    arguments: Ok(json!({"command": "ls"})),
                }],
            },
            Message::ToolResults(vec![ToolOutcome {
                id: "call_1".into(),
                content: "a\nb".into(),
                is_error: false,
            }]),
        ];
        let tools = vec![ToolDef {
            name: "shell".into(),
            description: "Run".into(),
            schema: json!({"type": "object"}),
        }];
        let request = Request {
            model: "m",
            system: "sys",
            messages: &messages,
            tools: &tools,
            max_tokens: 64,
            temperature: None,
            stream: true,
        };
        let sent = body(&request, false);
        assert_eq!(
            sent["messages"][0],
            json!({"role": "system", "content": "sys"})
        );
        assert_eq!(
            sent["messages"][2]["tool_calls"][0]["function"],
            json!({"name": "shell", "arguments": "{\"command\":\"ls\"}"})
        );
        assert_eq!(
            sent["messages"][3],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "a\nb"})
        );
        assert_eq!(
            sent["tools"][0]["function"]["parameters"],
            json!({"type": "object"})
        );
        assert_eq!(sent["max_tokens"], 64);
        assert_eq!(sent["stream_options"], json!({"include_usage": true}));
        let openai = body(
            &Request {
                stream: false,
                ..request
            },
            true,
        );
        assert_eq!(openai["max_completion_tokens"], 64);
        assert!(openai.get("max_tokens").is_none());
        assert!(openai.get("stream_options").is_none());
    }

    #[test]
    fn streamed_tool_calls_are_merged_by_index() {
        let mut stream = Stream::default();
        let mut text = String::new();
        let mut on_text = |t: &str| text.push_str(t);
        for data in [
            json!({"choices": [{"delta": {"role": "assistant", "content": "On it"}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_a",
                "type": "function", "function": {"name": "write_file", "arguments": "{\"pa"}}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0,
                "function": {"arguments": "th\": \"x\"}"}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 3}}),
        ] {
            assert!(!stream.apply(&chunk(data), &mut on_text).unwrap());
        }
        let done = SseEvent {
            event: None,
            data: "[DONE]".into(),
        };
        assert!(stream.apply(&done, &mut on_text).unwrap());
        let reply = stream.finish().unwrap();
        assert_eq!(text, "On it");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "call_a");
        assert_eq!(reply.tool_calls[0].arguments, Ok(json!({"path": "x"})));
        assert_eq!(
            reply.usage,
            Usage {
                input_tokens: 7,
                output_tokens: 3
            }
        );
        assert_eq!(reply.stop_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn whole_responses_and_errors_parse() {
        let reply = parse(
            &json!({"choices": [{"message": {"content": [{"type": "text",
            "text": "hi"}], "tool_calls": [{"function": {"name": "x", "arguments": "not json"}}]},
            "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 2}}),
        )
        .unwrap();
        assert_eq!(reply.text, "hi");
        assert_eq!(reply.tool_calls[0].arguments, Err("not json".into()));
        assert!(!reply.tool_calls[0].id.is_empty());
        assert_eq!(
            parse(&json!({"error": {"message": "rate limited"}})),
            Err("rate limited".into())
        );
        assert!(Stream::default().finish().is_err());
    }
}
