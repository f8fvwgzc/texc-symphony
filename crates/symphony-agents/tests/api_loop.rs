//! The `api` backend against wiremock servers speaking both wire formats.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use symphony_agents::AgentBackend;
use symphony_agents::api::ApiBackend;
use symphony_codex::dynamic_tool::tool_response;
use symphony_codex::{
    CodexError, CodexEvent, CodexEventKind, DynamicToolHandler, EventSink, StartOptions,
};
use symphony_core::config::{
    ApiBackendSettings, ApiProvider, BackendKind, BackendSettings, SandboxMode,
};
use symphony_core::{Issue, MapEnv, Settings};
use tokio::sync::mpsc;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "sk-test-0123456789";

struct Env {
    _root: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Env {
    fn new() -> Self {
        let root_dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root_dir.path()).unwrap();
        let workspace = root.join("MT-1");
        std::fs::create_dir(&workspace).unwrap();
        Self {
            _root: root_dir,
            root,
            workspace,
        }
    }

    fn settings(&self, tracker_kind: Option<&str>) -> Arc<Settings> {
        let mut settings = Settings::default();
        settings.workspace.root = self.root.to_string_lossy().into_owned();
        settings.tracker.kind = tracker_kind.map(str::to_owned);
        Arc::new(settings)
    }

    fn options(&self, settings: Arc<Settings>) -> StartOptions {
        StartOptions::new(
            self.workspace.to_string_lossy().into_owned(),
            settings,
            self.root.join("WORKFLOW.md"),
        )
    }
}

fn api(provider: ApiProvider, base_url: &str, stream: bool) -> ApiBackendSettings {
    ApiBackendSettings {
        provider,
        base_url: base_url.to_owned(),
        api_key_env: Some("TEST_API_KEY".into()),
        api_key_file: None,
        max_retries: 3,
        headers: [("x-extra".to_owned(), "yes".to_owned())].into(),
        tools: [
            "read_file",
            "write_file",
            "edit_file",
            "list_files",
            "shell",
            "tracker",
        ]
        .iter()
        .map(|t| (*t).to_owned())
        .collect(),
        max_steps: 5,
        max_output_tokens: 256,
        temperature: None,
        stream,
        request_timeout_ms: 10_000,
        sandbox: SandboxMode::None,
        network: false,
        shell_timeout_ms: 10_000,
    }
}

fn backend_settings(api: ApiBackendSettings) -> BackendSettings {
    BackendSettings {
        name: "test".into(),
        kind: BackendKind::Api(api),
        model: Some("test-model".into()),
        prompt_template: None,
        system_prompt: Some("You are a test agent.".into()),
        pricing: None,
        turn_timeout_ms: 30_000,
    }
}

fn backend(settings: BackendSettings) -> ApiBackend {
    ApiBackend::new(settings)
        .with_env(Arc::new(MapEnv::new().with("TEST_API_KEY", KEY)))
        .with_retry_base(Duration::from_millis(1))
}

fn issue() -> Issue {
    Issue {
        id: Some("i-1".into()),
        identifier: Some("MT-1".into()),
        ..Issue::default()
    }
}

fn sink() -> (EventSink, mpsc::UnboundedReceiver<CodexEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (EventSink::new(tx), rx)
}

fn drain(rx: &mut mpsc::UnboundedReceiver<CodexEvent>) -> Vec<CodexEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn sse(events: &[Value], named: bool) -> String {
    let mut body = String::new();
    for event in events {
        if named && let Some(kind) = event.get("type").and_then(Value::as_str) {
            body.push_str(&format!("event: {kind}\n"));
        }
        body.push_str(&format!("data: {event}\n\n"));
    }
    body
}

fn event_stream(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

// ----- canned responses -------------------------------------------------------------------------

fn anthropic_tool_use(stream: bool, name: &str, input: Value) -> ResponseTemplate {
    if !stream {
        return ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{"type": "tool_use", "id": "toolu_1", "name": name, "input": input}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "cache_read_input_tokens": 0, "output_tokens": 12},
        }));
    }
    event_stream(sse(
        &[
            json!({"type": "message_start", "message": {"id": "msg_1",
                "usage": {"input_tokens": 20, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block":
                {"type": "tool_use", "id": "toolu_1", "name": name, "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta":
                {"type": "input_json_delta", "partial_json": input.to_string()}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
                "usage": {"output_tokens": 12}}),
            json!({"type": "message_stop"}),
        ],
        true,
    ))
}

fn anthropic_text(stream: bool, text: &str) -> ResponseTemplate {
    if !stream {
        return ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_2", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 30, "cache_creation_input_tokens": 10, "output_tokens": 5},
        }));
    }
    let (first, rest) = text.split_at(text.len() / 2);
    event_stream(sse(
        &[
            json!({"type": "message_start", "message": {"id": "msg_2",
                "usage": {"input_tokens": 30, "cache_creation_input_tokens": 10, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
            json!({"type": "ping"}),
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": first}}),
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": rest}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ],
        true,
    ))
}

fn openai_tool_call(stream: bool, name: &str, arguments: Value) -> ResponseTemplate {
    if !stream {
        return ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()}}]}}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 12, "total_tokens": 32},
        }));
    }
    let raw = arguments.to_string();
    let (first, rest) = raw.split_at(raw.len() / 2);
    let mut body = sse(
        &[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": name, "arguments": first}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0,
                "function": {"arguments": rest}}]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 12}}),
        ],
        false,
    );
    body.push_str("data: [DONE]\n\n");
    event_stream(body)
}

fn openai_text(stream: bool, text: &str) -> ResponseTemplate {
    if !stream {
        return ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-2", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": text}}],
            "usage": {"prompt_tokens": 40, "completion_tokens": 5},
        }));
    }
    let (first, rest) = text.split_at(text.len() / 2);
    let mut body = sse(
        &[
            json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": first}}]}),
            json!({"choices": [{"index": 0, "delta": {"content": rest}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 40, "completion_tokens": 5}}),
        ],
        false,
    );
    body.push_str("data: [DONE]\n\n");
    event_stream(body)
}

fn endpoint(provider: ApiProvider) -> &'static str {
    match provider {
        ApiProvider::Anthropic => "/v1/messages",
        ApiProvider::OpenAi => "/chat/completions",
    }
}

/// Mounts `first` for the first request and `then` for every later one.
async fn mount_sequence(server: &MockServer, provider: ApiProvider, steps: Vec<ResponseTemplate>) {
    let count = steps.len();
    for (index, response) in steps.into_iter().enumerate() {
        let mock = Mock::given(method("POST")).and(path(endpoint(provider)));
        let mock = if index + 1 < count {
            mock.respond_with(response).up_to_n_times(1)
        } else {
            mock.respond_with(response)
        };
        // Earlier steps win while they still have calls left.
        mock.with_priority(u8::try_from(index + 1).unwrap())
            .mount(server)
            .await;
    }
}

fn request_json(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

fn tool_use_response(
    provider: ApiProvider,
    stream: bool,
    name: &str,
    input: Value,
) -> ResponseTemplate {
    match provider {
        ApiProvider::Anthropic => anthropic_tool_use(stream, name, input),
        ApiProvider::OpenAi => openai_tool_call(stream, name, input),
    }
}

fn text_response(provider: ApiProvider, stream: bool, text: &str) -> ResponseTemplate {
    match provider {
        ApiProvider::Anthropic => anthropic_text(stream, text),
        ApiProvider::OpenAi => openai_text(stream, text),
    }
}

/// The tool result the 2nd request sends back, in the provider's own shape.
fn sent_tool_result(provider: ApiProvider, body: &Value) -> (String, String) {
    let messages = body["messages"].as_array().unwrap();
    match provider {
        ApiProvider::Anthropic => {
            let last = messages.last().unwrap();
            assert_eq!(last["role"], "user");
            let block = &last["content"][0];
            assert_eq!(block["type"], "tool_result");
            (
                block["tool_use_id"].as_str().unwrap().to_owned(),
                block["content"].as_str().unwrap().to_owned(),
            )
        }
        ApiProvider::OpenAi => {
            let last = messages.last().unwrap();
            assert_eq!(last["role"], "tool");
            (
                last["tool_call_id"].as_str().unwrap().to_owned(),
                last["content"].as_str().unwrap().to_owned(),
            )
        }
    }
}

async fn two_step_turn(provider: ApiProvider, stream: bool) {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        provider,
        vec![
            tool_use_response(
                provider,
                stream,
                "write_file",
                json!({"path": "out/hello.txt", "content": "hi"}),
            ),
            text_response(provider, stream, "Done here."),
        ],
    )
    .await;
    let env = Env::new();
    let backend = backend(backend_settings(api(provider, &server.uri(), stream)));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    assert_eq!(session.pid(), None);
    let (events, mut rx) = sink();
    let outcome = session
        .run_turn("Write hello", &issue(), &events)
        .await
        .unwrap();
    assert_eq!(outcome.final_message.as_deref(), Some("Done here."));
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("out/hello.txt")).unwrap(),
        "hi"
    );

    let events = drain(&mut rx);
    assert_eq!(
        events.first().unwrap().kind(),
        CodexEventKind::SessionStarted
    );
    assert_eq!(
        events.first().unwrap().session_id(),
        Some(outcome.session_id.as_str())
    );
    assert_eq!(events.last().unwrap().kind(), CodexEventKind::TurnCompleted);
    let item = |kind: &str| {
        events.iter().find(|e| {
            e.method() == Some("item/completed")
                && e.payload().and_then(|p| p.pointer("/params/item/type")) == Some(&json!(kind))
        })
    };
    let change = item("fileChange").expect("fileChange item");
    assert_eq!(
        change.payload().unwrap()["params"]["item"]["changes"][0]["path"],
        "out/hello.txt"
    );
    let message = item("agentMessage").expect("agentMessage item");
    assert_eq!(
        message.payload().unwrap()["params"]["item"]["text"],
        "Done here."
    );
    if stream {
        let deltas: String = events
            .iter()
            .filter(|e| e.method() == Some("item/agentMessage/delta"))
            .map(|e| {
                e.payload().unwrap()["params"]["delta"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(deltas, "Done here.");
    }
    let totals: Vec<_> = events
        .iter()
        .filter_map(|e| e.token_usage)
        .map(|u| (u.input_tokens, u.output_tokens, u.total_tokens))
        .collect();
    let expected_input = match provider {
        ApiProvider::Anthropic => 20 + 30 + 10,
        ApiProvider::OpenAi => 20 + 40,
    };
    assert_eq!(totals.len(), 2, "one cumulative usage event per response");
    assert_eq!(
        totals.last().copied(),
        Some((Some(expected_input), Some(17), Some(expected_input + 17)))
    );

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        match provider {
            ApiProvider::Anthropic => {
                assert_eq!(request.headers["x-api-key"], KEY);
                assert_eq!(request.headers["anthropic-version"], "2023-06-01");
                assert!(request.headers.get("authorization").is_none());
            }
            ApiProvider::OpenAi => {
                assert_eq!(request.headers["authorization"], format!("Bearer {KEY}"));
                assert!(request.headers.get("x-api-key").is_none());
            }
        }
        assert_eq!(request.headers["x-extra"], "yes");
    }
    let first = request_json(&requests[0]);
    assert_eq!(first["model"], "test-model");
    assert_eq!(first["stream"], json!(stream));
    let names: Vec<String> = match provider {
        ApiProvider::Anthropic => first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect(),
        ApiProvider::OpenAi => first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect(),
    };
    assert_eq!(
        names,
        [
            "read_file",
            "write_file",
            "edit_file",
            "list_files",
            "shell"
        ]
    );
    let (id, content) = sent_tool_result(provider, &request_json(&requests[1]));
    let expected_id = match provider {
        ApiProvider::Anthropic => "toolu_1",
        ApiProvider::OpenAi => "call_1",
    };
    assert_eq!(id, expected_id);
    assert_eq!(content, "Wrote 2 bytes to out/hello.txt.");
    session.stop().await;
}

#[tokio::test]
async fn anthropic_streaming_two_step_turn() {
    two_step_turn(ApiProvider::Anthropic, true).await;
}

#[tokio::test]
async fn anthropic_non_streaming_two_step_turn() {
    two_step_turn(ApiProvider::Anthropic, false).await;
}

#[tokio::test]
async fn openai_streaming_two_step_turn() {
    two_step_turn(ApiProvider::OpenAi, true).await;
}

#[tokio::test]
async fn openai_non_streaming_two_step_turn() {
    two_step_turn(ApiProvider::OpenAi, false).await;
}

#[tokio::test]
async fn the_conversation_continues_across_turns() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_text(false, "first answer"),
            openai_text(false, "second answer"),
        ],
    )
    .await;
    let env = Env::new();
    let backend = backend(backend_settings(api(
        ApiProvider::OpenAi,
        &server.uri(),
        false,
    )));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    let one = session.run_turn("one", &issue(), &events).await.unwrap();
    let two = session.run_turn("two", &issue(), &events).await.unwrap();
    assert_eq!(one.thread_id, two.thread_id);
    assert_ne!(one.turn_id, two.turn_id);
    assert_eq!(two.final_message.as_deref(), Some("second answer"));
    let requests = server.received_requests().await.unwrap();
    let messages = request_json(&requests[1])["messages"].clone();
    let roles: Vec<&str> = messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(messages[1]["content"], "one");
    assert_eq!(messages[2]["content"], "first answer");
    assert_eq!(messages[0]["content"], "You are a test agent.");
}

#[tokio::test]
async fn busy_responses_are_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_text(false, "ok"))
        .with_priority(3)
        .mount(&server)
        .await;
    let env = Env::new();
    let backend = backend(backend_settings(api(
        ApiProvider::Anthropic,
        &server.uri(),
        false,
    )));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    let outcome = session.run_turn("go", &issue(), &events).await.unwrap();
    assert_eq!(outcome.final_message.as_deref(), Some("ok"));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn client_errors_fail_the_turn_without_leaking_the_key() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string(format!(
            "{{\"error\":{{\"message\":\"model not found (key {KEY})\"}}}}"
        )))
        .mount(&server)
        .await;
    let env = Env::new();
    let backend = backend(backend_settings(api(
        ApiProvider::OpenAi,
        &server.uri(),
        true,
    )));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, mut rx) = sink();
    let err = session.run_turn("go", &issue(), &events).await.unwrap_err();
    let CodexError::Backend(message) = &err else {
        panic!("expected a backend error, got {err}");
    };
    assert!(message.contains("HTTP 400"), "{message}");
    assert!(message.contains("model not found"), "{message}");
    assert!(!message.contains(KEY), "{message}");
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "4xx is not retried"
    );
    let events = drain(&mut rx);
    let last = events.last().unwrap();
    assert_eq!(last.kind(), CodexEventKind::TurnEndedWithError);
    assert_eq!(last.reason(), Some(&err));
}

#[tokio::test]
async fn max_steps_bounds_a_turn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(openai_tool_call(false, "list_files", json!({})))
        .mount(&server)
        .await;
    let env = Env::new();
    let mut settings = api(ApiProvider::OpenAi, &server.uri(), false);
    settings.max_steps = 2;
    let backend = backend(backend_settings(settings));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    let err = session
        .run_turn("loop", &issue(), &events)
        .await
        .unwrap_err();
    assert!(matches!(err, CodexError::LimitExceeded(_)), "{err}");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_failed_turn_is_rolled_back_so_the_next_one_is_consistent() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_tool_call(false, "list_files", json!({})),
            ResponseTemplate::new(400).set_body_string("bad"),
            openai_text(false, "fresh"),
        ],
    )
    .await;
    let env = Env::new();
    let backend = backend(backend_settings(api(
        ApiProvider::OpenAi,
        &server.uri(),
        false,
    )));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    assert!(session.run_turn("one", &issue(), &events).await.is_err());
    session.run_turn("two", &issue(), &events).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let messages = request_json(&requests[2])["messages"].clone();
    let roles: Vec<&str> = messages
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user"], "the failed exchange is gone");
    assert_eq!(messages[1]["content"], "two");
}

#[derive(Default)]
struct FakeTracker {
    calls: Mutex<Vec<(Option<String>, Value)>>,
}

#[async_trait]
impl DynamicToolHandler for FakeTracker {
    fn tool_specs(&self) -> Vec<Value> {
        vec![
            json!({"name": "linear_graphql", "description": "Run GraphQL.\n",
            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}},
                "required": ["query"]}}),
        ]
    }

    fn secret_environment_names(&self) -> Vec<String> {
        vec!["FAKE_TRACKER_TOKEN".into()]
    }

    async fn execute(&self, tool: Option<&str>, arguments: Value, issue: &Issue) -> Value {
        assert_eq!(issue.identifier.as_deref(), Some("MT-1"));
        self.calls
            .lock()
            .unwrap()
            .push((tool.map(str::to_owned), arguments));
        tool_response(true, "{\"data\":{\"ok\":true}}")
    }
}

#[tokio::test]
async fn tracker_tools_are_offered_and_routed_to_the_handler() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::Anthropic,
        vec![
            anthropic_tool_use(
                true,
                "linear_graphql",
                json!({"query": "{ viewer { id } }"}),
            ),
            anthropic_text(true, "commented"),
        ],
    )
    .await;
    let env = Env::new();
    let tracker = Arc::new(FakeTracker::default());
    let backend = backend(backend_settings(api(
        ApiProvider::Anthropic,
        &server.uri(),
        true,
    )));
    let options = env
        .options(env.settings(None))
        .with_tool_handler(Arc::clone(&tracker) as Arc<dyn DynamicToolHandler>);
    let mut session = backend.start(options).await.unwrap();
    let (events, mut rx) = sink();
    session
        .run_turn("comment", &issue(), &events)
        .await
        .unwrap();
    assert_eq!(
        tracker.calls.lock().unwrap().as_slice(),
        [(
            Some("linear_graphql".to_owned()),
            json!({"query": "{ viewer { id } }"})
        )]
    );
    let requests = server.received_requests().await.unwrap();
    let first = request_json(&requests[0]);
    let tool = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "linear_graphql")
        .expect("tracker tool offered");
    assert_eq!(tool["description"], "Run GraphQL.");
    let (_, content) = sent_tool_result(ApiProvider::Anthropic, &request_json(&requests[1]));
    assert_eq!(content, "{\"data\":{\"ok\":true}}");
    let events = drain(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| e.kind() == CodexEventKind::ToolCallCompleted
                && e.payload().unwrap()["params"]["tool"] == "linear_graphql")
    );
}

#[tokio::test]
async fn tools_cannot_leave_the_workspace() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_tool_call(
                false,
                "write_file",
                json!({"path": "../escape.txt", "content": "x"}),
            ),
            openai_text(false, "gave up"),
        ],
    )
    .await;
    let env = Env::new();
    let backend = backend(backend_settings(api(
        ApiProvider::OpenAi,
        &server.uri(),
        false,
    )));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, mut rx) = sink();
    session.run_turn("escape", &issue(), &events).await.unwrap();
    assert!(!env.root.join("escape.txt").exists());
    let requests = server.received_requests().await.unwrap();
    let (_, content) = sent_tool_result(ApiProvider::OpenAi, &request_json(&requests[1]));
    assert!(content.contains("`..` is not allowed"), "{content}");
    assert!(
        drain(&mut rx)
            .iter()
            .any(|e| e.kind() == CodexEventKind::ToolCallFailed)
    );
}

#[tokio::test]
async fn shell_runs_in_the_workspace_without_secret_variables() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_tool_call(
                true,
                "shell",
                json!({"command": "pwd; echo \"gh=${GITHUB_TOKEN:-unset} extra=$EXTRA_VAR\"; exit 3"}),
            ),
            openai_text(true, "ran it"),
        ],
    )
    .await;
    let env = Env::new();
    let backend = backend(backend_settings(api(
        ApiProvider::OpenAi,
        &server.uri(),
        true,
    )));
    let options = env
        .options(env.settings(Some("github")))
        .with_env("GITHUB_TOKEN", "ghp_should_not_reach_the_shell")
        .with_env("EXTRA_VAR", "visible");
    let mut session = backend.start(options).await.unwrap();
    let (events, mut rx) = sink();
    session.run_turn("run", &issue(), &events).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let (_, content) = sent_tool_result(ApiProvider::OpenAi, &request_json(&requests[1]));
    assert!(content.starts_with("exit code: 3\n"), "{content}");
    assert!(
        content.contains(&env.workspace.to_string_lossy().into_owned()),
        "{content}"
    );
    assert!(content.contains("gh=unset extra=visible"), "{content}");
    let events = drain(&mut rx);
    let started = events
        .iter()
        .find(|e| e.method() == Some("item/started"))
        .expect("command started");
    assert_eq!(
        started.payload().unwrap()["params"]["item"]["type"],
        "commandExecution"
    );
    let completed = events
        .iter()
        .find(|e| {
            e.method() == Some("item/completed")
                && e.payload().unwrap()["params"]["item"]["type"] == "commandExecution"
        })
        .expect("command completed");
    assert_eq!(
        completed.payload().unwrap()["params"]["item"]["exitCode"],
        3
    );
}

fn sandbox_available(mode: SandboxMode) -> bool {
    symphony_agents::api::sandbox::resolve(mode).is_ok()
}

#[tokio::test]
async fn the_seatbelt_sandbox_blocks_writes_outside_the_workspace() {
    if !cfg!(target_os = "macos") || !sandbox_available(SandboxMode::Seatbelt) {
        return;
    }
    let home = std::env::var("HOME").unwrap();
    let probe = Path::new(&home).join(format!(".symphony-seatbelt-probe-{}", std::process::id()));
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_tool_call(
                false,
                "shell",
                json!({"command": format!("echo inside > ok.txt; touch '{}' && echo ESCAPED",
                    probe.display())}),
            ),
            openai_text(false, "done"),
        ],
    )
    .await;
    let env = Env::new();
    let mut settings = api(ApiProvider::OpenAi, &server.uri(), false);
    // Explicit: `auto` never picks seatbelt (development only).
    settings.sandbox = SandboxMode::Seatbelt;
    let backend = backend(backend_settings(settings));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    session.run_turn("probe", &issue(), &events).await.unwrap();
    let escaped = probe.exists();
    if escaped {
        let _ = std::fs::remove_file(&probe);
    }
    assert!(
        !escaped,
        "the sandbox let a write outside the workspace through"
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("ok.txt")).unwrap(),
        "inside\n"
    );
    let requests = server.received_requests().await.unwrap();
    let (_, content) = sent_tool_result(ApiProvider::OpenAi, &request_json(&requests[1]));
    assert!(!content.contains("ESCAPED"), "{content}");
}

#[tokio::test]
async fn auto_without_a_sandbox_refuses_the_shell() {
    if sandbox_available(SandboxMode::Auto) {
        return;
    }
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_tool_call(false, "shell", json!({"command": "echo hi > ran.txt"})),
            openai_text(false, "ok"),
        ],
    )
    .await;
    let env = Env::new();
    let mut settings = api(ApiProvider::OpenAi, &server.uri(), false);
    settings.sandbox = SandboxMode::Auto;
    let backend = backend(backend_settings(settings));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    session.run_turn("try", &issue(), &events).await.unwrap();
    assert!(!env.workspace.join("ran.txt").exists());
    let requests = server.received_requests().await.unwrap();
    let (_, content) = sent_tool_result(ApiProvider::OpenAi, &request_json(&requests[1]));
    assert!(content.contains("sandbox: none"), "{content}");
}

#[tokio::test]
async fn an_unreachable_base_url_is_a_backend_error() {
    let env = Env::new();
    // Port 9 (discard) on loopback: connection refused.
    let backend = backend(backend_settings(api(
        ApiProvider::OpenAi,
        "http://127.0.0.1:9/v1",
        true,
    )));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, mut rx) = sink();
    let err = session.run_turn("go", &issue(), &events).await.unwrap_err();
    let CodexError::Backend(message) = &err else {
        panic!("expected a backend error, got {err}");
    };
    assert!(message.contains("request failed"), "{message}");
    assert_eq!(
        drain(&mut rx).last().unwrap().kind(),
        CodexEventKind::TurnEndedWithError
    );
}

#[tokio::test]
async fn a_turn_that_runs_too_long_times_out() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(anthropic_text(false, "late").set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let env = Env::new();
    let mut settings = backend_settings(api(ApiProvider::Anthropic, &server.uri(), false));
    settings.turn_timeout_ms = 200;
    let backend = backend(settings);
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    let err = session.run_turn("go", &issue(), &events).await.unwrap_err();
    assert_eq!(err, CodexError::TurnTimeout);
}

#[tokio::test]
async fn start_refuses_ssh_workers_missing_keys_and_foreign_workspaces() {
    let env = Env::new();
    let backend = backend(backend_settings(api(
        ApiProvider::OpenAi,
        "http://127.0.0.1:9",
        false,
    )));
    let options = env
        .options(env.settings(None))
        .with_worker_host(Some("worker-1".into()), None);
    let Err(err) = backend.start(options).await else {
        panic!("ssh workers must be refused");
    };
    assert!(matches!(err, CodexError::Unsupported(_)), "{err}");

    let keyless = ApiBackend::new(backend_settings(api(
        ApiProvider::OpenAi,
        "http://x",
        false,
    )))
    .with_env(Arc::new(MapEnv::new()));
    let Err(err) = keyless.start(env.options(env.settings(None))).await else {
        panic!("a missing key must be refused");
    };
    assert_eq!(
        err,
        CodexError::Backend("no API key: the variable TEST_API_KEY is not set".into())
    );

    let outside = tempfile::tempdir().unwrap();
    let options = StartOptions::new(
        outside.path().to_string_lossy().into_owned(),
        env.settings(None),
        env.root.join("WORKFLOW.md"),
    );
    let Err(err) = backend.start(options).await else {
        panic!("a workspace outside the root must be refused");
    };
    assert!(matches!(err, CodexError::InvalidWorkspaceCwd(_)), "{err}");
}

#[tokio::test]
async fn keyless_local_servers_get_no_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("\"model\":\"test-model\""))
        .respond_with(openai_text(false, "local"))
        .mount(&server)
        .await;
    let env = Env::new();
    let mut settings = api(ApiProvider::OpenAi, &format!("{}/v1/", server.uri()), false);
    settings.api_key_env = None;
    let backend = ApiBackend::new(backend_settings(settings));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    let outcome = session.run_turn("hi", &issue(), &events).await.unwrap();
    assert_eq!(outcome.final_message.as_deref(), Some("local"));
    let requests = server.received_requests().await.unwrap();
    assert!(requests[0].headers.get("authorization").is_none());
}

/// `true` when `bwrap` exists and may create namespaces here (often not in CI containers).
/// `true` when bubblewrap runs here. With `SYMPHONY_REQUIRE_SANDBOX` set (CI on Linux) a missing
/// or broken bubblewrap fails the test instead of skipping it.
fn bubblewrap_works() -> bool {
    let works = cfg!(target_os = "linux")
        && sandbox_available(SandboxMode::Bubblewrap)
        && std::process::Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--unshare-all",
                "true",
            ])
            .status()
            .is_ok_and(|s| s.success());
    if !works && std::env::var_os("SYMPHONY_REQUIRE_SANDBOX").is_some_and(|v| !v.is_empty()) {
        panic!("SYMPHONY_REQUIRE_SANDBOX is set but bubblewrap cannot run here");
    }
    works
}

/// Runs one `shell` call of `command` under bubblewrap and returns its output.
async fn bubblewrap_shell(env: &Env, command: &str) -> String {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_tool_call(false, "shell", json!({"command": command})),
            openai_text(false, "done"),
        ],
    )
    .await;
    let mut settings = api(ApiProvider::OpenAi, &server.uri(), false);
    settings.sandbox = SandboxMode::Bubblewrap;
    let mut session = backend(backend_settings(settings))
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    session.run_turn("probe", &issue(), &events).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    sent_tool_result(ApiProvider::OpenAi, &request_json(&requests[1])).1
}

#[tokio::test]
async fn the_bubblewrap_sandbox_hides_the_host_network_and_environment() {
    if !bubblewrap_works() {
        return;
    }
    let env = Env::new();
    // A sibling workspace and the user's home directory hold secrets the agent must not read.
    let sibling = env.workspace.parent().unwrap().join("OTHER-1");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("secret.txt"), "sibling-secret").unwrap();
    let home = std::env::var("HOME").unwrap();
    let output = bubblewrap_shell(
        &env,
        &format!(
            "cat '{}' 2>&1; ls '{home}' >/dev/null 2>&1 && echo HOME-VISIBLE; \
             (exec 3<>/dev/tcp/1.1.1.1/443) 2>/dev/null && echo NET-OPEN; \
             env | grep -c '^CARGO' ; echo pid1=$(cat /proc/1/comm)",
            sibling.join("secret.txt").display()
        ),
    )
    .await;
    assert!(!output.contains("sibling-secret"), "{output}");
    assert!(!output.contains("HOME-VISIBLE"), "{output}");
    assert!(!output.contains("NET-OPEN"), "{output}");
    assert!(
        output.contains("\n0\n"),
        "Symphony's environment is cleared: {output}"
    );
    assert!(!output.contains("pid1=symphony"), "{output}");
}

#[tokio::test]
async fn the_bubblewrap_sandbox_blocks_writes_outside_the_workspace() {
    if !bubblewrap_works() {
        return;
    }
    let outside = tempfile::tempdir().unwrap();
    let probe = outside.path().join("escaped");
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_tool_call(
                false,
                "shell",
                json!({"command": format!("echo inside > ok.txt; touch '{}' && echo ESCAPED",
                    probe.display())}),
            ),
            openai_text(false, "done"),
        ],
    )
    .await;
    let env = Env::new();
    let mut settings = api(ApiProvider::OpenAi, &server.uri(), false);
    settings.sandbox = SandboxMode::Bubblewrap;
    let backend = backend(backend_settings(settings));
    let mut session = backend
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    session.run_turn("probe", &issue(), &events).await.unwrap();
    assert!(
        !probe.exists(),
        "the sandbox let a write outside the workspace through"
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("ok.txt")).unwrap(),
        "inside\n"
    );
}

#[tokio::test]
async fn key_files_win_and_max_retries_bounds_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let env = Env::new();
    let key_file = env.root.join("api-key");
    std::fs::write(&key_file, "sk-from-file-0123456789\n").unwrap();
    let mut settings = api(ApiProvider::Anthropic, &server.uri(), false);
    settings.api_key_file = Some(key_file);
    settings.max_retries = 1;
    let mut session = backend(backend_settings(settings))
        .start(env.options(env.settings(None)))
        .await
        .unwrap();
    let (events, _rx) = sink();
    let err = session.run_turn("go", &issue(), &events).await.unwrap_err();
    assert!(err.to_string().contains("HTTP 503"), "{err}");
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "one try plus max_retries = 1");
    assert_eq!(
        requests[0]
            .headers
            .get("x-api-key")
            .unwrap()
            .to_str()
            .unwrap(),
        "sk-from-file-0123456789",
        "the file wins over api_key_env"
    );

    // An unreadable key file stops the session before any request.
    let mut settings = api(ApiProvider::Anthropic, &server.uri(), false);
    settings.api_key_file = Some(env.root.join("missing-key"));
    let err = match backend(backend_settings(settings))
        .start(env.options(env.settings(None)))
        .await
    {
        Err(err) => err,
        Ok(_) => panic!("a missing key file must fail the start"),
    };
    assert!(err.to_string().contains("missing-key"), "{err}");
}

#[tokio::test]
async fn a_task_egress_proxy_wins_over_network_true_and_task_variables_reach_the_shell() {
    let server = MockServer::start().await;
    mount_sequence(
        &server,
        ApiProvider::OpenAi,
        vec![
            openai_tool_call(
                false,
                "shell",
                json!({"command": "echo \"proxy=${HTTPS_PROXY:-none} gh=${GH_TOKEN:-unset} tracker=${GITHUB_TOKEN:-unset}\""}),
            ),
            openai_text(false, "ran it"),
        ],
    )
    .await;
    let env = Env::new();
    let mut settings = api(ApiProvider::OpenAi, &server.uri(), false);
    settings.network = true;
    let backend = backend(backend_settings(settings));
    let options = env
        .options(env.settings(Some("github")))
        .with_env("GITHUB_TOKEN", "ghp_operator_should_not_reach_the_shell")
        .with_env("GH_TOKEN", "operator-gh-should-not-reach-the-shell")
        .with_task_env("GH_TOKEN", "task-scoped-token")
        .with_egress(Some(symphony_codex::EgressRoute {
            socket: None,
            tcp: "127.0.0.1:41999".parse().unwrap(),
        }));
    let mut session = backend.start(options).await.unwrap();
    let (events, _rx) = sink();
    session.run_turn("run", &issue(), &events).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let (_, content) = sent_tool_result(ApiProvider::OpenAi, &request_json(&requests[1]));
    assert!(
        content.contains("proxy=http://127.0.0.1:41999 gh=task-scoped-token tracker=unset"),
        "{content}"
    );
}
