//! The provider-neutral conversation, and the dispatch to one wire format.
//!
//! The agent loop keeps its history as [`Message`]s; [`Wire`] turns them into an Anthropic
//! Messages or OpenAI Chat Completions request and the response (whole or streamed) back into a
//! [`Reply`].

use serde_json::Value;
use symphony_core::config::ApiProvider;

use super::sse::SseEvent;
use super::{anthropic, openai};
use crate::events::Usage;

/// A tool the model asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Provider call id (generated when the provider sent none).
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Parsed arguments; `Err(raw)` when the model sent text that is not JSON.
    pub arguments: Result<Value, String>,
}

/// The result of one tool call, sent back to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    /// The call id it answers.
    pub id: String,
    /// Result text.
    pub content: String,
    /// Whether the call failed.
    pub is_error: bool,
}

/// One entry of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A user prompt (one per turn).
    User(String),
    /// A model reply.
    Assistant {
        /// Text of the reply (may be empty).
        text: String,
        /// Tools it asked for.
        tool_calls: Vec<ToolCall>,
    },
    /// Results of the previous reply's tool calls.
    ToolResults(Vec<ToolOutcome>),
}

/// A tool offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    /// Tool name.
    pub name: String,
    /// What it does.
    pub description: String,
    /// JSON Schema of its arguments.
    pub schema: Value,
}

/// One model response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    /// Concatenated text.
    pub text: String,
    /// Requested tool calls, in order.
    pub tool_calls: Vec<ToolCall>,
    /// Tokens of this response.
    pub usage: Usage,
    /// Provider stop reason (`end_turn`, `tool_use`, `stop`, `tool_calls`, ...).
    pub stop_reason: Option<String>,
}

/// Everything one request needs.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// Model id.
    pub model: &'a str,
    /// System prompt.
    pub system: &'a str,
    /// The conversation so far.
    pub messages: &'a [Message],
    /// Offered tools.
    pub tools: &'a [ToolDef],
    /// `max_tokens` of the response.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Ask for a streamed response.
    pub stream: bool,
    /// Mark prompt-cache breakpoints (Anthropic wire; OpenAI-compatible servers cache by
    /// themselves).
    pub prompt_cache: bool,
}

/// Parses `raw` tool arguments (`""` is `{}`).
pub fn parse_arguments(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(raw).map_err(|_| raw.to_owned())
}

/// One wire format.
#[derive(Debug, Clone)]
pub enum Wire {
    /// Anthropic Messages.
    Anthropic,
    /// OpenAI-compatible Chat Completions (`max_completion_tokens` instead of `max_tokens` when
    /// talking to api.openai.com, whose newer models refuse the older parameter).
    OpenAi {
        /// Send `max_completion_tokens`.
        completion_tokens_param: bool,
    },
}

impl Wire {
    /// The wire format of `provider` at `base_url`.
    pub fn new(provider: ApiProvider, base_url: &str) -> Self {
        match provider {
            ApiProvider::Anthropic => Self::Anthropic,
            ApiProvider::OpenAi => Self::OpenAi {
                completion_tokens_param: url::Url::parse(base_url)
                    .ok()
                    .and_then(|u| {
                        u.host_str()
                            .map(|h| h.eq_ignore_ascii_case("api.openai.com"))
                    })
                    .unwrap_or(false),
            },
        }
    }

    /// Display name for errors.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi { .. } => "OpenAI-compatible",
        }
    }

    /// The endpoint URL under `base_url`.
    pub fn url(&self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        match self {
            Self::Anthropic => format!("{base}/v1/messages"),
            Self::OpenAi { .. } => format!("{base}/chat/completions"),
        }
    }

    /// Authentication and protocol headers (`key` only when there is one).
    pub fn headers(&self, key: Option<&str>) -> Vec<(String, String)> {
        match self {
            Self::Anthropic => anthropic::headers(key),
            Self::OpenAi { .. } => openai::headers(key),
        }
    }

    /// The JSON request body.
    pub fn body(&self, request: &Request<'_>) -> Value {
        match self {
            Self::Anthropic => anthropic::body(request),
            Self::OpenAi {
                completion_tokens_param,
            } => openai::body(request, *completion_tokens_param),
        }
    }

    /// Parses a whole (non-streamed) response body.
    pub fn parse(&self, body: &Value) -> Result<Reply, String> {
        match self {
            Self::Anthropic => anthropic::parse(body),
            Self::OpenAi { .. } => openai::parse(body),
        }
    }

    /// A fresh streaming accumulator.
    pub fn stream(&self) -> StreamState {
        match self {
            Self::Anthropic => StreamState::Anthropic(anthropic::Stream::default()),
            Self::OpenAi { .. } => StreamState::OpenAi(openai::Stream::default()),
        }
    }
}

/// Accumulates one streamed response.
#[derive(Debug)]
pub enum StreamState {
    /// Anthropic events.
    Anthropic(anthropic::Stream),
    /// OpenAI chunks.
    OpenAi(openai::Stream),
}

impl StreamState {
    /// Applies one event, passing text deltas to `on_text`. `Ok(true)` once the stream is done.
    pub fn apply(
        &mut self,
        event: &SseEvent,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<bool, String> {
        match self {
            Self::Anthropic(stream) => stream.apply(event, on_text),
            Self::OpenAi(stream) => stream.apply(event, on_text),
        }
    }

    /// The complete reply.
    pub fn finish(self) -> Result<Reply, String> {
        match self {
            Self::Anthropic(stream) => stream.finish(),
            Self::OpenAi(stream) => stream.finish(),
        }
    }
}

/// A call id for providers that send none.
pub fn fallback_call_id(index: usize) -> String {
    format!("call_{}_{index}", super::new_id("x"))
}
