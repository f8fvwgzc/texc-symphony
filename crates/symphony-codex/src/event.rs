//! Events emitted upward while a session runs (`emit_message/4` in `AppServer`).
//!
//! Every event carries the metadata Elixir merged into the message (`codex_app_server_pid`, plus
//! `worker_host` for session-level events and the message's top-level `usage` for stream events) and
//! the token usage / rate limits extracted from it, so the orchestrator can account tokens without
//! re-parsing. [`CodexEvent::to_json`] renders the flat Elixir-shaped map for logs, the store and SSE.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::error::{Blocker, CodexError};
use crate::tokens::{TokenUsage, extract_rate_limits, extract_token_usage};

/// The event name (`last_codex_event` in the orchestrator; serialized snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexEventKind {
    /// `turn/start` succeeded.
    SessionStarted,
    /// `turn/start` failed.
    StartupFailed,
    /// The turn loop ended with a non-blocker error.
    TurnEndedWithError,
    /// `turn/completed`.
    TurnCompleted,
    /// `turn/failed` (with params).
    TurnFailed,
    /// `turn/cancelled` (with params).
    TurnCancelled,
    /// Codex requested operator input.
    TurnInputRequired,
    /// Codex requested an approval that is not auto-granted.
    ApprovalRequired,
    /// An approval / MCP tool prompt was answered automatically (`approval_policy: never`).
    ApprovalAutoApproved,
    /// A dynamic tool call returned `success: true`.
    ToolCallCompleted,
    /// A named dynamic tool call failed (including unknown tool names).
    ToolCallFailed,
    /// A dynamic tool call without a usable tool name.
    UnsupportedToolCall,
    /// Any other method message (`item/*`, `thread/tokenUsage/updated`, `codex/event/*`, ...).
    Notification,
    /// Decoded JSON without a string `method` (stray responses, arrays, scalars).
    OtherMessage,
    /// A `{`-prefixed line that failed to decode.
    Malformed,
}

impl CodexEventKind {
    /// The snake_case name (`"turn_input_required"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStarted => "session_started",
            Self::StartupFailed => "startup_failed",
            Self::TurnEndedWithError => "turn_ended_with_error",
            Self::TurnCompleted => "turn_completed",
            Self::TurnFailed => "turn_failed",
            Self::TurnCancelled => "turn_cancelled",
            Self::TurnInputRequired => "turn_input_required",
            Self::ApprovalRequired => "approval_required",
            Self::ApprovalAutoApproved => "approval_auto_approved",
            Self::ToolCallCompleted => "tool_call_completed",
            Self::ToolCallFailed => "tool_call_failed",
            Self::UnsupportedToolCall => "unsupported_tool_call",
            Self::Notification => "notification",
            Self::OtherMessage => "other_message",
            Self::Malformed => "malformed",
        }
    }

    /// The blocker this event signals (`input_required_blocker?/1` on `last_codex_event`).
    pub fn blocker(self) -> Option<Blocker> {
        match self {
            Self::TurnInputRequired => Some(Blocker::InputRequired),
            Self::ApprovalRequired => Some(Blocker::ApprovalRequired),
            _ => None,
        }
    }
}

/// A decoded stream line and its raw text (`payload` + `raw` in Elixir).
#[derive(Debug, Clone, PartialEq)]
pub struct StreamMessage {
    /// The decoded JSON message.
    pub payload: Value,
    /// The line as received (without the trailing newline).
    pub raw: String,
}

/// Event-specific data.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexEventData {
    /// `session_id` = `"<thread_id>-<turn_id>"`.
    SessionStarted {
        /// `"<thread_id>-<turn_id>"`.
        session_id: String,
        /// Thread id from `thread/start`.
        thread_id: String,
        /// Turn id from `turn/start`.
        turn_id: String,
    },
    /// `turn/start` failed.
    StartupFailed {
        /// The failure.
        reason: CodexError,
    },
    /// The turn loop ended with an error that is not a blocker (blockers end on their own event).
    TurnEndedWithError {
        /// `"<thread_id>-<turn_id>"`.
        session_id: String,
        /// The failure.
        reason: CodexError,
    },
    /// `turn/completed` (`details` is the payload itself).
    TurnCompleted(StreamMessage),
    /// `turn/failed`.
    TurnFailed {
        /// The message.
        message: StreamMessage,
        /// Its `params`.
        details: Value,
    },
    /// `turn/cancelled`.
    TurnCancelled {
        /// The message.
        message: StreamMessage,
        /// Its `params`.
        details: Value,
    },
    /// Operator input requested.
    TurnInputRequired(StreamMessage),
    /// Approval requested and not auto-granted.
    ApprovalRequired(StreamMessage),
    /// Approval answered automatically.
    ApprovalAutoApproved {
        /// The request.
        message: StreamMessage,
        /// The decision sent (`acceptForSession`, `approved_for_session`, `Approve this Session`).
        decision: String,
    },
    /// Tool call succeeded.
    ToolCallCompleted(StreamMessage),
    /// Named tool call failed.
    ToolCallFailed(StreamMessage),
    /// Tool call without a usable name.
    UnsupportedToolCall(StreamMessage),
    /// Any other method message.
    Notification(StreamMessage),
    /// Decoded JSON without a string `method`.
    OtherMessage(StreamMessage),
    /// Undecodable `{`-prefixed line.
    Malformed {
        /// The raw line.
        raw: String,
    },
}

impl CodexEventData {
    /// The event name.
    pub fn kind(&self) -> CodexEventKind {
        match self {
            Self::SessionStarted { .. } => CodexEventKind::SessionStarted,
            Self::StartupFailed { .. } => CodexEventKind::StartupFailed,
            Self::TurnEndedWithError { .. } => CodexEventKind::TurnEndedWithError,
            Self::TurnCompleted(_) => CodexEventKind::TurnCompleted,
            Self::TurnFailed { .. } => CodexEventKind::TurnFailed,
            Self::TurnCancelled { .. } => CodexEventKind::TurnCancelled,
            Self::TurnInputRequired(_) => CodexEventKind::TurnInputRequired,
            Self::ApprovalRequired(_) => CodexEventKind::ApprovalRequired,
            Self::ApprovalAutoApproved { .. } => CodexEventKind::ApprovalAutoApproved,
            Self::ToolCallCompleted(_) => CodexEventKind::ToolCallCompleted,
            Self::ToolCallFailed(_) => CodexEventKind::ToolCallFailed,
            Self::UnsupportedToolCall(_) => CodexEventKind::UnsupportedToolCall,
            Self::Notification(_) => CodexEventKind::Notification,
            Self::OtherMessage(_) => CodexEventKind::OtherMessage,
            Self::Malformed { .. } => CodexEventKind::Malformed,
        }
    }

    fn message(&self) -> Option<&StreamMessage> {
        match self {
            Self::TurnCompleted(m)
            | Self::TurnInputRequired(m)
            | Self::ApprovalRequired(m)
            | Self::ToolCallCompleted(m)
            | Self::ToolCallFailed(m)
            | Self::UnsupportedToolCall(m)
            | Self::Notification(m)
            | Self::OtherMessage(m)
            | Self::TurnFailed { message: m, .. }
            | Self::TurnCancelled { message: m, .. }
            | Self::ApprovalAutoApproved { message: m, .. } => Some(m),
            _ => None,
        }
    }
}

/// One event (`%{event:, timestamp:, codex_app_server_pid:, ...}` in Elixir).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexEvent {
    /// When the event was emitted.
    pub timestamp: DateTime<Utc>,
    /// OS pid of the app-server process (the local `ssh` pid for remote sessions).
    pub codex_app_server_pid: Option<String>,
    /// Remote worker host; set only on session-level events (`session_started`, `startup_failed`,
    /// `turn_ended_with_error`), like Elixir.
    pub worker_host: Option<String>,
    /// The message's top-level `usage` object, if any.
    pub usage: Option<Value>,
    /// Absolute token totals extracted from `usage`/the message (see [`crate::tokens`]).
    pub token_usage: Option<TokenUsage>,
    /// Rate-limit map found in the message (latest wins in the orchestrator).
    pub rate_limits: Option<Value>,
    /// Event-specific data.
    pub data: CodexEventData,
}

impl CodexEvent {
    /// Builds an event stamped now, extracting usage, token totals and rate limits from the message.
    pub fn new(
        data: CodexEventData,
        codex_app_server_pid: Option<String>,
        worker_host: Option<String>,
    ) -> Self {
        let payload = data.message().map(|m| &m.payload);
        let usage = payload
            .and_then(|p| p.get("usage"))
            .filter(|u| u.is_object())
            .cloned();
        let token_usage = extract_token_usage(usage.iter().chain(payload));
        let details = match &data {
            CodexEventData::TurnFailed { details, .. }
            | CodexEventData::TurnCancelled { details, .. } => Some(details),
            _ => None,
        };
        let rate_limits = extract_rate_limits(payload.into_iter().chain(details).chain(&usage));
        Self {
            timestamp: Utc::now(),
            codex_app_server_pid,
            worker_host,
            usage,
            token_usage,
            rate_limits,
            data,
        }
    }

    /// The event name.
    pub fn kind(&self) -> CodexEventKind {
        self.data.kind()
    }

    /// The decoded message (`None` for session-level and malformed events).
    pub fn payload(&self) -> Option<&Value> {
        self.data.message().map(|m| &m.payload)
    }

    /// The raw line (`None` for session-level events).
    pub fn raw(&self) -> Option<&str> {
        match &self.data {
            CodexEventData::Malformed { raw } => Some(raw),
            data => data.message().map(|m| m.raw.as_str()),
        }
    }

    /// `u[:payload] || u[:raw]`: what the orchestrator stores as `last_codex_message.message`.
    pub fn message_value(&self) -> Option<Value> {
        self.payload()
            .cloned()
            .or_else(|| self.raw().map(|raw| Value::String(raw.to_owned())))
    }

    /// The message's `method`, if any.
    pub fn method(&self) -> Option<&str> {
        self.payload()?.get("method")?.as_str()
    }

    /// `session_id` of `session_started` / `turn_ended_with_error`.
    pub fn session_id(&self) -> Option<&str> {
        match &self.data {
            CodexEventData::SessionStarted { session_id, .. }
            | CodexEventData::TurnEndedWithError { session_id, .. } => Some(session_id),
            _ => None,
        }
    }

    /// The failure of `startup_failed` / `turn_ended_with_error`.
    pub fn reason(&self) -> Option<&CodexError> {
        match &self.data {
            CodexEventData::StartupFailed { reason }
            | CodexEventData::TurnEndedWithError { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// The blocker signalled by this event, from its kind or (defensively) its error reason.
    pub fn blocker(&self) -> Option<Blocker> {
        self.kind()
            .blocker()
            .or_else(|| self.reason().and_then(CodexError::blocker))
    }

    /// The flat Elixir-shaped map: metadata, details, `event` and `timestamp` (RFC 3339).
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        let mut put = |key: &str, value: Value| {
            map.insert(key.to_owned(), value);
        };
        put("event", Value::String(self.kind().as_str().to_owned()));
        put("timestamp", Value::String(self.timestamp.to_rfc3339()));
        if let Some(pid) = &self.codex_app_server_pid {
            put("codex_app_server_pid", Value::String(pid.clone()));
        }
        if let Some(host) = &self.worker_host {
            put("worker_host", Value::String(host.clone()));
        }
        if let Some(usage) = &self.usage {
            put("usage", usage.clone());
        }
        match &self.data {
            CodexEventData::SessionStarted {
                session_id,
                thread_id,
                turn_id,
            } => {
                put("session_id", Value::String(session_id.clone()));
                put("thread_id", Value::String(thread_id.clone()));
                put("turn_id", Value::String(turn_id.clone()));
            }
            CodexEventData::StartupFailed { reason } => {
                put("reason", Value::String(reason.to_string()));
            }
            CodexEventData::TurnEndedWithError { session_id, reason } => {
                put("session_id", Value::String(session_id.clone()));
                put("reason", Value::String(reason.to_string()));
            }
            CodexEventData::Malformed { raw } => {
                put("payload", Value::String(raw.clone()));
                put("raw", Value::String(raw.clone()));
            }
            data => {
                if let Some(message) = data.message() {
                    put("payload", message.payload.clone());
                    put("raw", Value::String(message.raw.clone()));
                }
                match data {
                    CodexEventData::TurnCompleted(message) => {
                        put("details", message.payload.clone())
                    }
                    CodexEventData::TurnFailed { details, .. }
                    | CodexEventData::TurnCancelled { details, .. } => {
                        put("details", details.clone())
                    }
                    CodexEventData::ApprovalAutoApproved { decision, .. } => {
                        put("decision", Value::String(decision.clone()));
                    }
                    _ => {}
                }
            }
        }
        Value::Object(map)
    }
}

impl Serialize for CodexEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

/// Where a turn's events go (Elixir's `on_message`). Sends never block; a dropped receiver is ignored.
#[derive(Debug, Clone, Default)]
pub struct EventSink(Option<UnboundedSender<CodexEvent>>);

impl EventSink {
    /// Forwards events to `tx`.
    pub fn new(tx: UnboundedSender<CodexEvent>) -> Self {
        Self(Some(tx))
    }

    /// Discards events (Elixir's default no-op `on_message`).
    pub fn none() -> Self {
        Self(None)
    }

    /// Delivers one event.
    pub fn emit(&self, event: CodexEvent) {
        if let Some(tx) = &self.0 {
            // The receiver may be gone (worker torn down); events are best-effort like `send/2`.
            let _ = tx.send(event);
        }
    }
}

impl From<UnboundedSender<CodexEvent>> for EventSink {
    fn from(tx: UnboundedSender<CodexEvent>) -> Self {
        Self::new(tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn message(payload: Value) -> StreamMessage {
        let raw = payload.to_string();
        StreamMessage { payload, raw }
    }

    #[test]
    fn stream_events_extract_usage_tokens_and_rate_limits() {
        let payload = json!({"method": "codex/event/token_count", "usage": {"total_tokens": 1},
            "params": {"msg": {"info": {"total_token_usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}}},
                "rate_limits": {"limit_id": "codex", "primary": {"used_percent": 10}}}});
        let event = CodexEvent::new(
            CodexEventData::Notification(message(payload)),
            Some("42".into()),
            None,
        );
        assert_eq!(event.usage, Some(json!({"total_tokens": 1})));
        assert_eq!(event.token_usage.and_then(|u| u.total_tokens), Some(5));
        assert_eq!(
            event.rate_limits,
            Some(json!({"limit_id": "codex", "primary": {"used_percent": 10}}))
        );
        let flat = event.to_json();
        assert_eq!(flat["event"], json!("notification"));
        assert_eq!(flat["codex_app_server_pid"], json!("42"));
        assert_eq!(flat["usage"], json!({"total_tokens": 1}));
        assert!(flat.get("worker_host").is_none());
    }

    #[test]
    fn malformed_and_session_events_render_like_elixir() {
        let malformed = CodexEvent::new(
            CodexEventData::Malformed {
                raw: "{\"method\"".into(),
            },
            None,
            None,
        );
        assert_eq!(malformed.message_value(), Some(json!("{\"method\"")));
        assert_eq!(malformed.to_json()["payload"], json!("{\"method\""));
        assert!(malformed.usage.is_none());

        let ended = CodexEvent::new(
            CodexEventData::TurnEndedWithError {
                session_id: "t-u".into(),
                reason: CodexError::PortExit(1),
            },
            Some("1".into()),
            Some("worker-01".into()),
        );
        let flat = ended.to_json();
        assert_eq!(flat["reason"], json!("port_exit: 1"));
        assert_eq!(flat["worker_host"], json!("worker-01"));
        assert_eq!(ended.session_id(), Some("t-u"));
        assert_eq!(ended.blocker(), None);
        assert_eq!(
            serde_json::to_value(CodexEventKind::TurnInputRequired).unwrap(),
            json!("turn_input_required")
        );
    }

    #[test]
    fn blocker_events_report_their_blocker() {
        let event = CodexEvent::new(
            CodexEventData::TurnInputRequired(message(json!({"method": "turn/input_required"}))),
            None,
            None,
        );
        assert_eq!(event.blocker(), Some(Blocker::InputRequired));
        assert_eq!(event.method(), Some("turn/input_required"));
        assert_eq!(
            Blocker::ApprovalRequired.message(),
            "codex turn requires approval"
        );
    }
}
