//! Wire format of the Codex app-server protocol as Symphony speaks it.
//!
//! Wire parity with Elixir: newline-delimited JSON, **no `"jsonrpc"` field**, fixed request ids
//! ([`INITIALIZE_ID`], [`THREAD_START_ID`], [`TURN_START_ID`] — the last one reused for every turn), and
//! responses matched by exact integer id.

use serde_json::{Map, Value, json};

/// `initialize` request id.
pub const INITIALIZE_ID: i64 = 1;
/// `thread/start` request id.
pub const THREAD_START_ID: i64 = 2;
/// `turn/start` request id (reused by every turn of a session).
pub const TURN_START_ID: i64 = 3;

/// `clientInfo` sent in `initialize`.
pub const CLIENT_NAME: &str = "symphony-orchestrator";
/// `clientInfo.title`.
pub const CLIENT_TITLE: &str = "Symphony Orchestrator";
/// `clientInfo.version`.
pub const CLIENT_VERSION: &str = "0.1.0";

/// Decision sent for v2 approval requests when auto-approving.
pub const DECISION_ACCEPT_FOR_SESSION: &str = "acceptForSession";
/// Decision sent for legacy approval requests when auto-approving.
pub const DECISION_APPROVED_FOR_SESSION: &str = "approved_for_session";
/// Decision reported for auto-answered MCP tool approval prompts (always this text, even when another
/// approve-like label was sent — Elixir parity, C.13 #14).
pub const DECISION_APPROVE_THIS_SESSION: &str = "Approve this Session";

/// The `initialize` request.
pub fn initialize_request() -> Value {
    json!({
        "method": "initialize",
        "id": INITIALIZE_ID,
        "params": {
            "capabilities": {"experimentalApi": true},
            "clientInfo": {"name": CLIENT_NAME, "title": CLIENT_TITLE, "version": CLIENT_VERSION},
        },
    })
}

/// The `initialized` notification (sent after the `initialize` result).
pub fn initialized_notification() -> Value {
    json!({"method": "initialized", "params": {}})
}

/// The `thread/start` request (`dynamicTools` always present).
pub fn thread_start_request(
    approval_policy: &Value,
    thread_sandbox: &str,
    cwd: &str,
    dynamic_tools: Vec<Value>,
) -> Value {
    json!({
        "method": "thread/start",
        "id": THREAD_START_ID,
        "params": {
            "approvalPolicy": approval_policy,
            "sandbox": thread_sandbox,
            "cwd": cwd,
            "dynamicTools": dynamic_tools,
        },
    })
}

/// The `turn/start` request (no `model`/`effort`/`summary`).
pub fn turn_start_request(
    thread_id: &str,
    prompt: &str,
    cwd: &str,
    title: &str,
    approval_policy: &Value,
    sandbox_policy: &Value,
) -> Value {
    json!({
        "method": "turn/start",
        "id": TURN_START_ID,
        "params": {
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt}],
            "cwd": cwd,
            "title": title,
            "approvalPolicy": approval_policy,
            "sandboxPolicy": sandbox_policy,
        },
    })
}

/// A response to a server-initiated request; `id` is echoed exactly as received.
pub fn response(id: &Value, result: Value) -> Value {
    json!({"id": id, "result": result})
}

/// How a line relates to the request being awaited.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseMatch {
    /// `{"id":<id>,"result":R}` (`result` may be `null`).
    Result(Value),
    /// `{"id":<id>,"error":E}` (checked first) or `{"id":<id>}` without either (the whole message).
    Error(Value),
    /// Not a response to this request (dropped while waiting).
    Other,
}

/// Exact integer id equality (`^request_id`): `3.0` and `"3"` do not match `3`.
pub fn id_matches(value: Option<&Value>, id: i64) -> bool {
    // `as_i64` is `None` for floats and strings.
    value.and_then(Value::as_i64) == Some(id)
}

/// Classifies a decoded message against the awaited request id.
pub fn match_response(message: &Value, id: i64) -> ResponseMatch {
    let Some(map) = message.as_object() else {
        return ResponseMatch::Other;
    };
    if !id_matches(map.get("id"), id) {
        return ResponseMatch::Other;
    }
    if let Some(error) = map.get("error") {
        ResponseMatch::Error(error.clone())
    } else if let Some(result) = map.get("result") {
        ResponseMatch::Result(result.clone())
    } else {
        ResponseMatch::Error(message.clone())
    }
}

/// Extracts a string id (numbers are stringified, like Elixir interpolation) from `parent.<key>.id`.
fn nested_id(result: &Value, key: &str) -> Option<String> {
    match result.get(key)?.get("id")? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `thread/start` result → thread id. `Err` carries the payload for `invalid_thread_payload`
/// (`result.thread` when present, else the whole result).
pub fn thread_id_from_result(result: &Value) -> Result<String, Value> {
    nested_id(result, "thread").ok_or_else(|| {
        result
            .get("thread")
            .cloned()
            .unwrap_or_else(|| result.clone())
    })
}

/// `turn/start` result → turn id. `Err` carries the whole result for `invalid_turn_payload`.
pub fn turn_id_from_result(result: &Value) -> Result<String, Value> {
    nested_id(result, "turn").ok_or_else(|| result.clone())
}

/// Approval request methods and the decision sent when auto-approving.
pub fn approval_decision(method: &str) -> Option<&'static str> {
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Some(DECISION_ACCEPT_FOR_SESSION)
        }
        "execCommandApproval" | "applyPatchApproval" => Some(DECISION_APPROVED_FOR_SESSION),
        _ => None,
    }
}

const INPUT_REQUIRED_METHODS: [&str; 7] = [
    "turn/input_required",
    "turn/needs_input",
    "turn/need_input",
    "turn/request_input",
    "turn/request_response",
    "turn/provide_input",
    "turn/approval_required",
];

fn needs_input_field(value: Option<&Value>) -> bool {
    let Some(map) = value.and_then(Value::as_object) else {
        return false;
    };
    let is_true = |key: &str| map.get(key) == Some(&Value::Bool(true));
    let type_is = |ty: &str| map.get("type").and_then(Value::as_str) == Some(ty);
    is_true("requiresInput")
        || is_true("needsInput")
        || is_true("input_required")
        || is_true("inputRequired")
        || type_is("input_required")
        || type_is("needs_input")
}

/// `needs_input?/2`: MCP elicitations always; otherwise only `turn/*` methods that are input methods or
/// carry an input flag on the message or its `params`.
pub fn needs_input(method: &str, payload: &Value) -> bool {
    if method == "mcpServer/elicitation/request" {
        return true;
    }
    method.starts_with("turn/")
        && (INPUT_REQUIRED_METHODS.contains(&method)
            || needs_input_field(Some(payload))
            || needs_input_field(payload.get("params")))
}

fn option_label(option: &Value) -> Option<&str> {
    option.get("label")?.as_str()
}

fn approval_option_label(options: &[Value]) -> Option<String> {
    let labels: Vec<&str> = options.iter().filter_map(option_label).collect();
    labels
        .iter()
        .find(|l| **l == "Approve this Session")
        .or_else(|| labels.iter().find(|l| **l == "Approve Once"))
        .or_else(|| {
            labels.iter().find(|l| {
                let normalized = l.trim().to_lowercase();
                normalized.starts_with("approve") || normalized.starts_with("allow")
            })
        })
        .map(|l| (*l).to_owned())
}

/// `tool_request_user_input_approval_answers/1`: answers for an `item/tool/requestUserInput` whose
/// questions are **all** MCP tool approval prompts (`mcp_tool_call_approval_*` ids with an approve-like
/// option). Returns the `answers` map, or `None` (→ input required).
pub fn mcp_approval_answers(params: &Value) -> Option<Map<String, Value>> {
    let questions = params.get("questions")?.as_array()?;
    let mut answers = Map::new();
    for question in questions {
        let id = question.get("id")?.as_str()?;
        let options = question.get("options")?.as_array()?;
        if !id.starts_with("mcp_tool_call_approval_") {
            return None;
        }
        let label = approval_option_label(options)?;
        answers.insert(id.to_owned(), json!({"answers": [label]}));
    }
    (!answers.is_empty()).then_some(answers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_have_no_jsonrpc_field_and_fixed_ids() {
        let init = initialize_request();
        assert!(init.get("jsonrpc").is_none());
        assert_eq!(init["id"], json!(1));
        assert_eq!(
            init["params"]["capabilities"]["experimentalApi"],
            json!(true)
        );
        let turn = turn_start_request("t", "p", "/w", "MT-1: x", &json!("never"), &json!({}));
        assert_eq!(turn["id"], json!(3));
        assert_eq!(
            turn["params"]["input"],
            json!([{"type": "text", "text": "p"}])
        );
        assert!(turn["params"].get("model").is_none());
        assert_eq!(
            thread_start_request(&json!({}), "workspace-write", "/w", vec![])["params"]["dynamicTools"],
            json!([])
        );
    }

    #[test]
    fn responses_match_by_exact_integer_id() {
        assert_eq!(
            match_response(&json!({"id": 1, "result": {}}), 1),
            ResponseMatch::Result(json!({}))
        );
        assert_eq!(
            match_response(&json!({"id": 1, "result": null}), 1),
            ResponseMatch::Result(Value::Null)
        );
        assert_eq!(
            match_response(&json!({"id": 1, "error": {"code": 1}, "result": {}}), 1),
            ResponseMatch::Error(json!({"code": 1}))
        );
        assert_eq!(
            match_response(&json!({"id": 1}), 1),
            ResponseMatch::Error(json!({"id": 1}))
        );
        assert_eq!(
            match_response(&json!({"id": "1", "result": {}}), 1),
            ResponseMatch::Other
        );
        assert_eq!(
            match_response(&json!({"id": 1.0, "result": {}}), 1),
            ResponseMatch::Other
        );
        assert_eq!(
            match_response(&json!({"id": 2, "result": {}}), 1),
            ResponseMatch::Other
        );
        assert_eq!(match_response(&json!([1, 2]), 1), ResponseMatch::Other);
    }

    #[test]
    fn thread_and_turn_ids_are_validated() {
        assert_eq!(
            thread_id_from_result(&json!({"thread": {"id": "thread-1"}})),
            Ok("thread-1".into())
        );
        assert_eq!(
            thread_id_from_result(&json!({"thread": {"name": "x"}})),
            Err(json!({"name": "x"}))
        );
        assert_eq!(thread_id_from_result(&json!({})), Err(json!({})));
        assert_eq!(
            turn_id_from_result(&json!({"turn": {"id": 7}})),
            Ok("7".into())
        );
        assert_eq!(turn_id_from_result(&json!({"x": 1})), Err(json!({"x": 1})));
    }

    #[test]
    fn needs_input_only_for_turn_methods_and_elicitations() {
        assert!(needs_input("mcpServer/elicitation/request", &json!({})));
        assert!(needs_input("turn/input_required", &json!({})));
        assert!(needs_input(
            "turn/whatever",
            &json!({"params": {"inputRequired": true}})
        ));
        assert!(needs_input(
            "turn/whatever",
            &json!({"type": "needs_input"})
        ));
        assert!(!needs_input(
            "turn/whatever",
            &json!({"params": {"requiresInput": "true"}})
        ));
        assert!(!needs_input(
            "item/updated",
            &json!({"params": {"requiresInput": true}})
        ));
        assert!(!needs_input("turn/started", &json!({"params": {}})));
    }

    #[test]
    fn approval_decisions_cover_v2_and_legacy_methods() {
        assert_eq!(
            approval_decision("item/commandExecution/requestApproval"),
            Some("acceptForSession")
        );
        assert_eq!(
            approval_decision("item/fileChange/requestApproval"),
            Some("acceptForSession")
        );
        assert_eq!(
            approval_decision("execCommandApproval"),
            Some("approved_for_session")
        );
        assert_eq!(
            approval_decision("applyPatchApproval"),
            Some("approved_for_session")
        );
        assert_eq!(approval_decision("item/tool/call"), None);
    }

    #[test]
    fn mcp_approval_answers_require_every_question_to_be_an_approval() {
        let params = json!({"questions": [{"id": "mcp_tool_call_approval_call-1",
            "options": [{"label": "Approve Once"}, {"label": "Approve this Session"}, {"label": "Deny"}]}]});
        let answers = mcp_approval_answers(&params).unwrap();
        assert_eq!(
            Value::Object(answers),
            json!({"mcp_tool_call_approval_call-1": {"answers": ["Approve this Session"]}})
        );
        let once = json!({"questions": [{"id": "mcp_tool_call_approval_2", "options": [{"label": "Approve Once"}]}]});
        assert_eq!(
            mcp_approval_answers(&once).unwrap()["mcp_tool_call_approval_2"],
            json!({"answers": ["Approve Once"]})
        );
        let allow = json!({"questions": [{"id": "mcp_tool_call_approval_3", "options": [{"label": " Allow it "}, {"x": 1}]}]});
        assert_eq!(
            mcp_approval_answers(&allow).unwrap()["mcp_tool_call_approval_3"],
            json!({"answers": [" Allow it "]})
        );
        for params in [
            json!({"questions": []}),
            json!({"questions": [{"id": "freeform-1", "options": null}]}),
            json!({"questions": [{"id": "options-1", "options": [{"label": "Allow"}]}]}),
            json!({"questions": [{"id": "mcp_tool_call_approval_4", "options": [{"label": "Deny"}]}]}),
            json!({"questions": [
                {"id": "mcp_tool_call_approval_5", "options": [{"label": "Approve Once"}]},
                {"id": "freeform-2", "options": []}]}),
            json!({}),
            json!(null),
        ] {
            assert_eq!(mcp_approval_answers(&params), None, "{params}");
        }
    }
}
