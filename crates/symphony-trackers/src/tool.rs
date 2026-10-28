//! Agent (dynamic) tool plumbing shared by every adapter: the response shape, the compact JSON
//! encoding of tool output, and the session-bound [`ToolBinding`].

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use symphony_core::Issue;
use symphony_core::config::TrackerSettings;

use crate::Tracker;
use crate::error::inspect_string;

/// One `contentItems` entry (`{"type": "inputText", "text": ...}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentItem {
    /// Always `"inputText"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Same string as the tool output.
    pub text: String,
}

/// Dynamic tool response: `{"success": bool, "output": string, "contentItems": [...]}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the call succeeded.
    pub success: bool,
    /// The output text (compact JSON for adapter tools).
    pub output: String,
    /// One `inputText` item mirroring `output`.
    #[serde(rename = "contentItems")]
    pub content_items: Vec<ContentItem>,
}

impl ToolResult {
    /// Builds a response whose single content item mirrors `output`.
    pub fn new(success: bool, output: String) -> Self {
        Self {
            success,
            content_items: vec![ContentItem {
                kind: "inputText".into(),
                text: output.clone(),
            }],
            output,
        }
    }

    /// `success` with `output = tool_json(payload)`.
    pub fn from_payload(success: bool, payload: &Value) -> Self {
        Self::new(success, tool_json(payload))
    }

    /// A failure whose output is `tool_json(payload)`.
    pub fn failure(payload: &Value) -> Self {
        Self::from_payload(false, payload)
    }

    /// `{"error": {"message": message}}` as a failure.
    pub fn error_message(message: &str) -> Self {
        Self::failure(&json!({ "error": { "message": message } }))
    }

    /// An adapter's "unsupported tool" failure listing its tools (`supportedTools: []` for adapters
    /// without tools, such as Memory).
    pub fn unsupported(tool: Option<&str>, supported: &[&str]) -> Self {
        Self::failure(&unsupported_payload(tool, supported))
    }

    /// The response as a JSON object.
    pub fn to_value(&self) -> Value {
        json!({
            "success": self.success,
            "output": self.output,
            "contentItems": self.content_items
                .iter()
                .map(|item| json!({"type": item.kind, "text": item.text}))
                .collect::<Vec<_>>(),
        })
    }
}

fn unsupported_payload(tool: Option<&str>, supported: &[&str]) -> Value {
    json!({
        "error": {
            "message": format!("Unsupported dynamic tool: {}.", inspect_tool_name(tool)),
            "supportedTools": supported,
        }
    })
}

/// Elixir `inspect(tool)`: a quoted string, or `nil`.
pub fn inspect_tool_name(tool: Option<&str>) -> String {
    tool.map_or_else(|| "nil".to_owned(), inspect_string)
}

/// The text of a JSON tool result: compact JSON with keys sorted (serde_json maps are ordered by key
/// because `preserve_order` is not enabled).
///
/// Elixir sent `Jason.encode!(payload, pretty: true)`. A tool result stays in the thread, so every later
/// model request of the run re-sends it; dropping the indentation removes about a third of the bytes of
/// a typical tracker payload without changing its data (SPEC §10.5 leaves payloads adapter-owned).
pub fn tool_json(value: &Value) -> String {
    value.to_string()
}

/// Per-call context for tool execution.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Tracker settings bound at session start (auth/endpoint snapshot).
    pub settings: Arc<TrackerSettings>,
    /// The issue the session works on (SPEC §10.5; no current adapter reads it).
    pub issue: Option<Issue>,
}

/// Session-bound tool advertisement and execution (`Tracker.bind_agent_tools/0`).
///
/// Captures the adapter and its settings once per Codex session so a workflow reload cannot make a
/// session advertise one provider and execute another, nor swap the auth snapshot.
#[derive(Clone)]
pub struct ToolBinding {
    tracker: Arc<dyn Tracker>,
    settings: Arc<TrackerSettings>,
    tool_specs: Vec<Value>,
    secret_environment_names: Vec<String>,
}

impl std::fmt::Debug for ToolBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolBinding")
            .field("kind", &self.tracker.kind())
            .field("settings", &self.settings)
            .field("tool_specs", &self.tool_specs)
            .field("secret_environment_names", &self.secret_environment_names)
            .finish()
    }
}

impl ToolBinding {
    /// Snapshots `tracker` with `settings`.
    pub fn bind(tracker: Arc<dyn Tracker>, settings: TrackerSettings) -> Self {
        let tool_specs = tracker.agent_tool_specs(&settings);
        let secret_environment_names = tracker.secret_environment_names(&settings);
        Self {
            tracker,
            settings: Arc::new(settings),
            tool_specs,
            secret_environment_names,
        }
    }

    /// The bound adapter.
    pub fn tracker(&self) -> &Arc<dyn Tracker> {
        &self.tracker
    }

    /// The bound adapter kind (`linear`, `github`, ...).
    pub fn kind(&self) -> &'static str {
        self.tracker.kind()
    }

    /// The bound settings snapshot.
    pub fn settings(&self) -> &Arc<TrackerSettings> {
        &self.settings
    }

    /// Tool specs to advertise to Codex (`{"name", "description", "inputSchema"}`).
    pub fn tool_specs(&self) -> &[Value] {
        &self.tool_specs
    }

    /// Advertised tool names.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_specs
            .iter()
            .filter_map(|spec| spec.get("name").and_then(Value::as_str))
            .map(str::to_owned)
            .collect()
    }

    /// Env var names to remove from the Codex child environment.
    pub fn secret_environment_names(&self) -> &[String] {
        &self.secret_environment_names
    }

    /// `Tracker.execute_bound_agent_tool/4`: runs `tool` with the bound settings.
    pub async fn execute(
        &self,
        tool: Option<&str>,
        arguments: &Value,
        issue: Option<&Issue>,
    ) -> ToolResult {
        let ctx = ToolContext {
            settings: Arc::clone(&self.settings),
            issue: issue.cloned(),
        };
        self.tracker.execute_agent_tool(tool, arguments, &ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_json_is_compact_with_sorted_keys() {
        let payload = json!({"status": 201, "body": {"id": 9, "tags": ["a", "b"]}});
        assert_eq!(
            tool_json(&payload),
            r#"{"body":{"id":9,"tags":["a","b"]},"status":201}"#
        );
        assert_eq!(
            ToolResult::from_payload(true, &payload).output,
            tool_json(&payload)
        );
    }

    #[test]
    fn unsupported_without_tools_lists_none() {
        let result = ToolResult::unsupported(Some("x"), &[]);
        assert!(!result.success);
        assert_eq!(
            result.output,
            r#"{"error":{"message":"Unsupported dynamic tool: \"x\".","supportedTools":[]}}"#
        );
        assert_eq!(result.content_items[0].text, result.output);
        assert_eq!(
            ToolResult::unsupported(None, &[]).output,
            r#"{"error":{"message":"Unsupported dynamic tool: nil.","supportedTools":[]}}"#
        );
        let value = result.to_value();
        assert_eq!(value["contentItems"][0]["type"], "inputText");
    }
}
