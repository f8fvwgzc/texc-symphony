//! Contract tests of the `claude_code` backend against a fake `claude` (tests/fixtures/claude_fake.sh)
//! that records its argv, stdin and environment and replays recorded stream-json lines.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use symphony_agents::AgentBackend;
use symphony_agents::claude::ClaudeCodeBackend;
use symphony_codex::{CodexError, CodexEvent, CodexEventKind, EventSink, StartOptions};
use symphony_core::config::{BackendKind, BackendSettings, ClaudeCodeSettings};
use symphony_core::{Issue, MapEnv, Settings};
use tempfile::TempDir;
use tokio::sync::mpsc;

struct Fixture {
    dir: TempDir,
    workspace: PathBuf,
    state: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("root/TASK-1");
    let state = dir.path().join("state");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    Fixture {
        dir,
        workspace,
        state,
    }
}

fn fake_claude() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude_fake.sh")
        .display()
        .to_string()
}

fn backend_settings(claude: ClaudeCodeSettings, turn_timeout_ms: u64) -> BackendSettings {
    BackendSettings {
        name: "claude".into(),
        kind: BackendKind::ClaudeCode(claude),
        model: Some("sonnet".into()),
        prompt_template: None,
        system_prompt: None,
        pricing: None,
        turn_timeout_ms,
    }
}

fn claude_settings() -> ClaudeCodeSettings {
    ClaudeCodeSettings {
        command: fake_claude(),
        allowed_tools: vec!["Read".into(), "Bash(git log:*)".into()],
        ..ClaudeCodeSettings::default()
    }
}

impl Fixture {
    fn settings(&self) -> Arc<Settings> {
        let mut settings = Settings::default();
        settings.workspace.root = self.dir.path().join("root").display().to_string();
        settings.tracker.kind = Some("github".into());
        Arc::new(settings)
    }

    fn options(&self) -> StartOptions {
        StartOptions::new(
            self.workspace.display().to_string(),
            self.settings(),
            self.dir.path().join("WORKFLOW.md"),
        )
        .with_env("FAKE_CLAUDE_DIR", self.state.display().to_string())
        .with_env("GITHUB_TOKEN", "ghp_must_not_reach_claude")
    }

    fn write(&self, name: &str, content: &str) {
        std::fs::write(self.state.join(name), content).unwrap();
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.state.join(name)).unwrap_or_default()
    }

    fn argv(&self, turn: u32) -> Vec<String> {
        self.read(&format!("argv.{turn}"))
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn issue() -> Issue {
    Issue {
        id: Some("task:1".into()),
        identifier: Some("TASK-1".into()),
        title: Some("Say hi".into()),
        ..Issue::default()
    }
}

fn events() -> (EventSink, mpsc::UnboundedReceiver<CodexEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (EventSink::new(tx), rx)
}

fn drain(rx: &mut mpsc::UnboundedReceiver<CodexEvent>) -> Vec<CodexEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

const TURN_ONE: &str = r#"{"type":"system","subtype":"init","session_id":"sess-1","model":"sonnet"}
{"type":"assistant","message":{"content":[{"type":"text","text":"Working on it."},{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"README.md"}]}}
this line is not JSON
{"type":"result","subtype":"success","is_error":false,"result":"All done.","session_id":"sess-1","usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":3}}
"#;

const TURN_TWO: &str = r#"{"type":"system","subtype":"init","session_id":"sess-1"}
{"type":"result","subtype":"success","is_error":false,"result":"Still done.","session_id":"sess-1","usage":{"input_tokens":1,"output_tokens":1}}
"#;

#[tokio::test]
async fn turns_stream_events_usage_and_resume_the_claude_session() {
    let f = fixture();
    f.write("turn.1.jsonl", TURN_ONE);
    f.write("turn.2.jsonl", TURN_TWO);
    let env = MapEnv::new().with("MY_CLAUDE_KEY", "sk-ant-test-key-0123456789");
    let mut claude = claude_settings();
    claude.api_key_env = Some("MY_CLAUDE_KEY".into());
    let backend = ClaudeCodeBackend::new(backend_settings(claude, 60_000)).with_env(Arc::new(env));
    let mut session = backend.start(f.options()).await.unwrap();
    assert_eq!(session.pid(), None);
    let (sink, mut rx) = events();

    let first = session
        .run_turn("Please say hi.\n", &issue(), &sink)
        .await
        .unwrap();
    assert_eq!(first.final_message.as_deref(), Some("All done."));
    assert_eq!(first.turn_id, "turn-1");
    assert_eq!(first.session_id, format!("{}-turn-1", first.thread_id));

    let argv = f.argv(1);
    for expected in [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        "sonnet",
        "--permission-mode",
        "acceptEdits",
        "--allowedTools",
        "Read",
        "Bash(git log:*)",
    ] {
        assert!(argv.iter().any(|a| a == expected), "{expected} in {argv:?}");
    }
    assert!(!argv.iter().any(|a| a == "--resume"), "{argv:?}");
    assert_eq!(f.read("stdin.1"), "Please say hi.\n");
    let env = f.read("env.1");
    assert!(
        !env.lines().any(|l| l.starts_with("GITHUB_TOKEN=")),
        "{env}"
    );
    assert!(
        env.lines()
            .any(|l| l == "ANTHROPIC_API_KEY=sk-ant-test-key-0123456789")
    );
    assert!(f.workspace.canonicalize().unwrap().ends_with("root/TASK-1"));

    let events = drain(&mut rx);
    let kinds: Vec<(CodexEventKind, Option<&str>)> =
        events.iter().map(|e| (e.kind(), e.method())).collect();
    assert_eq!(
        kinds,
        vec![
            (CodexEventKind::SessionStarted, None),
            (CodexEventKind::Notification, Some("item/completed")),
            (CodexEventKind::Notification, Some("item/started")),
            (CodexEventKind::Notification, Some("item/completed")),
            (
                CodexEventKind::Notification,
                Some("thread/tokenUsage/updated")
            ),
            (CodexEventKind::TurnCompleted, Some("turn/completed")),
        ]
    );
    assert_eq!(
        events[1].payload().unwrap()["params"]["item"]["text"],
        "Working on it."
    );
    assert_eq!(events[4].token_usage.unwrap().total_tokens, Some(18));

    let second = session
        .run_turn("Continue.", &issue(), &sink)
        .await
        .unwrap();
    assert_eq!(second.final_message.as_deref(), Some("Still done."));
    assert_eq!(second.thread_id, first.thread_id);
    let argv = f.argv(2);
    let resume = argv.iter().position(|a| a == "--resume").expect("--resume");
    assert_eq!(argv[resume + 1], "sess-1");
    let usage = drain(&mut rx)
        .into_iter()
        .find_map(|e| e.token_usage)
        .unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens, usage.total_tokens),
        (Some(16), Some(4), Some(20)),
        "usage is cumulative over the session"
    );
    session.stop().await;
}

#[tokio::test]
async fn an_error_result_fails_the_turn() {
    let f = fixture();
    f.write(
        "turn.jsonl",
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom","usage":{"input_tokens":2,"output_tokens":0}}
"#,
    );
    let backend = ClaudeCodeBackend::new(backend_settings(claude_settings(), 60_000));
    let mut session = backend.start(f.options()).await.unwrap();
    let (sink, mut rx) = events();
    let err = session.run_turn("x", &issue(), &sink).await.unwrap_err();
    match &err {
        CodexError::Backend(message) => {
            assert!(
                message.contains("error_during_execution: boom"),
                "{message}"
            )
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(err.tag(), "agent_backend_failed");
    let kinds: Vec<CodexEventKind> = drain(&mut rx).iter().map(CodexEvent::kind).collect();
    assert_eq!(kinds.last(), Some(&CodexEventKind::TurnEndedWithError));
    assert!(
        kinds.contains(&CodexEventKind::Notification),
        "usage still reported"
    );
}

#[tokio::test]
async fn exiting_without_a_result_reports_the_status_and_stderr() {
    let f = fixture();
    f.write(
        "turn.1.sh",
        "echo 'Invalid API key · Please run /login' >&2\nexit 3\n",
    );
    let backend = ClaudeCodeBackend::new(backend_settings(claude_settings(), 60_000));
    let mut session = backend.start(f.options()).await.unwrap();
    let (sink, mut rx) = events();
    let err = session.run_turn("x", &issue(), &sink).await.unwrap_err();
    let CodexError::Backend(message) = &err else {
        panic!("unexpected {err:?}")
    };
    assert!(message.contains("status 3"), "{message}");
    assert!(message.contains("Invalid API key"), "{message}");
    let events = drain(&mut rx);
    assert_eq!(
        events.last().map(CodexEvent::kind),
        Some(CodexEventKind::TurnEndedWithError)
    );
}

#[tokio::test]
async fn a_silent_turn_times_out() {
    let f = fixture();
    f.write("turn.1.sh", "sleep 30\n");
    let backend = ClaudeCodeBackend::new(backend_settings(claude_settings(), 300));
    let mut session = backend.start(f.options()).await.unwrap();
    let (sink, _rx) = events();
    let started = std::time::Instant::now();
    let err = session.run_turn("x", &issue(), &sink).await.unwrap_err();
    assert_eq!(err, CodexError::TurnTimeout);
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[tokio::test]
async fn dropping_a_running_turn_kills_its_process_group() {
    let f = fixture();
    f.write(
        "turn.1.sh",
        "sleep 300 &\necho $! > \"$dir/bg.pid\"\necho '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}'\nwait\n",
    );
    let backend = ClaudeCodeBackend::new(backend_settings(claude_settings(), 60_000));
    let mut session = backend.start(f.options()).await.unwrap();
    let turn = tokio::spawn(async move {
        let (sink, _rx) = events();
        session.run_turn("x", &issue(), &sink).await
    });
    let mut pid = None;
    for _ in 0..500 {
        if let Ok(text) = std::fs::read_to_string(f.state.join("bg.pid"))
            && let Ok(found) = text.trim().parse::<i32>()
        {
            pid = Some(found);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = pid.expect("the fake claude started its background child");
    turn.abort();
    let _ = turn.await;
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    for _ in 0..250 {
        if rustix::process::test_kill_process(pid).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the turn's background process survived");
}

#[tokio::test]
async fn ssh_workers_and_foreign_workspaces_are_refused() {
    let f = fixture();
    let backend = ClaudeCodeBackend::new(backend_settings(claude_settings(), 60_000));
    let remote = f.options().with_worker_host(Some("worker-1".into()), None);
    let err = backend.start(remote).await.err().unwrap();
    assert_eq!(err.tag(), "agent_backend_unsupported");

    let outside = StartOptions::new(
        f.state.display().to_string(),
        f.settings(),
        f.dir.path().join("WORKFLOW.md"),
    );
    let err = backend.start(outside).await.err().unwrap();
    assert_eq!(err.tag(), "invalid_workspace_cwd");
}

/// A handler with one tracker tool, so the session starts the MCP bridge.
struct OneTool;

#[async_trait::async_trait]
impl symphony_codex::DynamicToolHandler for OneTool {
    fn tool_specs(&self) -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({"name": "linear_graphql", "description": "Linear",
            "inputSchema": {"type": "object"}}),
        ]
    }

    fn secret_environment_names(&self) -> Vec<String> {
        vec!["LINEAR_API_KEY".into()]
    }

    async fn execute(
        &self,
        _tool: Option<&str>,
        _arguments: serde_json::Value,
        _issue: &Issue,
    ) -> serde_json::Value {
        symphony_codex::dynamic_tool::tool_response(true, "{}")
    }
}

#[tokio::test]
async fn tracker_tools_are_bridged_through_an_mcp_config() {
    let f = fixture();
    f.write("turn.jsonl", TURN_TWO);
    let backend = ClaudeCodeBackend::new(backend_settings(claude_settings(), 60_000))
        .with_tool_bridge(Some(PathBuf::from("/opt/symphony/bin/symphony")));
    let options = f
        .options()
        .with_tool_handler(Arc::new(OneTool))
        .with_env("LINEAR_API_KEY", "lin_api_must_not_reach_claude");
    let mut session = backend.start(options).await.unwrap();
    let (sink, _rx) = events();
    session.run_turn("x", &issue(), &sink).await.unwrap();

    let argv = f.argv(1);
    let config = argv
        .iter()
        .position(|a| a == "--mcp-config")
        .map(|i| PathBuf::from(&argv[i + 1]))
        .expect("--mcp-config");
    assert!(argv.iter().any(|a| a == "--strict-mcp-config"), "{argv:?}");
    assert!(
        argv.iter().any(|a| a == "mcp__symphony__linear_graphql"),
        "{argv:?}"
    );
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        body["mcpServers"]["symphony"]["command"],
        "/opt/symphony/bin/symphony"
    );
    assert_eq!(body["mcpServers"]["symphony"]["args"][0], "agent-tools");
    let env = f.read("env.1");
    assert!(
        !env.lines().any(|l| l.starts_with("LINEAR_API_KEY=")),
        "{env}"
    );

    session.stop().await;
    assert!(!config.exists(), "the bridge files go with the session");
}

#[tokio::test]
async fn the_llm_contract_points_claude_code_at_the_gateway() {
    use symphony_core::config::{LlmConnection, LlmGateway, LlmProvider, SecretSource};
    let f = fixture();
    f.write("turn.jsonl", TURN_TWO);
    let token = f.dir.path().join("gateway-token");
    std::fs::write(&token, "gw-token-0123456789\n").unwrap();
    let mut settings = (*f.settings()).clone();
    settings.llm = Some(LlmConnection {
        provider: LlmProvider::Anthropic,
        base_url: None,
        api_key: Some(SecretSource::Env("LLM_API_KEY".into())),
        model: "claude-sonnet-4-5".into(),
        model_small: Some("claude-haiku-4-5".into()),
        timeout_secs: Some(90),
        max_retries: Some(2),
        gateway: Some(LlmGateway {
            url: "http://gateway.platform:8080".into(),
            token: Some(SecretSource::File(token)),
        }),
    });
    let mut options = f
        .options()
        .with_env("ANTHROPIC_API_KEY", "sk-ant-stray-key-0123456789");
    options.settings = Arc::new(settings);
    let backend = ClaudeCodeBackend::new(backend_settings(claude_settings(), 60_000)).with_env(
        Arc::new(MapEnv::new().with("LLM_API_KEY", "provider-key-unused")),
    );
    let mut session = backend.start(options).await.unwrap();
    let (sink, _rx) = events();
    session.run_turn("hi", &issue(), &sink).await.unwrap();
    let env = f.read("env.1");
    for line in [
        "ANTHROPIC_BASE_URL=http://gateway.platform:8080",
        "ANTHROPIC_AUTH_TOKEN=gw-token-0123456789",
        "ANTHROPIC_SMALL_FAST_MODEL=claude-haiku-4-5",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL=claude-haiku-4-5",
        "API_TIMEOUT_MS=90000",
    ] {
        assert!(env.lines().any(|l| l == line), "missing {line}");
    }
    assert!(
        !env.contains("ANTHROPIC_API_KEY="),
        "no provider key bypasses the gateway"
    );
    assert!(!env.contains("provider-key-unused"));
    session.stop().await;

    // A backend's own key wins over the contract.
    let f = fixture();
    f.write("turn.jsonl", TURN_TWO);
    let mut settings = (*f.settings()).clone();
    settings.llm = Some(LlmConnection {
        provider: LlmProvider::Anthropic,
        base_url: Some("https://anthropic.proxy.example".into()),
        api_key: Some(SecretSource::Env("LLM_API_KEY".into())),
        model: "claude-sonnet-4-5".into(),
        model_small: None,
        timeout_secs: None,
        max_retries: None,
        gateway: None,
    });
    let mut options = f.options();
    options.settings = Arc::new(settings.clone());
    let backend = ClaudeCodeBackend::new(backend_settings(claude_settings(), 60_000)).with_env(
        Arc::new(MapEnv::new().with("LLM_API_KEY", "sk-ant-contract-0123456789")),
    );
    let mut session = backend.start(options).await.unwrap();
    session.run_turn("hi", &issue(), &sink).await.unwrap();
    let env = f.read("env.1");
    assert!(
        env.lines()
            .any(|l| l == "ANTHROPIC_API_KEY=sk-ant-contract-0123456789")
    );
    assert!(
        env.lines()
            .any(|l| l == "ANTHROPIC_BASE_URL=https://anthropic.proxy.example")
    );
    session.stop().await;

    let f = fixture();
    f.write("turn.jsonl", TURN_TWO);
    let mut options = f.options();
    let mut own = (*f.settings()).clone();
    own.llm = settings.llm;
    options.settings = Arc::new(own);
    let mut claude = claude_settings();
    claude.api_key_env = Some("MY_CLAUDE_KEY".into());
    let backend = ClaudeCodeBackend::new(backend_settings(claude, 60_000)).with_env(Arc::new(
        MapEnv::new()
            .with("LLM_API_KEY", "sk-ant-contract-0123456789")
            .with("MY_CLAUDE_KEY", "sk-ant-own-0123456789"),
    ));
    let mut session = backend.start(options).await.unwrap();
    session.run_turn("hi", &issue(), &sink).await.unwrap();
    let env = f.read("env.1");
    assert!(
        env.lines()
            .any(|l| l == "ANTHROPIC_API_KEY=sk-ant-own-0123456789")
    );
    assert!(
        !env.contains("anthropic.proxy.example"),
        "the contract's endpoint is not used with the backend's own key"
    );
    session.stop().await;
}
