//! One-line summaries of Codex messages (`StatusDashboard.humanize_codex_message/1`, E.5.9), shared
//! by the HTTP API (`last_message`) and the terminal dashboard (EVENT column).
//!
//! Payloads are decoded JSON, so only string keys are looked up (Elixir also tried atom keys).
//! Elixir's `inspect/1` fallback for unrecognized maps is replaced by compact JSON; grapheme slicing
//! is approximated by `char`s. The result is truncated to 140 characters plus `...`.

use serde_json::Value;
use symphony_codex::CodexEventKind;

use crate::snapshot::CodexMessage;

/// Maximum length of a humanized message before `...` is appended.
pub const MAX_MESSAGE_CHARS: usize = 140;
const INLINE_MAX_CHARS: usize = 80;

/// `humanize_codex_message/1`; `None` is `"no codex message yet"`.
pub fn humanize_codex_message(message: Option<&CodexMessage>) -> String {
    match message {
        None => "no codex message yet".to_owned(),
        Some(message) => humanize_event_message(Some(message.event), message.message.as_ref()),
    }
}

/// Humanizes a raw message (`payload || raw`) for an optional event name.
pub fn humanize_event_message(event: Option<CodexEventKind>, message: Option<&Value>) -> String {
    let null = Value::Null;
    let message = message.unwrap_or(&null);
    let payload = unwrap_payload(message);
    let text = event
        .and_then(|event| event_rule(event, message, payload))
        .unwrap_or_else(|| payload_rule(payload));
    truncate(&text, MAX_MESSAGE_CHARS)
}

fn is_str(value: Option<&Value>) -> bool {
    value.is_some_and(Value::is_string)
}

/// `map_value/2`: the first truthy (non-null, non-false) value among `keys`.
fn map_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let map = value.as_object()?;
    keys.iter()
        .filter_map(|key| map.get(*key))
        .find(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

/// `map_path/2`.
fn map_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.as_object()?.get(*key)?;
    }
    Some(current).filter(|v| !v.is_null())
}

fn first_path<'a>(value: &'a Value, paths: &[&[&str]]) -> Option<&'a Value> {
    paths.iter().find_map(|path| map_path(value, path))
}

fn path_str<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    map_path(value, path).and_then(Value::as_str)
}

fn unwrap_payload(message: &Value) -> &Value {
    if !message.is_object()
        || is_str(map_value(message, &["method"]))
        || is_str(map_value(message, &["session_id"]))
        || is_str(map_value(message, &["reason"]))
    {
        return message;
    }
    map_value(message, &["payload"]).unwrap_or(message)
}

fn event_rule(event: CodexEventKind, message: &Value, payload: &Value) -> Option<String> {
    Some(match event {
        CodexEventKind::SessionStarted => {
            match map_value(payload, &["session_id"]).and_then(Value::as_str) {
                Some(id) => format!("session started ({id})"),
                None => "session started".to_owned(),
            }
        }
        CodexEventKind::TurnInputRequired => "turn blocked: waiting for user input".to_owned(),
        CodexEventKind::ApprovalAutoApproved => {
            let method = map_value(payload, &["method"])
                .or_else(|| map_path(message, &["payload", "method"]))
                .and_then(Value::as_str);
            let base = match method {
                Some(method) => format!("{} (auto-approved)", method_text(method, payload)),
                None => "approval request auto-approved".to_owned(),
            };
            match map_value(message, &["decision"]).and_then(Value::as_str) {
                Some(decision) => format!("{base}: {decision}"),
                None => base,
            }
        }
        CodexEventKind::ToolCallCompleted => tool_event("dynamic tool call completed", payload),
        CodexEventKind::ToolCallFailed => tool_event("dynamic tool call failed", payload),
        CodexEventKind::UnsupportedToolCall => {
            tool_event("unsupported dynamic tool call rejected", payload)
        }
        CodexEventKind::TurnEndedWithError => {
            format!("turn ended with error: {}", format_reason(message))
        }
        CodexEventKind::StartupFailed => format!("startup failed: {}", format_reason(message)),
        CodexEventKind::TurnFailed => method_text("turn/failed", payload),
        CodexEventKind::TurnCancelled => "turn cancelled".to_owned(),
        CodexEventKind::Malformed => "malformed JSON event from codex".to_owned(),
        _ => return None,
    })
}

fn payload_rule(payload: &Value) -> String {
    match payload {
        Value::Object(map) => {
            if let Some(method) = map_value(payload, &["method"]).and_then(Value::as_str) {
                return method_text(method, payload);
            }
            if let Some(id) = map_value(payload, &["session_id"]).and_then(Value::as_str) {
                return format!("session started ({id})");
            }
            if let Some(error) = map.get("error") {
                return format!("error: {}", format_error_value(error));
            }
            sanitize(&payload.to_string())
        }
        Value::String(text) => sanitize(text),
        Value::Null => "nil".to_owned(),
        other => sanitize(&other.to_string()),
    }
}

/// Newlines to spaces, ANSI escapes and control bytes removed, trimmed.
fn sanitize(text: &str) -> String {
    let text = text.replace('\n', " ");
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // `\e[[0-9;]*[A-Za-z]`
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                    if !(next.is_ascii_digit() || next == ';') {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        if c.is_ascii_control() {
            continue;
        }
        out.push(c);
    }
    out.trim().to_owned()
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("...");
    out
}

fn inline_text(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&collapsed, INLINE_MAX_CHARS)
}

fn format_error_value(error: &Value) -> String {
    match error {
        Value::Object(_) => match map_value(error, &["message"]).and_then(Value::as_str) {
            Some(message) => message.to_owned(),
            None => error.to_string(),
        },
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn format_reason(message: &Value) -> String {
    match message {
        Value::Object(_) => match map_value(message, &["reason"]) {
            Some(reason) => format_error_value(reason),
            None => inline_text(&message.to_string()),
        },
        other => format_error_value(other),
    }
}

fn tool_name(payload: &Value) -> Option<&str> {
    path_str(payload, &["params", "tool"]).or_else(|| path_str(payload, &["params", "name"]))
}

fn tool_event(base: &str, payload: &Value) -> String {
    match tool_name(payload).map(str::trim).filter(|t| !t.is_empty()) {
        Some(tool) => format!("{base} ({tool})"),
        None => base.to_owned(),
    }
}

fn integer(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `group_thousands/1` of an integer.
pub fn format_count(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if value < 0 { format!("-{out}") } else { out }
}

fn usage_counts(usage: Option<&Value>) -> Option<String> {
    let usage = usage.filter(|u| u.is_object())?;
    let input = integer(map_value(
        usage,
        &[
            "input_tokens",
            "prompt_tokens",
            "inputTokens",
            "promptTokens",
        ],
    ));
    let output = integer(map_value(
        usage,
        &[
            "output_tokens",
            "completion_tokens",
            "outputTokens",
            "completionTokens",
        ],
    ));
    let total = integer(map_value(usage, &["total_tokens", "total", "totalTokens"]));
    let parts: Vec<String> = [("in", input), ("out", output), ("total", total)]
        .into_iter()
        .filter_map(|(label, value)| value.map(|v| format!("{label} {}", format_count(v))))
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn with_usage(base: String, usage: Option<&Value>) -> String {
    match usage_counts(usage) {
        Some(text) => format!("{base} ({text})"),
        None => base,
    }
}

const DELTA_PATHS: &[&[&str]] = &[
    &["params", "delta"],
    &["params", "msg", "delta"],
    &["params", "textDelta"],
    &["params", "msg", "textDelta"],
    &["params", "outputDelta"],
    &["params", "msg", "outputDelta"],
    &["params", "text"],
    &["params", "msg", "text"],
    &["params", "summaryText"],
    &["params", "msg", "summaryText"],
    &["params", "msg", "content"],
    &["params", "msg", "payload", "delta"],
    &["params", "msg", "payload", "textDelta"],
    &["params", "msg", "payload", "outputDelta"],
    &["params", "msg", "payload", "text"],
    &["params", "msg", "payload", "summaryText"],
    &["params", "msg", "payload", "content"],
];

const REASONING_PATHS: &[&[&str]] = &[
    &["params", "reason"],
    &["params", "summaryText"],
    &["params", "summary"],
    &["params", "text"],
    &["params", "msg", "reason"],
    &["params", "msg", "summaryText"],
    &["params", "msg", "summary"],
    &["params", "msg", "text"],
    &["params", "msg", "payload", "reason"],
    &["params", "msg", "payload", "summaryText"],
    &["params", "msg", "payload", "summary"],
    &["params", "msg", "payload", "text"],
];

const TOKEN_USAGE_PATHS: &[&[&str]] = &[
    &["params", "msg", "payload", "info", "total_token_usage"],
    &["params", "msg", "info", "total_token_usage"],
    &["params", "tokenUsage", "total"],
];

fn preview(payload: &Value, paths: &[&[&str]]) -> Option<String> {
    let text = first_path(payload, paths)?.as_str()?.trim();
    (!text.is_empty()).then(|| inline_text(text))
}

fn streaming(label: &str, payload: &Value) -> String {
    match preview(payload, DELTA_PATHS) {
        Some(preview) => format!("{label}: {preview}"),
        None => label.to_owned(),
    }
}

fn normalize_command(command: Option<&Value>) -> Option<String> {
    match command? {
        Value::String(text) => Some(inline_text(text)),
        Value::Array(items) => {
            let parts: Option<Vec<&str>> = items.iter().map(Value::as_str).collect();
            parts.map(|parts| inline_text(&parts.join(" ")))
        }
        object @ Value::Object(_) => {
            let binary = map_value(object, &["parsedCmd", "command", "cmd"]);
            let args = map_value(object, &["args", "argv"]);
            match (
                binary.and_then(Value::as_str),
                args.and_then(Value::as_array),
            ) {
                (Some(binary), Some(args)) => {
                    let mut all = vec![Value::String(binary.to_owned())];
                    all.extend(args.iter().cloned());
                    normalize_command(Some(&Value::Array(all)))
                }
                _ => normalize_command(binary.or(args)),
            }
        }
        _ => None,
    }
}

fn humanize_item_type(kind: Option<&Value>) -> String {
    match kind {
        None => "item".to_owned(),
        Some(Value::String(text)) => {
            let mut spaced = String::new();
            let mut previous: Option<char> = None;
            for c in text.chars() {
                if c.is_ascii_uppercase()
                    && previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
                {
                    spaced.push(' ');
                }
                spaced.push(c);
                previous = Some(c);
            }
            spaced
                .replace(['_', '/'], " ")
                .to_lowercase()
                .trim()
                .to_owned()
        }
        Some(other) => other.to_string(),
    }
}

fn item_lifecycle(state: &str, payload: &Value) -> String {
    let null = Value::Null;
    let item = map_path(payload, &["params", "item"]).unwrap_or(&null);
    let kind = humanize_item_type(map_value(item, &["type"]));
    let mut details = Vec::new();
    if let Some(id) = map_value(item, &["id"]).and_then(Value::as_str) {
        let short: String = id.chars().take(12).collect();
        if !short.is_empty() {
            details.push(short);
        }
    }
    if let Some(status) = map_value(item, &["status"]).and_then(Value::as_str) {
        let status = status
            .replace(['_', '-'], " ")
            .to_lowercase()
            .trim()
            .to_owned();
        if !status.is_empty() {
            details.push(status);
        }
    }
    if details.is_empty() {
        format!("item {state}: {kind}")
    } else {
        format!("item {state}: {kind} ({})", details.join(", "))
    }
}

fn rate_limit_bucket(bucket: Option<&Value>) -> Option<String> {
    let bucket = bucket.filter(|b| b.is_object())?;
    let used = map_value(bucket, &["usedPercent"]).filter(|v| v.is_number())?;
    match map_value(bucket, &["windowDurationMins"]).and_then(Value::as_i64) {
        Some(window) => Some(format!("{used}% / {window}m")),
        None => Some(format!("{used}% used")),
    }
}

fn rate_limits_summary(rate_limits: Option<&Value>) -> String {
    let Some(rate_limits) = rate_limits.filter(|r| r.is_object()) else {
        return "n/a".to_owned();
    };
    let primary = rate_limit_bucket(map_value(rate_limits, &["primary"]));
    let secondary = rate_limit_bucket(map_value(rate_limits, &["secondary"]));
    match (primary, secondary) {
        (Some(p), Some(s)) => format!("primary {p}; secondary {s}"),
        (Some(p), None) => format!("primary {p}"),
        (None, Some(s)) => format!("secondary {s}"),
        (None, None) => "n/a".to_owned(),
    }
}

fn method_text(method: &str, payload: &Value) -> String {
    match method {
        "thread/started" => match path_str(payload, &["params", "thread", "id"]) {
            Some(id) => format!("thread started ({id})"),
            None => "thread started".to_owned(),
        },
        "turn/started" => match path_str(payload, &["params", "turn", "id"]) {
            Some(id) => format!("turn started ({id})"),
            None => "turn started".to_owned(),
        },
        "turn/completed" => {
            let status = map_path(payload, &["params", "turn", "status"])
                .map(|s| s.as_str().map_or_else(|| s.to_string(), str::to_owned))
                .unwrap_or_else(|| "completed".to_owned());
            let usage = map_path(payload, &["params", "usage"])
                .or_else(|| map_path(payload, &["params", "tokenUsage"]))
                .or_else(|| map_value(payload, &["usage"]));
            with_usage(format!("turn completed ({status})"), usage)
        }
        "turn/failed" => match path_str(payload, &["params", "error", "message"]) {
            Some(message) => format!("turn failed: {message}"),
            None => "turn failed".to_owned(),
        },
        "turn/cancelled" => "turn cancelled".to_owned(),
        "turn/diff/updated" => {
            match path_str(payload, &["params", "diff"]).filter(|d| !d.is_empty()) {
                Some(diff) => format!(
                    "turn diff updated ({} lines)",
                    diff.split('\n').filter(|l| !l.is_empty()).count()
                ),
                None => "turn diff updated".to_owned(),
            }
        }
        "turn/plan/updated" => {
            let entries = map_path(payload, &["params", "plan"])
                .or_else(|| map_path(payload, &["params", "steps"]))
                .or_else(|| map_path(payload, &["params", "items"]));
            match entries {
                None => "plan updated (0 steps)".to_owned(),
                Some(Value::Array(items)) => format!("plan updated ({} steps)", items.len()),
                Some(_) => "plan updated".to_owned(),
            }
        }
        "thread/tokenUsage/updated" => {
            let usage = map_path(payload, &["params", "tokenUsage", "total"])
                .or_else(|| map_value(payload, &["usage"]));
            with_usage("thread token usage updated".to_owned(), usage)
        }
        "item/started" => item_lifecycle("started", payload),
        "item/completed" => item_lifecycle("completed", payload),
        "item/agentMessage/delta" => streaming("agent message streaming", payload),
        "item/plan/delta" => streaming("plan streaming", payload),
        "item/reasoning/summaryTextDelta" => streaming("reasoning summary streaming", payload),
        "item/reasoning/summaryPartAdded" => streaming("reasoning summary section added", payload),
        "item/reasoning/textDelta" => streaming("reasoning text streaming", payload),
        "item/commandExecution/outputDelta" => streaming("command output streaming", payload),
        "item/fileChange/outputDelta" => streaming("file change output streaming", payload),
        "item/commandExecution/requestApproval" => {
            let command = map_path(payload, &["params", "parsedCmd"]).or_else(|| {
                first_path(
                    payload,
                    &[
                        &["params", "command"],
                        &["params", "cmd"],
                        &["params", "argv"],
                        &["params", "args"],
                    ],
                )
            });
            match normalize_command(command) {
                Some(command) => format!("command approval requested ({command})"),
                None => "command approval requested".to_owned(),
            }
        }
        "item/fileChange/requestApproval" => {
            let count = map_path(payload, &["params", "fileChangeCount"])
                .or_else(|| map_path(payload, &["params", "changeCount"]))
                .and_then(Value::as_i64)
                .filter(|n| *n > 0);
            match count {
                Some(n) => format!("file change approval requested ({n} files)"),
                None => "file change approval requested".to_owned(),
            }
        }
        "item/tool/requestUserInput" | "tool/requestUserInput" => {
            let question = path_str(payload, &["params", "question"])
                .or_else(|| path_str(payload, &["params", "prompt"]))
                .filter(|q| !q.trim().is_empty());
            match question {
                Some(question) => format!("tool requires user input: {}", inline_text(question)),
                None => "tool requires user input".to_owned(),
            }
        }
        "account/updated" => format!(
            "account updated (auth {})",
            path_str(payload, &["params", "authMode"]).unwrap_or("unknown")
        ),
        "account/rateLimits/updated" => format!(
            "rate limits updated: {}",
            rate_limits_summary(map_path(payload, &["params", "rateLimits"]))
        ),
        "account/chatgptAuthTokens/refresh" => "account auth token refresh requested".to_owned(),
        "item/tool/call" => match tool_name(payload).filter(|t| !t.trim().is_empty()) {
            Some(tool) => format!("dynamic tool call requested ({tool})"),
            None => "dynamic tool call requested".to_owned(),
        },
        other => match other.strip_prefix("codex/event/") {
            Some(suffix) => wrapper_event(suffix, payload),
            None => match path_str(payload, &["params", "msg", "type"]) {
                Some(kind) => format!("{other} ({kind})"),
                None => other.to_owned(),
            },
        },
    }
}

fn wrapper_event(suffix: &str, payload: &Value) -> String {
    let payload_type = || path_str(payload, &["params", "msg", "payload", "type"]);
    match suffix {
        "mcp_startup_update" => format!(
            "mcp startup: {} {}",
            path_str(payload, &["params", "msg", "server"]).unwrap_or("mcp"),
            path_str(payload, &["params", "msg", "status", "state"]).unwrap_or("updated")
        ),
        "mcp_startup_complete" => "mcp startup complete".to_owned(),
        "task_started" => "task started".to_owned(),
        "user_message" => "user message received".to_owned(),
        "item_started" | "item_completed" => {
            let verb = if suffix == "item_started" {
                "started"
            } else {
                "completed"
            };
            match payload_type() {
                Some("token_count") => wrapper_event("token_count", payload),
                Some(kind) => format!(
                    "item {verb} ({})",
                    humanize_item_type(Some(&Value::String(kind.to_owned())))
                ),
                None => format!("item {verb}"),
            }
        }
        "agent_message_delta" => streaming("agent message streaming", payload),
        "agent_message_content_delta" => streaming("agent message content streaming", payload),
        "agent_reasoning_delta" => streaming("reasoning streaming", payload),
        "reasoning_content_delta" => streaming("reasoning content streaming", payload),
        "agent_reasoning_section_break" => "reasoning section break".to_owned(),
        "agent_reasoning" => match preview(payload, REASONING_PATHS) {
            Some(focus) => format!("reasoning update: {focus}"),
            None => "reasoning update".to_owned(),
        },
        "turn_diff" => "turn diff updated".to_owned(),
        "exec_command_begin" => normalize_command(
            map_path(payload, &["params", "msg", "command"])
                .or_else(|| map_path(payload, &["params", "msg", "parsed_cmd"])),
        )
        .unwrap_or_else(|| "command started".to_owned()),
        "exec_command_end" => {
            let code = map_path(payload, &["params", "msg", "exit_code"])
                .or_else(|| map_path(payload, &["params", "msg", "exitCode"]))
                .and_then(Value::as_i64);
            match code {
                Some(code) => format!("command completed (exit {code})"),
                None => "command completed".to_owned(),
            }
        }
        "exec_command_output_delta" => "command output streaming".to_owned(),
        "mcp_tool_call_begin" => "mcp tool call started".to_owned(),
        "mcp_tool_call_end" => "mcp tool call completed".to_owned(),
        "token_count" => with_usage(
            "token count update".to_owned(),
            first_path(payload, TOKEN_USAGE_PATHS),
        ),
        other => match path_str(payload, &["params", "msg", "type"]) {
            Some(kind) => format!("{other} ({kind})"),
            None => other.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    fn notification(message: Value) -> String {
        humanize_codex_message(Some(&CodexMessage {
            event: CodexEventKind::Notification,
            message: Some(message),
            timestamp: Utc::now(),
        }))
    }

    fn event(event: CodexEventKind, message: Value) -> String {
        humanize_codex_message(Some(&CodexMessage {
            event,
            message: Some(message),
            timestamp: Utc::now(),
        }))
    }

    #[test]
    fn humanizes_the_full_app_server_event_set() {
        let cases = [
            (
                json!({"method": "thread/started", "params": {"thread": {"id": "thread-1"}}}),
                "thread started (thread-1)",
            ),
            (
                json!({"method": "turn/started", "params": {"turn": {"id": "turn-1"}}}),
                "turn started (turn-1)",
            ),
            (
                json!({"method": "turn/completed", "params": {"turn": {"status": "completed"}, "usage": {"input_tokens": "12", "output_tokens": 4, "total_tokens": 1600}}}),
                "turn completed (completed) (in 12, out 4, total 1,600)",
            ),
            (
                json!({"method": "turn/failed", "params": {"error": {"message": "boom"}}}),
                "turn failed: boom",
            ),
            (json!({"method": "turn/cancelled"}), "turn cancelled"),
            (
                json!({"method": "turn/diff/updated", "params": {"diff": "a\nb\n\nc"}}),
                "turn diff updated (3 lines)",
            ),
            (
                json!({"method": "turn/plan/updated", "params": {"plan": [1, 2]}}),
                "plan updated (2 steps)",
            ),
            (
                json!({"method": "thread/tokenUsage/updated", "params": {"tokenUsage": {"total": {"inputTokens": 8, "outputTokens": 3, "totalTokens": 11}}}}),
                "thread token usage updated (in 8, out 3, total 11)",
            ),
            (
                json!({"method": "item/started", "params": {"item": {"type": "commandExecution", "id": "item-1234567890abcdef", "status": "in_progress"}}}),
                "item started: command execution (item-1234567, in progress)",
            ),
            (
                json!({"method": "item/completed", "params": {"item": {}}}),
                "item completed: item",
            ),
            (
                json!({"method": "item/agentMessage/delta", "params": {"delta": "  hello\n world  "}}),
                "agent message streaming: hello world",
            ),
            (
                json!({"method": "item/commandExecution/requestApproval", "params": {"parsedCmd": ["git", "status"]}}),
                "command approval requested (git status)",
            ),
            (
                json!({"method": "item/fileChange/requestApproval", "params": {"fileChangeCount": 3}}),
                "file change approval requested (3 files)",
            ),
            (
                json!({"method": "item/tool/requestUserInput", "params": {"question": "Which?"}}),
                "tool requires user input: Which?",
            ),
            (
                json!({"method": "account/updated", "params": {"authMode": "chatgpt"}}),
                "account updated (auth chatgpt)",
            ),
            (
                json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {"primary": {"usedPercent": 12, "windowDurationMins": 300}, "secondary": {"usedPercent": 3.5}}}}),
                "rate limits updated: primary 12% / 300m; secondary 3.5% used",
            ),
            (
                json!({"method": "account/chatgptAuthTokens/refresh"}),
                "account auth token refresh requested",
            ),
            (
                json!({"method": "item/tool/call", "params": {"tool": "linear_graphql"}}),
                "dynamic tool call requested (linear_graphql)",
            ),
            (
                json!({"method": "custom/thing", "params": {"msg": {"type": "x"}}}),
                "custom/thing (x)",
            ),
            (
                json!({"method": "codex/event/mcp_startup_update", "params": {"msg": {"server": "linear", "status": {"state": "ready"}}}}),
                "mcp startup: linear ready",
            ),
            (
                json!({"method": "codex/event/task_started"}),
                "task started",
            ),
            (
                json!({"method": "codex/event/item_started", "params": {"msg": {"payload": {"type": "webSearch"}}}}),
                "item started (web search)",
            ),
            (
                json!({"method": "codex/event/exec_command_begin", "params": {"msg": {"command": ["git", "status", "--short"]}}}),
                "git status --short",
            ),
            (
                json!({"method": "codex/event/exec_command_end", "params": {"msg": {"exit_code": 0}}}),
                "command completed (exit 0)",
            ),
            (
                json!({"method": "codex/event/token_count", "params": {"msg": {"info": {"total_token_usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}}}}),
                "token count update (in 10, out 5, total 15)",
            ),
            (
                json!({"method": "codex/event/agent_reasoning", "params": {"msg": {"text": "Planning"}}}),
                "reasoning update: Planning",
            ),
            (
                json!({"method": "codex/event/agent_reasoning"}),
                "reasoning update",
            ),
            (
                json!({"method": "codex/event/whatever", "params": {"msg": {"type": "t"}}}),
                "whatever (t)",
            ),
        ];
        for (message, expected) in cases {
            assert_eq!(notification(message.clone()), expected, "{message}");
        }
    }

    #[test]
    fn humanizes_event_specific_messages() {
        assert_eq!(humanize_codex_message(None), "no codex message yet");
        assert_eq!(
            event(CodexEventKind::SessionStarted, json!({"session_id": "t-1"})),
            "session started (t-1)"
        );
        assert_eq!(
            event(
                CodexEventKind::TurnInputRequired,
                json!({"method": "turn/input_required"})
            ),
            "turn blocked: waiting for user input"
        );
        assert_eq!(
            event(
                CodexEventKind::ApprovalAutoApproved,
                json!({"method": "item/commandExecution/requestApproval", "params": {"command": "ls -la"}})
            ),
            "command approval requested (ls -la) (auto-approved)"
        );
        assert_eq!(
            event(
                CodexEventKind::ToolCallCompleted,
                json!({"method": "item/tool/call", "params": {"tool": " linear_graphql "}})
            ),
            "dynamic tool call completed (linear_graphql)"
        );
        assert_eq!(
            event(
                CodexEventKind::ToolCallFailed,
                json!({"method": "item/tool/call", "params": {"name": "x"}})
            ),
            "dynamic tool call failed (x)"
        );
        assert_eq!(
            event(
                CodexEventKind::UnsupportedToolCall,
                json!({"method": "item/tool/call", "params": {}})
            ),
            "unsupported dynamic tool call rejected"
        );
        assert_eq!(
            event(
                CodexEventKind::TurnEndedWithError,
                json!({"reason": "port_exit: 1", "session_id": "s"})
            ),
            "turn ended with error: port_exit: 1"
        );
        assert_eq!(
            event(
                CodexEventKind::StartupFailed,
                json!({"reason": "response_timeout"})
            ),
            "startup failed: response_timeout"
        );
        assert_eq!(
            event(CodexEventKind::Malformed, json!("{bad")),
            "malformed JSON event from codex"
        );
    }

    #[test]
    fn falls_back_to_sanitized_payloads_and_truncates() {
        assert_eq!(
            notification(json!("  line1\nline2\u{1b}[31m red\u{7}  ")),
            "line1 line2 red"
        );
        assert_eq!(
            notification(json!({"error": {"message": "nope"}})),
            "error: nope"
        );
        assert_eq!(
            notification(json!({"payload": {"method": "turn/cancelled"}})),
            "turn cancelled"
        );
        assert_eq!(notification(json!({"a": 1})), r#"{"a":1}"#);
        let long = "x".repeat(200);
        let text = notification(json!(long));
        assert_eq!(text.len(), MAX_MESSAGE_CHARS + 3);
        assert!(text.ends_with("..."));
        assert_eq!(format_count(9876543), "9,876,543");
        assert_eq!(format_count(-1234), "-1,234");
    }
}
