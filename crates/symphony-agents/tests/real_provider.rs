//! One tiny real turn per provider. Ignored by default; `.github/workflows/real-provider.yml`
//! runs them with `--ignored` when the repository has the secrets. A test whose key is absent
//! passes without calling anything.
//!
//! * Anthropic: `ANTHROPIC_API_KEY`, model `REAL_ANTHROPIC_MODEL` (default `claude-haiku-4-5`).
//! * OpenAI-compatible: `OPENAI_API_KEY`, `REAL_OPENAI_BASE_URL` (default
//!   `https://api.openai.com/v1`), model `REAL_OPENAI_MODEL` (default `gpt-4o-mini`).

use std::sync::Arc;

use symphony_agents::AgentBackend;
use symphony_agents::api::ApiBackend;
use symphony_codex::{CodexEventKind, EventSink, StartOptions};
use symphony_core::config::backend::ANTHROPIC_BASE_URL;
use symphony_core::config::{
    ApiBackendSettings, ApiProvider, BackendKind, BackendSettings, SandboxMode,
};
use symphony_core::{EnvSource, Issue, ProcessEnv, Settings};

fn present(name: &str) -> bool {
    ProcessEnv.var(name).is_some_and(|v| !v.trim().is_empty())
}

async fn one_turn(provider: ApiProvider, base_url: String, key_env: &str, model: String) {
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.keep()).unwrap();
    let workspace = root.join("REAL-1");
    std::fs::create_dir(&workspace).unwrap();
    let mut settings = Settings::default();
    settings.workspace.root = root.to_string_lossy().into_owned();
    let backend = ApiBackend::new(BackendSettings {
        name: "real".into(),
        kind: BackendKind::Api(ApiBackendSettings {
            provider,
            base_url,
            api_key_env: Some(key_env.into()),
            api_key_file: None,
            max_retries: 3,
            headers: Default::default(),
            tools: vec!["write_file".into(), "read_file".into()],
            max_steps: 4,
            max_output_tokens: 256,
            temperature: Some(0.0),
            stream: true,
            prompt_cache: true,
            compact_after_tokens: 100_000,
            request_timeout_ms: 120_000,
            sandbox: SandboxMode::None,
            network: false,
            shell_timeout_ms: 10_000,
        }),
        model: Some(model),
        prompt_template: None,
        system_prompt: None,
        pricing: None,
        turn_timeout_ms: 180_000,
    });
    let options = StartOptions::new(
        workspace.to_string_lossy().into_owned(),
        Arc::new(settings),
        root.join("WORKFLOW.md"),
    );
    let mut session = backend.start(options).await.expect("session starts");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = session
        .run_turn(
            "Use the write_file tool to create answer.txt containing exactly the word pong, \
             then reply with the single word done.",
            &Issue::default(),
            &EventSink::new(tx),
        )
        .await
        .expect("the turn completes");
    session.stop().await;
    let mut usage = None;
    while let Ok(event) = rx.try_recv() {
        if event.kind() == CodexEventKind::Notification && event.token_usage.is_some() {
            usage = event.token_usage;
        }
    }
    let written = std::fs::read_to_string(workspace.join("answer.txt")).unwrap_or_default();
    println!(
        "final={:?} answer.txt={written:?} usage={usage:?}",
        outcome.final_message
    );
    assert!(written.to_lowercase().contains("pong"), "{written:?}");
    assert!(usage.and_then(|u| u.total_tokens).unwrap_or(0) > 0);
}

#[tokio::test]
#[ignore = "calls the real Anthropic API; run with --ignored and ANTHROPIC_API_KEY"]
async fn anthropic_messages_turn() {
    if !present("ANTHROPIC_API_KEY") {
        eprintln!("ANTHROPIC_API_KEY is not set; skipping");
        return;
    }
    let model = ProcessEnv
        .var("REAL_ANTHROPIC_MODEL")
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "claude-haiku-4-5".into());
    one_turn(
        ApiProvider::Anthropic,
        ANTHROPIC_BASE_URL.into(),
        "ANTHROPIC_API_KEY",
        model,
    )
    .await;
}

#[tokio::test]
#[ignore = "calls a real OpenAI-compatible API; run with --ignored and OPENAI_API_KEY"]
async fn openai_compatible_chat_completions_turn() {
    if !present("OPENAI_API_KEY") {
        eprintln!("OPENAI_API_KEY is not set; skipping");
        return;
    }
    let base_url = ProcessEnv
        .var("REAL_OPENAI_BASE_URL")
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    let model = ProcessEnv
        .var("REAL_OPENAI_MODEL")
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "gpt-4o-mini".into());
    one_turn(ApiProvider::OpenAi, base_url, "OPENAI_API_KEY", model).await;
}
