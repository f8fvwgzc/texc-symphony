//! Client-side dynamic tools (`Codex.DynamicTool` + the `item/tool/call` handling in `AppServer`).
//!
//! The codex crate does not know about trackers: the runtime implements [`DynamicToolHandler`] by
//! binding the tracker's agent tools (a snapshot taken at session start) and hands it to
//! [`crate::StartOptions`]. Specs are advertised in `thread/start.dynamicTools`; calls are dispatched to
//! [`DynamicToolHandler::execute`] and the result is normalized with [`normalize_tool_result`] before it is
//! sent back.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use symphony_core::Issue;

/// Executes Codex dynamic tool calls for one session.
#[async_trait]
pub trait DynamicToolHandler: Send + Sync {
    /// Tool specs (`{"name","description","inputSchema"}`) advertised in `thread/start.dynamicTools`
    /// (an empty list is still sent).
    fn tool_specs(&self) -> Vec<Value>;

    /// Extra env var names to strip from the Codex child (unioned with
    /// `Settings::secret_environment_names`). Defaults to none.
    fn secret_environment_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// Executes `tool` (trimmed name, `None` when missing/blank — still called, like Elixir) with the raw
    /// `arguments` (any JSON; `{}` when absent). The return value is normalized with
    /// [`normalize_tool_result`], so a map with boolean `success` and string `output` is expected.
    async fn execute(&self, tool: Option<&str>, arguments: Value, issue: &Issue) -> Value;
}

/// A handler with no tools: advertises `[]` and answers every call with the tracker-without-tools
/// failure (`{"error":{"message":"Unsupported dynamic tool: \"x\".","supportedTools":[]}}`, compact JSON).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoDynamicTools;

#[async_trait]
impl DynamicToolHandler for NoDynamicTools {
    fn tool_specs(&self) -> Vec<Value> {
        Vec::new()
    }

    async fn execute(&self, tool: Option<&str>, _arguments: Value, _issue: &Issue) -> Value {
        let output = unsupported_tool_payload(tool, &[]).to_string();
        tool_response(false, output)
    }
}

/// Elixir `inspect/1` of a tool name: a JSON-quoted string, or `nil`.
pub fn inspect_tool_name(tool: Option<&str>) -> String {
    match tool {
        Some(name) => Value::String(name.to_owned()).to_string(),
        None => "nil".to_owned(),
    }
}

/// `{"error":{"message":"Unsupported dynamic tool: <inspect tool>.","supportedTools":[...]}}`.
pub fn unsupported_tool_payload(tool: Option<&str>, supported_tools: &[&str]) -> Value {
    json!({
        "error": {
            "message": format!("Unsupported dynamic tool: {}.", inspect_tool_name(tool)),
            "supportedTools": supported_tools,
        }
    })
}

/// `{"success":…,"output":…,"contentItems":[{"type":"inputText","text":output}]}`.
pub fn tool_response(success: bool, output: impl Into<String>) -> Value {
    let output = output.into();
    json!({
        "success": success,
        "output": output,
        "contentItems": content_items(&output),
    })
}

/// Pretty JSON (`Jason.encode!(payload, pretty: true)`) used as tool output text.
pub fn encode_tool_payload(payload: &Value) -> String {
    serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
}

fn content_items(output: &str) -> Value {
    json!([{ "type": "inputText", "text": output }])
}

/// `normalize_dynamic_tool_result/1`.
///
/// - A map with boolean `success` keeps every key; `output` is the existing string `output`, else the
///   `text` of the first `contentItems` entry, else the pretty JSON of the result; `contentItems` is the
///   existing list, else `[{"type":"inputText","text":output}]`.
/// - Anything else becomes a failure whose output is the value's compact JSON (Elixir used `inspect/1`).
pub fn normalize_tool_result(result: Value) -> Value {
    match result {
        Value::Object(mut map) if map.get("success").is_some_and(Value::is_boolean) => {
            let output = match map.get("output") {
                Some(Value::String(existing)) => existing.clone(),
                _ => first_content_text(&map)
                    .unwrap_or_else(|| encode_tool_payload(&Value::Object(map.clone()))),
            };
            if !map.get("contentItems").is_some_and(Value::is_array) {
                map.insert("contentItems".into(), content_items(&output));
            }
            map.insert("output".into(), Value::String(output));
            Value::Object(map)
        }
        other => tool_response(false, other.to_string()),
    }
}

fn first_content_text(map: &Map<String, Value>) -> Option<String> {
    map.get("contentItems")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()
        .map(str::to_owned)
}

fn falsy(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null | Value::Bool(false)))
}

/// `tool_call_name/1`: `params.tool || params.name`, a trimmed non-empty string, else `None`.
pub fn tool_call_name(params: &Value) -> Option<String> {
    let map = params.as_object()?;
    let raw = if falsy(map.get("tool")) {
        map.get("name")
    } else {
        map.get("tool")
    };
    let trimmed = raw?.as_str()?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// `tool_call_arguments/1`: `params.arguments` (any JSON) unless missing/`null`/`false`, else `{}`.
pub fn tool_call_arguments(params: &Value) -> Value {
    match params.as_object().map(|m| m.get("arguments")) {
        Some(args) if !falsy(args) => args.cloned().unwrap_or_else(|| json!({})),
        _ => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_tools_reports_the_unsupported_tool_payload() {
        let issue = Issue::default();
        let result = NoDynamicTools.execute(Some("x"), json!({}), &issue).await;
        let output =
            r#"{"error":{"message":"Unsupported dynamic tool: \"x\".","supportedTools":[]}}"#;
        assert_eq!(result, tool_response(false, output));
        let nil = NoDynamicTools.execute(None, json!({}), &issue).await;
        assert!(
            nil["output"]
                .as_str()
                .unwrap()
                .contains("Unsupported dynamic tool: nil.")
        );
        assert!(NoDynamicTools.tool_specs().is_empty());
    }

    #[test]
    fn unsupported_payload_lists_supported_tools() {
        let payload = unsupported_tool_payload(Some("not_a_real_tool"), &["linear_graphql"]);
        assert_eq!(
            payload,
            json!({"error": {"message": "Unsupported dynamic tool: \"not_a_real_tool\".",
                "supportedTools": ["linear_graphql"]}})
        );
    }

    #[test]
    fn normalization_derives_output_and_content_items() {
        let text = r#"{"data":{"viewer":{"id":"usr_123"}}}"#;
        let normalized = normalize_tool_result(
            json!({"success": true, "contentItems": [{"type": "inputText", "text": text}]}),
        );
        assert_eq!(normalized["output"], json!(text));
        assert_eq!(normalized["contentItems"][0]["text"], json!(text));

        let kept = normalize_tool_result(json!({"success": false, "output": "boom", "extra": 1}));
        assert_eq!(
            kept,
            json!({"success": false, "output": "boom", "extra": 1,
            "contentItems": [{"type": "inputText", "text": "boom"}]})
        );

        let pretty = normalize_tool_result(json!({"success": true, "data": {"a": 1}}));
        assert_eq!(
            pretty["output"],
            json!("{\n  \"data\": {\n    \"a\": 1\n  },\n  \"success\": true\n}")
        );

        let odd = normalize_tool_result(json!(["not", "a", "map"]));
        assert_eq!(odd, tool_response(false, r#"["not","a","map"]"#));
        let non_bool = normalize_tool_result(json!({"success": "yes"}));
        assert_eq!(non_bool["success"], json!(false));
    }

    #[test]
    fn tool_name_and_arguments_follow_elixir_precedence() {
        assert_eq!(tool_call_name(&json!({"tool": " a "})), Some("a".into()));
        assert_eq!(tool_call_name(&json!({"name": "b"})), Some("b".into()));
        assert_eq!(
            tool_call_name(&json!({"tool": null, "name": "b"})),
            Some("b".into())
        );
        assert_eq!(tool_call_name(&json!({"tool": 5, "name": "b"})), None);
        assert_eq!(tool_call_name(&json!({"tool": "   "})), None);
        assert_eq!(tool_call_name(&json!("x")), None);
        assert_eq!(tool_call_arguments(&json!({"arguments": "q"})), json!("q"));
        assert_eq!(tool_call_arguments(&json!({"arguments": null})), json!({}));
        assert_eq!(tool_call_arguments(&json!({})), json!({}));
        assert_eq!(tool_call_arguments(&json!(null)), json!({}));
    }
}
