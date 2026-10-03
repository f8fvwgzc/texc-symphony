//! Ports of `app_server_test.exs` (C.11) plus edge cases, driven by the fake app-server scripts in
//! `tests/fixtures/codex/`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use symphony_codex::dynamic_tool::{encode_tool_payload, tool_response, unsupported_tool_payload};
use symphony_codex::launch::shell_escape;
use symphony_codex::{
    AppServerSession, CodexError, CodexEvent, CodexEventKind, DynamicToolHandler, EventSink,
    InvalidWorkspaceCwd, RemoteLauncher, StartOptions, TokenAccumulator, TokenCounts, run,
};
use symphony_core::config;
use symphony_core::{Issue, MapEnv, Settings};
use tempfile::TempDir;
use tokio::sync::mpsc;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex")
        .join(name)
}

fn fixture_command(name: &str) -> String {
    format!(
        "sh {} app-server",
        shell_escape(&fixture(name).to_string_lossy())
    )
}

fn issue(identifier: &str) -> Issue {
    Issue {
        id: Some(format!("issue-{identifier}")),
        identifier: Some(identifier.to_owned()),
        title: Some("Test issue".into()),
        state: Some("In Progress".into()),
        url: Some(format!("https://example.org/issues/{identifier}")),
        labels: vec!["backend".into()],
        ..Issue::default()
    }
}

/// Settings through the real config caster (tracker defaults mirror `write_workflow_file!`).
fn settings(workspace_root: &Path, codex: Value, tracker: Value) -> Arc<Settings> {
    let mut tracker_config =
        json!({"kind": "linear", "api_key": "token", "project_slug": "project"});
    if let (Some(base), Some(extra)) = (tracker_config.as_object_mut(), tracker.as_object()) {
        base.extend(extra.clone());
    }
    let front_matter = json!({
        "tracker": tracker_config,
        "workspace": {"root": workspace_root.to_string_lossy()},
        "codex": codex,
    });
    let parsed = config::parse(front_matter.as_object().unwrap(), &MapEnv::new())
        .expect("valid test settings");
    Arc::new(parsed)
}

/// A temp tree with `workspaces/`, an empty `home/` (so `bash -l` does not read the developer's
/// profile) and a trace file path.
struct Harness {
    root: TempDir,
}

impl Harness {
    fn new() -> Self {
        let harness = Self {
            root: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(harness.workspace_root()).unwrap();
        fs::create_dir_all(harness.home()).unwrap();
        harness
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn workspace_root(&self) -> PathBuf {
        self.path("workspaces")
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    fn trace(&self) -> PathBuf {
        self.path("codex.trace")
    }

    fn workspace(&self, name: &str) -> String {
        let ws = self.workspace_root().join(name);
        fs::create_dir_all(&ws).unwrap();
        ws.to_string_lossy().into_owned()
    }

    fn options(&self, workspace: &str, settings: Arc<Settings>) -> StartOptions {
        StartOptions::new(workspace, settings, self.path("WORKFLOW.md"))
            .with_env("HOME", self.home().to_string_lossy())
            .with_env("SYMP_TEST_CODEX_TRACE", self.trace().to_string_lossy())
            .with_stop_grace(Duration::from_millis(500))
    }

    fn options_for(&self, workspace: &str, command: String, codex: Value) -> StartOptions {
        let mut codex = codex;
        codex["command"] = Value::String(command);
        self.options(
            workspace,
            settings(&self.workspace_root(), codex, json!({})),
        )
    }

    /// Writes a `scripted.sh` script and returns the command running it.
    fn scripted(&self, script: &str) -> String {
        let path = self.path("server.script");
        fs::write(&path, script).unwrap();
        format!(
            "sh {} {}",
            shell_escape(&fixture("scripted.sh").to_string_lossy()),
            shell_escape(&path.to_string_lossy())
        )
    }

    fn trace_lines(&self) -> Vec<String> {
        fs::read_to_string(self.trace())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn traced_messages(&self) -> Vec<Value> {
        self.trace_lines()
            .iter()
            .filter_map(|line| line.strip_prefix("JSON:"))
            .map(|json| serde_json::from_str(json).expect("client sent valid JSON"))
            .collect()
    }
}

struct Events {
    sink: EventSink,
    rx: mpsc::UnboundedReceiver<CodexEvent>,
}

impl Events {
    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            sink: EventSink::new(tx),
            rx,
        }
    }

    fn drain(&mut self) -> Vec<CodexEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            out.push(event);
        }
        out
    }
}

fn kinds(events: &[CodexEvent]) -> Vec<CodexEventKind> {
    events.iter().map(CodexEvent::kind).collect()
}

fn linear_spec() -> Value {
    json!({
        "name": "linear_graphql",
        "description": "Execute a raw GraphQL query or mutation against Linear using Symphony's configured auth.\n",
        "inputSchema": {
            "type": "object", "additionalProperties": false, "required": ["query"],
            "properties": {
                "query": {"type": "string", "description": "GraphQL query or mutation document to execute against Linear."},
                "variables": {"type": ["object", "null"], "description": "Optional GraphQL variables object.", "additionalProperties": true}
            }
        }
    })
}

/// A Linear-shaped tool handler (the runtime binds the real one from the tracker crate).
#[derive(Default)]
struct LinearLikeTools {
    calls: Mutex<Vec<(Option<String>, Value)>>,
    result: Option<Value>,
    delay: Option<Duration>,
}

#[async_trait]
impl DynamicToolHandler for LinearLikeTools {
    fn tool_specs(&self) -> Vec<Value> {
        vec![linear_spec()]
    }

    async fn execute(&self, tool: Option<&str>, arguments: Value, _issue: &Issue) -> Value {
        self.calls
            .lock()
            .unwrap()
            .push((tool.map(str::to_owned), arguments));
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        match (tool, &self.result) {
            (Some("linear_graphql"), Some(result)) => result.clone(),
            _ => tool_response(
                false,
                encode_tool_payload(&unsupported_tool_payload(tool, &["linear_graphql"])),
            ),
        }
    }
}

async fn run_with_message(
    harness: &Harness,
    name: &str,
    message: &str,
    codex: Value,
    tools: Arc<LinearLikeTools>,
    events: &EventSink,
) -> Result<symphony_codex::TurnOutcome, CodexError> {
    let workspace = harness.workspace(name);
    let options = harness
        .options_for(&workspace, fixture_command("single_request.sh"), codex)
        .with_env("FAKE_CODEX_MESSAGE", message)
        .with_tool_handler(tools);
    run(options, "prompt", &issue(name), events).await
}

// 1
#[tokio::test]
async fn rejects_the_workspace_root_and_paths_outside_workspace_root() {
    let h = Harness::new();
    let outside = h.path("outside");
    fs::create_dir_all(&outside).unwrap();
    let root = h.workspace_root().to_string_lossy().into_owned();
    let settings = settings(&h.workspace_root(), json!({"command": "false"}), json!({}));

    let err = run(
        h.options(&root, settings.clone()),
        "guard",
        &issue("MT-999"),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        CodexError::InvalidWorkspaceCwd(InvalidWorkspaceCwd::WorkspaceRoot(_))
    ));
    assert_eq!(err.tag(), "invalid_workspace_cwd");

    let err = run(
        h.options(&outside.to_string_lossy(), settings),
        "guard",
        &issue("MT-999"),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        CodexError::InvalidWorkspaceCwd(InvalidWorkspaceCwd::OutsideWorkspaceRoot(_, _))
    ));
}

// 2
#[tokio::test]
async fn rejects_symlink_escape_cwd_paths_under_the_workspace_root() {
    let h = Harness::new();
    let outside = h.path("outside");
    fs::create_dir_all(&outside).unwrap();
    let symlink = h.workspace_root().join("MT-1000");
    std::os::unix::fs::symlink(&outside, &symlink).unwrap();
    let settings = settings(&h.workspace_root(), json!({"command": "false"}), json!({}));

    let err = run(
        h.options(&symlink.to_string_lossy(), settings),
        "guard",
        &issue("MT-1000"),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    match err {
        CodexError::InvalidWorkspaceCwd(InvalidWorkspaceCwd::SymlinkEscape(path, _root)) => {
            assert_eq!(path, symlink);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn unreadable_workspace_paths_are_reported() {
    let h = Harness::new();
    let file = h.workspace_root().join("file");
    fs::write(&file, "x").unwrap();
    let ws = file.join("MT-1");
    let settings = settings(&h.workspace_root(), json!({"command": "false"}), json!({}));
    let err = run(
        h.options(&ws.to_string_lossy(), settings),
        "p",
        &issue("MT-1"),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    match err {
        CodexError::InvalidWorkspaceCwd(InvalidWorkspaceCwd::PathUnreadable(_, reason)) => {
            assert_eq!(reason.name(), "enotdir");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// 3
#[tokio::test]
async fn turn_timeout_resets_on_stream_updates_and_fires_after_silence() {
    let h = Harness::new();
    let ws = h.workspace("MT-TIMEOUT");
    let options = h.options_for(
        &ws,
        fixture_command("stream_updates.sh"),
        json!({"turn_timeout_ms": 250}),
    );
    run(
        options,
        "stream updates",
        &issue("MT-TIMEOUT"),
        &EventSink::none(),
    )
    .await
    .expect("stream updates keep the turn alive");

    let options = h.options_for(
        &ws,
        fixture_command("silent_turn.sh"),
        json!({"turn_timeout_ms": 100}),
    );
    let err = run(
        options,
        "silent turn",
        &issue("MT-TIMEOUT"),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    assert_eq!(err, CodexError::TurnTimeout);
}

// 4
#[tokio::test]
async fn passes_explicit_turn_sandbox_policies_through_unchanged() {
    let h = Harness::new();
    let ws = h.workspace("MT-1001");
    for policy in [
        json!({"type": "dangerFullAccess"}),
        json!({"type": "externalSandbox", "profile": "remote-ci"}),
        json!({"type": "workspaceWrite", "writableRoots": ["relative/path"], "networkAccess": true}),
        json!({"type": "futureSandbox", "nested": {"flag": true}}),
    ] {
        let _ = fs::remove_file(h.trace());
        let options = h.options_for(
            &ws,
            fixture_command("basic.sh"),
            json!({"turn_sandbox_policy": policy.clone()}),
        );
        run(options, "policy", &issue("MT-1001"), &EventSink::none())
            .await
            .unwrap();
        let turn_start = h
            .traced_messages()
            .into_iter()
            .find(|m| m["method"] == "turn/start")
            .expect("turn/start traced");
        assert_eq!(turn_start["params"]["sandboxPolicy"], policy);
    }
}

#[tokio::test]
async fn default_turn_policy_and_cwd_use_the_canonical_workspace() {
    let h = Harness::new();
    let ws = h.workspace("MT-1002");
    let canonical = fs::canonicalize(&ws)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let options = h.options_for(&ws, fixture_command("basic.sh"), json!({}));
    run(options, "p", &issue("MT-1002"), &EventSink::none())
        .await
        .unwrap();
    let messages = h.traced_messages();
    let thread_start = messages
        .iter()
        .find(|m| m["method"] == "thread/start")
        .unwrap();
    assert_eq!(thread_start["params"]["cwd"], json!(canonical));
    assert_eq!(thread_start["params"]["sandbox"], json!("workspace-write"));
    assert_eq!(
        thread_start["params"]["approvalPolicy"],
        json!({"reject": {"sandbox_approval": true, "rules": true, "mcp_elicitations": true}})
    );
    let turn_start = messages
        .iter()
        .find(|m| m["method"] == "turn/start")
        .unwrap();
    assert_eq!(turn_start["params"]["cwd"], json!(canonical));
    assert_eq!(turn_start["params"]["title"], json!("MT-1002: Test issue"));
    assert_eq!(
        turn_start["params"]["sandboxPolicy"],
        json!({"type": "workspaceWrite", "writableRoots": [canonical], "readOnlyAccess": {"type": "fullAccess"},
            "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false})
    );
    for message in &messages {
        assert!(
            message.get("jsonrpc").is_none(),
            "no jsonrpc field: {message}"
        );
    }
    assert_eq!(messages[1], json!({"method": "initialized", "params": {}}));
}

// 5
#[tokio::test]
async fn marks_request_for_input_events_as_a_hard_failure() {
    let h = Harness::new();
    let mut events = Events::new();
    let err = run_with_message(
        &h,
        "MT-88",
        r#"{"method":"turn/input_required","id":"resp-1","params":{"requiresInput":true,"reason":"blocked"}}"#,
        json!({}),
        Arc::new(LinearLikeTools::default()),
        &events.sink,
    )
    .await
    .unwrap_err();
    let CodexError::TurnInputRequired(payload) = &err else {
        panic!("unexpected error: {err:?}");
    };
    assert_eq!(payload["method"], json!("turn/input_required"));
    assert_eq!(err.blocker(), Some(symphony_codex::Blocker::InputRequired));

    // C.13 #8: the blocker event stays the last event (no `turn_ended_with_error` overwrites it).
    let events = events.drain();
    assert_eq!(
        kinds(&events),
        vec![
            CodexEventKind::SessionStarted,
            CodexEventKind::TurnInputRequired
        ]
    );
    assert_eq!(
        events.last().and_then(CodexEvent::blocker),
        Some(symphony_codex::Blocker::InputRequired)
    );
}

// 6
#[tokio::test]
async fn treats_mcp_elicitation_requests_as_hard_input_blockers() {
    let h = Harness::new();
    let err = run_with_message(
        &h,
        "MT-188",
        r#"{"method":"mcpServer/elicitation/request","params":{"message":"Need operator input"}}"#,
        json!({}),
        Arc::new(LinearLikeTools::default()),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    let CodexError::TurnInputRequired(payload) = err else {
        panic!("unexpected error: {err:?}");
    };
    assert_eq!(payload["method"], json!("mcpServer/elicitation/request"));
}

const COMMAND_APPROVAL: &str = r#"{"id":99,"method":"item/commandExecution/requestApproval","params":{"command":"gh pr view","cwd":"/tmp","reason":"need approval"}}"#;

// 7
#[tokio::test]
async fn fails_when_command_execution_approval_is_required_under_safer_defaults() {
    let h = Harness::new();
    let mut events = Events::new();
    let err = run_with_message(
        &h,
        "MT-89",
        COMMAND_APPROVAL,
        json!({}),
        Arc::new(LinearLikeTools::default()),
        &events.sink,
    )
    .await
    .unwrap_err();
    let CodexError::ApprovalRequired(payload) = &err else {
        panic!("unexpected error: {err:?}");
    };
    assert_eq!(
        payload["method"],
        json!("item/commandExecution/requestApproval")
    );
    assert_eq!(
        err.blocker(),
        Some(symphony_codex::Blocker::ApprovalRequired)
    );
    assert!(
        h.traced_messages().iter().all(|m| m["id"] != json!(99)),
        "no reply sent"
    );
    assert_eq!(
        kinds(&events.drain()),
        vec![
            CodexEventKind::SessionStarted,
            CodexEventKind::ApprovalRequired
        ]
    );
}

// 8
#[tokio::test]
async fn auto_approves_command_execution_approval_requests_when_approval_policy_is_never() {
    let h = Harness::new();
    let mut events = Events::new();
    run_with_message(
        &h,
        "MT-89",
        COMMAND_APPROVAL,
        json!({"approval_policy": "never"}),
        Arc::new(LinearLikeTools::default()),
        &events.sink,
    )
    .await
    .unwrap();
    let messages = h.traced_messages();
    assert!(
        messages.iter().any(|m| m["id"] == json!(1)
            && m["params"]["capabilities"]["experimentalApi"] == json!(true))
    );
    let thread_start = messages.iter().find(|m| m["id"] == json!(2)).unwrap();
    let tools = thread_start["params"]["dynamicTools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], json!("linear_graphql"));
    assert_eq!(tools[0]["inputSchema"]["required"], json!(["query"]));
    assert!(tools[0]["description"].as_str().unwrap().contains("Linear"));
    assert_eq!(thread_start["params"]["approvalPolicy"], json!("never"));
    assert!(
        messages
            .iter()
            .any(|m| m == &json!({"id": 99, "result": {"decision": "acceptForSession"}}))
    );

    let events = events.drain();
    let approved = events
        .iter()
        .find(|e| e.kind() == CodexEventKind::ApprovalAutoApproved)
        .unwrap();
    assert_eq!(approved.to_json()["decision"], json!("acceptForSession"));
}

#[tokio::test]
async fn auto_approves_legacy_approvals_echoing_string_ids() {
    let h = Harness::new();
    run_with_message(
        &h,
        "MT-LEGACY",
        r#"{"id":"exec-7","method":"execCommandApproval","params":{"command":["ls"]}}"#,
        json!({"approval_policy": "never"}),
        Arc::new(LinearLikeTools::default()),
        &EventSink::none(),
    )
    .await
    .unwrap();
    assert!(
        h.traced_messages()
            .iter()
            .any(|m| m == &json!({"id": "exec-7", "result": {"decision": "approved_for_session"}}))
    );
}

// 9
#[tokio::test]
async fn auto_approves_mcp_tool_approval_prompts_when_approval_policy_is_never() {
    let h = Harness::new();
    let message = r#"{"id":110,"method":"item/tool/requestUserInput","params":{"itemId":"call-717","questions":[{"header":"Approve app tool call?","id":"mcp_tool_call_approval_call-717","isOther":false,"isSecret":false,"options":[{"description":"Run the tool and continue.","label":"Approve Once"},{"description":"Run the tool and remember this choice for this session.","label":"Approve this Session"},{"description":"Decline this tool call and continue.","label":"Deny"},{"description":"Cancel this tool call","label":"Cancel"}],"question":"The linear MCP server wants to run the tool \"Save issue\", which may modify or delete data. Allow this action?"}],"threadId":"thread-717","turnId":"turn-717"}}"#;
    run_with_message(
        &h,
        "MT-717",
        message,
        json!({"approval_policy": "never"}),
        Arc::new(LinearLikeTools::default()),
        &EventSink::none(),
    )
    .await
    .unwrap();
    let reply = h
        .traced_messages()
        .into_iter()
        .find(|m| m["id"] == json!(110))
        .expect("reply sent");
    assert_eq!(
        reply["result"]["answers"]["mcp_tool_call_approval_call-717"]["answers"],
        json!(["Approve this Session"])
    );
}

// 10
#[tokio::test]
async fn blocks_freeform_tool_input_prompts() {
    let h = Harness::new();
    let message = r#"{"id":111,"method":"item/tool/requestUserInput","params":{"itemId":"call-718","questions":[{"header":"Provide context","id":"freeform-718","isOther":false,"isSecret":false,"options":null,"question":"What comment should I post back to the issue?"}],"threadId":"thread-718","turnId":"turn-718"}}"#;
    let err = run_with_message(
        &h,
        "MT-718",
        message,
        json!({"approval_policy": "never"}),
        Arc::new(LinearLikeTools::default()),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    let CodexError::TurnInputRequired(payload) = err else {
        panic!("unexpected error: {err:?}");
    };
    assert_eq!(payload["method"], json!("item/tool/requestUserInput"));
    assert!(h.traced_messages().iter().all(|m| m["id"] != json!(111)));
}

// 11
#[tokio::test]
async fn blocks_option_based_tool_input_prompts() {
    let h = Harness::new();
    let message = r#"{"id":112,"method":"item/tool/requestUserInput","params":{"itemId":"call-719","questions":[{"header":"Choose an action","id":"options-719","isOther":false,"isSecret":false,"options":[{"description":"Proceed with the requested action.","label":"Allow"},{"description":"Do not proceed.","label":"Deny"}],"question":"How should I proceed?"}],"threadId":"thread-719","turnId":"turn-719"}}"#;
    let err = run_with_message(
        &h,
        "MT-719",
        message,
        json!({"approval_policy": "never"}),
        Arc::new(LinearLikeTools::default()),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    let CodexError::TurnInputRequired(payload) = err else {
        panic!("unexpected error: {err:?}");
    };
    assert_eq!(payload["method"], json!("item/tool/requestUserInput"));
}

#[tokio::test]
async fn mcp_tool_prompts_require_input_under_the_default_policy() {
    let h = Harness::new();
    let message = r#"{"id":113,"method":"item/tool/requestUserInput","params":{"questions":[{"id":"mcp_tool_call_approval_x","options":[{"label":"Approve Once"}]}]}}"#;
    let err = run_with_message(
        &h,
        "MT-720",
        message,
        json!({}),
        Arc::new(LinearLikeTools::default()),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    assert_eq!(err.tag(), "turn_input_required");
}

// 12
#[tokio::test]
async fn rejects_unsupported_dynamic_tool_calls_without_stalling() {
    let h = Harness::new();
    let mut events = Events::new();
    let message = r#"{"id":101,"method":"item/tool/call","params":{"tool":"some_tool","callId":"call-90","threadId":"thread-90","turnId":"turn-90","arguments":{}}}"#;
    run_with_message(
        &h,
        "MT-90",
        message,
        json!({}),
        Arc::new(LinearLikeTools::default()),
        &events.sink,
    )
    .await
    .unwrap();
    let reply = h
        .traced_messages()
        .into_iter()
        .find(|m| m["id"] == json!(101))
        .expect("reply sent");
    assert_eq!(reply["result"]["success"], json!(false));
    assert!(
        reply["result"]["output"]
            .as_str()
            .unwrap()
            .contains("Unsupported dynamic tool")
    );
    // A named but unknown tool is `tool_call_failed`, not `unsupported_tool_call` (parity, C.13 #7).
    assert!(kinds(&events.drain()).contains(&CodexEventKind::ToolCallFailed));
}

#[tokio::test]
async fn nameless_tool_calls_are_unsupported() {
    let h = Harness::new();
    let mut events = Events::new();
    let tools = Arc::new(LinearLikeTools::default());
    run_with_message(
        &h,
        "MT-90N",
        r#"{"id":104,"method":"item/tool/call","params":{"tool":"  ","arguments":null}}"#,
        json!({}),
        tools.clone(),
        &events.sink,
    )
    .await
    .unwrap();
    assert_eq!(*tools.calls.lock().unwrap(), vec![(None, json!({}))]);
    let reply = h
        .traced_messages()
        .into_iter()
        .find(|m| m["id"] == json!(104))
        .unwrap();
    assert!(
        reply["result"]["output"]
            .as_str()
            .unwrap()
            .contains("Unsupported dynamic tool: nil.")
    );
    assert!(kinds(&events.drain()).contains(&CodexEventKind::UnsupportedToolCall));
}

// 13
#[tokio::test]
async fn executes_supported_dynamic_tool_calls_and_returns_the_tool_result() {
    let h = Harness::new();
    let text = r#"{"data":{"viewer":{"id":"usr_123"}}}"#;
    let tools = Arc::new(LinearLikeTools {
        result: Some(
            json!({"success": true, "contentItems": [{"type": "inputText", "text": text}]}),
        ),
        ..LinearLikeTools::default()
    });
    let mut events = Events::new();
    let message = r#"{"id":102,"method":"item/tool/call","params":{"name":"linear_graphql","callId":"call-90a","threadId":"thread-90a","turnId":"turn-90a","arguments":{"query":"query Viewer { viewer { id } }","variables":{"includeTeams":false}}}}"#;
    run_with_message(
        &h,
        "MT-90A",
        message,
        json!({}),
        tools.clone(),
        &events.sink,
    )
    .await
    .unwrap();
    assert_eq!(
        *tools.calls.lock().unwrap(),
        vec![(
            Some("linear_graphql".to_owned()),
            json!({"query": "query Viewer { viewer { id } }", "variables": {"includeTeams": false}})
        )]
    );
    let reply = h
        .traced_messages()
        .into_iter()
        .find(|m| m["id"] == json!(102))
        .unwrap();
    assert_eq!(reply["result"]["success"], json!(true));
    assert_eq!(reply["result"]["output"], json!(text));
    assert!(kinds(&events.drain()).contains(&CodexEventKind::ToolCallCompleted));
}

// 14
#[tokio::test]
async fn emits_tool_call_failed_for_supported_tool_failures() {
    let h = Harness::new();
    let tools = Arc::new(LinearLikeTools {
        result: Some(
            json!({"success": false, "contentItems": [{"type": "inputText", "text": r#"{"error":{"message":"boom"}}"#}]}),
        ),
        ..LinearLikeTools::default()
    });
    let mut events = Events::new();
    let message = r#"{"id":103,"method":"item/tool/call","params":{"tool":"linear_graphql","callId":"call-90b","threadId":"thread-90b","turnId":"turn-90b","arguments":{"query":"query Viewer { viewer { id } }"}}}"#;
    run_with_message(
        &h,
        "MT-90B",
        message,
        json!({}),
        tools.clone(),
        &events.sink,
    )
    .await
    .unwrap();
    assert_eq!(
        tools.calls.lock().unwrap()[0],
        (
            Some("linear_graphql".to_owned()),
            json!({"query": "query Viewer { viewer { id } }"})
        )
    );
    let failed = events
        .drain()
        .into_iter()
        .find(|e| e.kind() == CodexEventKind::ToolCallFailed)
        .expect("tool_call_failed emitted");
    assert_eq!(
        failed.payload().unwrap()["params"]["tool"],
        json!("linear_graphql")
    );
}

#[tokio::test]
async fn slow_tool_calls_are_bounded_by_the_turn_timeout() {
    let h = Harness::new();
    let tools = Arc::new(LinearLikeTools {
        result: Some(json!({"success": true, "output": "late"})),
        delay: Some(Duration::from_secs(5)),
        ..LinearLikeTools::default()
    });
    let started = Instant::now();
    let message =
        r#"{"id":105,"method":"item/tool/call","params":{"tool":"linear_graphql","arguments":{}}}"#;
    run_with_message(
        &h,
        "MT-SLOW",
        message,
        json!({"turn_timeout_ms": 300}),
        tools,
        &EventSink::none(),
    )
    .await
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));
    let reply = h
        .traced_messages()
        .into_iter()
        .find(|m| m["id"] == json!(105))
        .unwrap();
    assert_eq!(reply["result"]["success"], json!(false));
    assert!(
        reply["result"]["output"]
            .as_str()
            .unwrap()
            .contains("timed out after 300ms")
    );
}

// 15
#[tokio::test]
async fn buffers_partial_json_lines_until_newline_terminator() {
    let h = Harness::new();
    let ws = h.workspace("MT-91");
    let options = h.options_for(&ws, fixture_command("partial_line.sh"), json!({}));
    run(options, "p", &issue("MT-91"), &EventSink::none())
        .await
        .unwrap();
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// 16
#[tokio::test(flavor = "current_thread")]
async fn captures_codex_side_output_and_logs_it() {
    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let h = Harness::new();
    let ws = h.workspace("MT-92");
    let mut events = Events::new();
    let options = h.options_for(&ws, fixture_command("stderr_noise.sh"), json!({}));
    let mut session = AppServerSession::start(options).await.unwrap();
    session
        .run_turn("p", &issue("MT-92"), &events.sink)
        .await
        .unwrap();
    session.stop().await;

    let kinds = kinds(&events.drain());
    assert!(kinds.contains(&CodexEventKind::TurnCompleted));
    assert!(!kinds.contains(&CodexEventKind::Malformed));
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("Codex turn stream output: warning: this is stderr noise"),
        "logs: {text}"
    );
    assert!(text.contains("WARN"));
}

#[tokio::test]
async fn stderr_tail_keeps_recent_lines() {
    let h = Harness::new();
    let ws = h.workspace("MT-92T");
    let options = h.options_for(&ws, fixture_command("stderr_noise.sh"), json!({}));
    let mut session = AppServerSession::start(options).await.unwrap();
    session
        .run_turn("p", &issue("MT-92T"), &EventSink::none())
        .await
        .unwrap();
    // The stderr reader runs concurrently; give it a moment before inspecting the tail.
    for _ in 0..50 {
        if !session.stderr_tail().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        session.stderr_tail(),
        vec!["warning: this is stderr noise".to_owned()]
    );
    session.stop().await;
}

// 17
#[tokio::test]
async fn emits_malformed_events_for_json_like_protocol_lines() {
    let h = Harness::new();
    let ws = h.workspace("MT-93");
    let mut events = Events::new();
    let options = h.options_for(&ws, fixture_command("malformed.sh"), json!({}));
    run(options, "p", &issue("MT-93"), &events.sink)
        .await
        .unwrap();
    let events = events.drain();
    assert_eq!(
        kinds(&events),
        vec![
            CodexEventKind::SessionStarted,
            CodexEventKind::Malformed,
            CodexEventKind::OtherMessage,
            CodexEventKind::TurnCompleted
        ]
    );
    assert_eq!(
        events[1].message_value(),
        Some(json!("{\"method\":\"turn/completed\""))
    );
    assert_eq!(events[2].payload(), Some(&json!([1, 2])));
}

// 18
#[tokio::test]
async fn does_not_pass_tracker_credentials_to_the_local_codex_child() {
    let h = Harness::new();
    let bash_home = h.path("bash-home");
    fs::create_dir_all(&bash_home).unwrap();
    fs::write(
        bash_home.join(".bash_profile"),
        "export LINEAR_API_KEY='profile-canonical-secret-that-must-not-reach-child'\n\
         export SYMP_CUSTOM_LINEAR_API_KEY='profile-custom-secret-that-must-not-reach-child'\n\
         export SYMP_TEST_BASH_PROFILE_LOADED=1\n",
    )
    .unwrap();
    let ws = h.workspace("MT-SECRET");
    let settings = settings(
        &h.workspace_root(),
        json!({"command": fixture_command("secret_env.sh")}),
        json!({"api_key": "$SYMP_CUSTOM_LINEAR_API_KEY"}),
    );
    let options = h
        .options(&ws, settings)
        .with_env("HOME", bash_home.to_string_lossy())
        .with_env(
            "LINEAR_API_KEY",
            "canonical-secret-that-must-not-reach-child",
        )
        .with_env(
            "SYMP_CUSTOM_LINEAR_API_KEY",
            "custom-secret-that-must-not-reach-child",
        );
    run(options, "p", &issue("MT-SECRET"), &EventSink::none())
        .await
        .unwrap();
    let trace = fs::read_to_string(h.trace()).unwrap();
    assert!(
        trace.contains("PROFILE_LOADED:1\n"),
        "login shell used: {trace}"
    );
    assert!(trace.contains("CANONICAL_SECRET:\n"));
    assert!(trace.contains("CUSTOM_SECRET:\n"));
    assert!(!trace.contains("secret-that-must-not-reach-child"));
}

/// The runtime's SSH launcher shape (`-T [-p PORT] DEST "bash -lc '<cmd>'"`), pointed at a fake `ssh`.
struct FakeSsh;

impl RemoteLauncher for FakeSsh {
    fn command(
        &self,
        worker_host: &str,
        remote_command: &str,
    ) -> Result<tokio::process::Command, CodexError> {
        let (dest, port) = worker_host
            .rsplit_once(':')
            .map_or((worker_host, None), |(d, p)| (d, Some(p)));
        let mut command = tokio::process::Command::new("sh");
        command.arg(fixture("fake_ssh.sh")).arg("-T");
        if let Some(port) = port {
            command.args(["-p", port]);
        }
        command
            .arg(dest)
            .arg(format!("bash -lc {}", shell_escape(remote_command)));
        Ok(command)
    }
}

// 19
#[tokio::test]
async fn launches_over_ssh_for_remote_workers() {
    let h = Harness::new();
    let remote_workspace = "/remote/workspaces/MT-REMOTE";
    let settings = settings(
        Path::new("/remote/workspaces"),
        json!({"command": "fake-remote-codex app-server"}),
        json!({}),
    );
    let ssh_trace = h.path("ssh.trace");
    let options = h
        .options(remote_workspace, settings)
        .with_worker_host(Some("worker-01:2200".into()), Some(Arc::new(FakeSsh)))
        .with_env("SYMP_TEST_SSH_TRACE", ssh_trace.to_string_lossy())
        .with_env("LINEAR_API_KEY", "local-secret");
    let mut events = Events::new();
    run(
        options,
        "Run remote worker",
        &issue("MT-REMOTE"),
        &events.sink,
    )
    .await
    .unwrap();

    let trace = fs::read_to_string(&ssh_trace).unwrap();
    let argv = trace.lines().find(|l| l.starts_with("ARGV:")).unwrap();
    assert!(argv.contains("-T -p 2200 worker-01 bash -lc"), "{argv}");
    assert!(argv.contains("cd "));
    assert!(argv.contains(remote_workspace));
    assert!(argv.contains("unset LINEAR_API_KEY"));
    assert!(argv.contains("exec "));
    assert!(argv.contains("fake-remote-codex app-server"));
    assert!(
        trace.contains("ENV_SECRET:\n"),
        "secrets are stripped from the local ssh too"
    );

    let messages: Vec<Value> = trace
        .lines()
        .filter_map(|l| l.strip_prefix("JSON:"))
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let thread_start = messages
        .iter()
        .find(|m| m["method"] == "thread/start")
        .unwrap();
    assert_eq!(thread_start["params"]["cwd"], json!(remote_workspace));
    let turn_start = messages
        .iter()
        .find(|m| m["method"] == "turn/start")
        .unwrap();
    assert_eq!(turn_start["params"]["cwd"], json!(remote_workspace));
    assert_eq!(
        turn_start["params"]["sandboxPolicy"],
        json!({"type": "workspaceWrite", "writableRoots": [remote_workspace], "readOnlyAccess": {"type": "fullAccess"},
            "networkAccess": false, "excludeTmpdirEnvVar": false, "excludeSlashTmp": false})
    );

    let events = events.drain();
    let started = &events[0];
    assert_eq!(started.kind(), CodexEventKind::SessionStarted);
    assert_eq!(started.worker_host.as_deref(), Some("worker-01:2200"));
    assert!(started.codex_app_server_pid.is_some());
    // Stream events never carry worker_host (parity).
    assert!(events[1..].iter().all(|e| e.worker_host.is_none()));
}

#[tokio::test]
async fn remote_sessions_require_a_launcher_and_a_clean_workspace() {
    let h = Harness::new();
    let settings = settings(Path::new("/remote"), json!({"command": "x"}), json!({}));
    let err = AppServerSession::start(
        h.options("/remote/ws", settings.clone())
            .with_worker_host(Some("host".into()), None),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err, CodexError::RemoteLauncherMissing("host".into()));

    let err = AppServerSession::start(
        h.options("  ", settings.clone())
            .with_worker_host(Some("host".into()), Some(Arc::new(FakeSsh))),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        err,
        CodexError::InvalidWorkspaceCwd(InvalidWorkspaceCwd::EmptyRemoteWorkspace("host".into()))
    );
    let err = AppServerSession::start(
        h.options("/a\nb", settings)
            .with_worker_host(Some("host".into()), Some(Arc::new(FakeSsh))),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(err.tag(), "invalid_workspace_cwd");
}

#[tokio::test]
async fn multiple_turns_reuse_request_id_three_on_one_thread() {
    let h = Harness::new();
    let ws = h.workspace("MT-MULTI");
    let options = h.options_for(&ws, fixture_command("multi_turn.sh"), json!({}));
    let mut events = Events::new();
    let mut session = AppServerSession::start(options).await.unwrap();
    assert_eq!(session.thread_id(), "thread-multi");
    let first = session
        .run_turn("first", &issue("MT-MULTI"), &events.sink)
        .await
        .unwrap();
    let second = session
        .run_turn("continue", &issue("MT-MULTI"), &events.sink)
        .await
        .unwrap();
    session.stop().await;
    assert_eq!(first.session_id, "thread-multi-turn-a");
    assert_eq!(second.session_id, "thread-multi-turn-b");
    assert_eq!(second.thread_id, "thread-multi");

    let turn_starts: Vec<Value> = h
        .traced_messages()
        .into_iter()
        .filter(|m| m["method"] == "turn/start")
        .collect();
    assert_eq!(turn_starts.len(), 2);
    assert!(turn_starts.iter().all(|m| m["id"] == json!(3)));
    assert_eq!(
        turn_starts[1]["params"]["input"][0]["text"],
        json!("continue")
    );

    let mut accumulator = TokenAccumulator::default();
    let mut session_ids = Vec::new();
    for event in events.drain() {
        accumulator.apply(event.token_usage.as_ref());
        if let Some(id) = event.session_id() {
            session_ids.push(id.to_owned());
        }
    }
    assert_eq!(
        session_ids,
        vec!["thread-multi-turn-a", "thread-multi-turn-b"]
    );
    // The smaller turn/completed `usage` of turn 2 does not regress the thread totals.
    assert_eq!(
        accumulator.totals,
        TokenCounts {
            input_tokens: 10,
            output_tokens: 4,
            total_tokens: 14
        }
    );
}

#[tokio::test]
async fn thread_start_errors_and_invalid_payloads_fail_startup() {
    let h = Harness::new();
    let ws = h.workspace("MT-ERR");
    let cases = [
        (
            "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"error\":{\"code\":-32000,\"message\":\"nope\"}}\n",
            CodexError::ResponseError(json!({"code": -32000, "message": "nope"})),
        ),
        (
            "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2}\n",
            CodexError::ResponseError(json!({"id": 2})),
        ),
        (
            "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{\"thread\":{\"name\":\"x\"}}}\n",
            CodexError::InvalidThreadPayload(json!({"name": "x"})),
        ),
        (
            "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{}}\n",
            CodexError::InvalidThreadPayload(json!({})),
        ),
        ("0 @exit 3\n", CodexError::PortExit(3)),
    ];
    for (script, expected) in cases {
        let options = h.options_for(&ws, h.scripted(script), json!({}));
        let err = AppServerSession::start(options).await.err().unwrap();
        assert_eq!(err, expected, "script: {script}");
    }
}

#[tokio::test]
async fn startup_ignores_non_object_json_and_noise_before_the_response() {
    let h = Harness::new();
    let ws = h.workspace("MT-NOISE");
    let script = "1 123\n1 [1]\n1 not json\n1 {\"id\":\"1\",\"result\":{}}\n1 {\"id\":1,\"result\":{}}\n\
                  3 {\"id\":2,\"result\":{\"thread\":{\"id\":\"t\"}}}\n\
                  4 {\"id\":3,\"result\":{\"turn\":{\"id\":\"u\"}}}\n4 {\"method\":\"turn/completed\"}\n";
    let options = h.options_for(&ws, h.scripted(script), json!({}));
    let outcome = run(options, "p", &issue("MT-NOISE"), &EventSink::none())
        .await
        .unwrap();
    assert_eq!(outcome.session_id, "t-u");
}

#[tokio::test]
async fn response_timeout_when_the_server_never_answers() {
    let h = Harness::new();
    let ws = h.workspace("MT-RT");
    let options = h.options_for(
        &ws,
        h.scripted("1 {\"method\":\"noise\"}\n"),
        json!({"read_timeout_ms": 150}),
    );
    let err = AppServerSession::start(options).await.err().unwrap();
    assert_eq!(err, CodexError::ResponseTimeout);
}

#[tokio::test]
async fn invalid_turn_payload_emits_startup_failed() {
    let h = Harness::new();
    let ws = h.workspace("MT-TP");
    let script = "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{\"thread\":{\"id\":\"t\"}}}\n\
                  4 {\"id\":3,\"result\":{\"turn\":{}}}\n";
    let options = h.options_for(&ws, h.scripted(script), json!({}));
    let mut events = Events::new();
    let err = run(options, "p", &issue("MT-TP"), &events.sink)
        .await
        .unwrap_err();
    assert_eq!(err, CodexError::InvalidTurnPayload(json!({"turn": {}})));
    let events = events.drain();
    assert_eq!(kinds(&events), vec![CodexEventKind::StartupFailed]);
    assert_eq!(events[0].reason(), Some(&err));
}

#[tokio::test]
async fn process_exit_during_a_turn_is_port_exit_and_ends_with_error() {
    let h = Harness::new();
    let ws = h.workspace("MT-EXIT");
    let script = "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{\"thread\":{\"id\":\"t\"}}}\n\
                  4 {\"id\":3,\"result\":{\"turn\":{\"id\":\"u\"}}}\n4 {\"method\":\"item/started\"}\n4 @exit 0\n";
    let options = h.options_for(&ws, h.scripted(script), json!({}));
    let mut events = Events::new();
    let err = run(options, "p", &issue("MT-EXIT"), &events.sink)
        .await
        .unwrap_err();
    assert_eq!(err, CodexError::PortExit(0));
    let events = events.drain();
    assert_eq!(
        kinds(&events),
        vec![
            CodexEventKind::SessionStarted,
            CodexEventKind::Notification,
            CodexEventKind::TurnEndedWithError
        ]
    );
    assert_eq!(events[2].session_id(), Some("t-u"));
    assert_eq!(events[2].reason(), Some(&CodexError::PortExit(0)));
}

#[tokio::test]
async fn turn_failed_and_cancelled_need_params() {
    let h = Harness::new();
    let ws = h.workspace("MT-FAIL");
    let handshake = "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{\"thread\":{\"id\":\"t\"}}}\n\
                     4 {\"id\":3,\"result\":{\"turn\":{\"id\":\"u\"}}}\n";
    let failed =
        format!("{handshake}4 {{\"method\":\"turn/failed\",\"params\":{{\"error\":\"boom\"}}}}\n");
    let mut events = Events::new();
    let err = run(
        h.options_for(&ws, h.scripted(&failed), json!({})),
        "p",
        &issue("MT-FAIL"),
        &events.sink,
    )
    .await
    .unwrap_err();
    assert_eq!(err, CodexError::TurnFailed(json!({"error": "boom"})));
    assert_eq!(
        kinds(&events.drain()),
        vec![
            CodexEventKind::SessionStarted,
            CodexEventKind::TurnFailed,
            CodexEventKind::TurnEndedWithError
        ]
    );

    let cancelled = format!("{handshake}4 {{\"method\":\"turn/cancelled\",\"params\":null}}\n");
    let err = run(
        h.options_for(&ws, h.scripted(&cancelled), json!({})),
        "p",
        &issue("MT-FAIL"),
        &EventSink::none(),
    )
    .await
    .unwrap_err();
    assert_eq!(err, CodexError::TurnCancelled(Value::Null));

    let paramless = format!(
        "{handshake}4 {{\"method\":\"turn/failed\"}}\n4 {{\"method\":\"turn/completed\"}}\n"
    );
    let mut events = Events::new();
    run(
        h.options_for(&ws, h.scripted(&paramless), json!({})),
        "p",
        &issue("MT-FAIL"),
        &events.sink,
    )
    .await
    .unwrap();
    assert_eq!(
        kinds(&events.drain()),
        vec![
            CodexEventKind::SessionStarted,
            CodexEventKind::Notification,
            CodexEventKind::TurnCompleted
        ]
    );
}

#[tokio::test]
async fn server_requests_before_the_turn_start_response_are_not_dropped() {
    let h = Harness::new();
    let ws = h.workspace("MT-EARLY");
    // The approval request arrives before the `turn/start` result; Elixir dropped it (C.13 #13).
    let script = "1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{\"thread\":{\"id\":\"t\"}}}\n\
                  4 {\"id\":7,\"method\":\"item/fileChange/requestApproval\",\"params\":{}}\n\
                  4 {\"method\":\"turn/started\",\"params\":{}}\n\
                  4 {\"id\":3,\"result\":{\"turn\":{\"id\":\"u\"}}}\n\
                  5 {\"method\":\"turn/completed\"}\n";
    let mut events = Events::new();
    run(
        h.options_for(&ws, h.scripted(script), json!({"approval_policy": "never"})),
        "p",
        &issue("MT-EARLY"),
        &events.sink,
    )
    .await
    .unwrap();
    assert!(
        h.traced_messages()
            .iter()
            .any(|m| m == &json!({"id": 7, "result": {"decision": "acceptForSession"}}))
    );
    assert_eq!(
        kinds(&events.drain()),
        vec![
            CodexEventKind::SessionStarted,
            CodexEventKind::ApprovalAutoApproved,
            CodexEventKind::Notification,
            CodexEventKind::TurnCompleted
        ]
    );
}

fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

async fn wait_until_dead(pid: &str) -> bool {
    for _ in 0..100 {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn stop_kills_the_whole_process_group() {
    let h = Harness::new();
    let ws = h.workspace("MT-PG");
    let pidfile = h.path("grandchild.pid");
    // The leader exits on EOF but leaves a grandchild behind in its process group.
    let script = format!(
        "0 @spawn {}\n1 {{\"id\":1,\"result\":{{}}}}\n3 {{\"id\":2,\"result\":{{\"thread\":{{\"id\":\"t\"}}}}}}\n",
        pidfile.display()
    );
    let session = AppServerSession::start(h.options_for(&ws, h.scripted(&script), json!({})))
        .await
        .unwrap();
    let grandchild = fs::read_to_string(&pidfile).unwrap().trim().to_owned();
    assert!(process_alive(&grandchild));
    session.stop().await;
    assert!(
        wait_until_dead(&grandchild).await,
        "grandchild {grandchild} survived stop"
    );
}

#[tokio::test]
async fn stop_kills_servers_that_ignore_stdin_eof() {
    let h = Harness::new();
    let ws = h.workspace("MT-EOF");
    let script = "0 @ignore-eof\n1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{\"thread\":{\"id\":\"t\"}}}\n";
    let session = AppServerSession::start(h.options_for(&ws, h.scripted(script), json!({})))
        .await
        .unwrap();
    let pid = session.codex_app_server_pid().unwrap().to_owned();
    let started = Instant::now();
    session.stop().await;
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        wait_until_dead(&pid).await,
        "app-server {pid} survived stop"
    );
}

#[tokio::test]
async fn dropping_a_session_kills_the_process_group() {
    let h = Harness::new();
    let ws = h.workspace("MT-DROP");
    let script = "0 @ignore-eof\n1 {\"id\":1,\"result\":{}}\n3 {\"id\":2,\"result\":{\"thread\":{\"id\":\"t\"}}}\n";
    let session = AppServerSession::start(h.options_for(&ws, h.scripted(script), json!({})))
        .await
        .unwrap();
    let pid = session.codex_app_server_pid().unwrap().to_owned();
    drop(session);
    assert!(
        wait_until_dead(&pid).await,
        "app-server {pid} survived drop"
    );
}
