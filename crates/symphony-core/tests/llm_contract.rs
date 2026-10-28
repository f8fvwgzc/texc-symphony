//! The platform LLM connection contract (`LLM_*`): parsing, defaults for the backends and Codex,
//! precedence of `WORKFLOW.md`, and validation.

use serde_json::{Value, json};
use symphony_core::config::llm::{CODEX_LLM_KEY_ENV, LLM_BACKEND};
use symphony_core::config::{self, ApiProvider, BackendKind, LlmProvider, SecretSource, Settings};
use symphony_core::{ConfigError, MapEnv};

fn parse(env: &MapEnv, value: Value) -> Result<Settings, ConfigError> {
    let Value::Object(map) = value else {
        panic!("expected an object")
    };
    config::parse(&map, env)
}

fn api(settings: &Settings, name: &str) -> symphony_core::config::ApiBackendSettings {
    match settings.backend(Some(name)).unwrap().kind {
        BackendKind::Api(api) => api,
        other => panic!("{name} is {other:?}"),
    }
}

#[test]
fn without_the_contract_nothing_changes() {
    let settings = parse(&MapEnv::new(), json!({})).unwrap();
    assert_eq!(settings.llm, None);
    assert_eq!(settings.agent.backend, "codex");
    assert!(settings.backend(Some(LLM_BACKEND)).is_none());
    assert_eq!(settings.codex.provider, None);
}

#[test]
fn an_anthropic_key_file_configures_the_llm_backend() {
    let env = MapEnv::new()
        .with("LLM_PROVIDER", "anthropic")
        .with("LLM_API_KEY_FILE", "/var/run/secrets/llm/api-key")
        .with("LLM_API_KEY", "sk-ant-ignored-when-a-file-is-set")
        .with("LLM_MODEL", "claude-sonnet-4-5")
        .with("LLM_MODEL_SMALL", "claude-haiku-4-5");
    let settings = parse(&env, json!({})).unwrap();
    let llm = settings.llm.clone().unwrap();
    assert_eq!(llm.provider, LlmProvider::Anthropic);
    assert_eq!(
        llm.api_key,
        Some(SecretSource::File("/var/run/secrets/llm/api-key".into()))
    );
    assert_eq!(
        settings.agent.backend, "codex",
        "an existing deployment keeps its backend; `llm` must be chosen"
    );
    let backend = settings.backend(Some(LLM_BACKEND)).unwrap();
    assert_eq!(backend.model.as_deref(), Some("claude-sonnet-4-5"));
    let api = api(&settings, LLM_BACKEND);
    assert_eq!(api.provider, ApiProvider::Anthropic);
    assert_eq!(api.base_url, "https://api.anthropic.com");
    assert_eq!(
        api.api_key_file.as_deref(),
        Some(std::path::Path::new("/var/run/secrets/llm/api-key"))
    );
    assert_eq!(api.api_key_env, None);
    // Codex speaks the Responses API only: an Anthropic contract leaves it alone.
    assert_eq!(settings.codex.provider, None);
    for name in ["LLM_API_KEY", "LLM_API_KEY_FILE", "LLM_GATEWAY_TOKEN_FILE"] {
        assert!(
            settings
                .secret_environment_names()
                .contains(&name.to_owned()),
            "{name}"
        );
    }
}

#[test]
fn an_openai_compatible_server_configures_codex_and_api_backends() {
    let env = MapEnv::new()
        .with("LLM_PROVIDER", "openai-compatible")
        .with("LLM_BASE_URL", "http://vllm.internal:8000/v1/")
        .with("LLM_API_KEY", "sk-vllm-0123456789")
        .with("LLM_MODEL", "qwen3-coder")
        .with("LLM_TIMEOUT_SECS", "45")
        .with("LLM_MAX_RETRIES", "5");
    let settings = parse(
        &env,
        json!({"backends": {
            "inherits": {"kind": "api"},
            "own": {"kind": "api", "provider": "openai", "base_url": "http://ollama:11434/v1",
                "model": "llama3"},
            "anthropic": {"kind": "api", "provider": "anthropic", "model": "claude-x"},
        }}),
    )
    .unwrap();
    let llm = api(&settings, LLM_BACKEND);
    assert_eq!(llm.provider, ApiProvider::OpenAi);
    assert_eq!(llm.base_url, "http://vllm.internal:8000/v1");
    assert_eq!(llm.api_key_env.as_deref(), Some("LLM_API_KEY"));
    assert_eq!((llm.request_timeout_ms, llm.max_retries), (45_000, 5));

    let inherits = api(&settings, "inherits");
    assert_eq!(inherits.base_url, "http://vllm.internal:8000/v1");
    assert_eq!(inherits.api_key_env.as_deref(), Some("LLM_API_KEY"));
    assert_eq!(
        settings.backend(Some("inherits")).unwrap().model.as_deref(),
        Some("qwen3-coder")
    );
    // Its own endpoint: no contract key, its own model.
    let own = api(&settings, "own");
    assert_eq!((own.api_key_env, own.api_key_file), (None, None));
    assert_eq!(own.max_retries, 5, "LLM_MAX_RETRIES is still the default");
    // Another wire format than the contract's: provider defaults, no contract key.
    let anthropic = api(&settings, "anthropic");
    assert_eq!(anthropic.base_url, "https://api.anthropic.com");
    assert_eq!(anthropic.api_key_env, None);

    let codex = settings.codex.provider.clone().unwrap();
    assert_eq!(codex.base_url, "http://vllm.internal:8000/v1");
    assert_eq!(codex.api_key_env.as_deref(), Some(CODEX_LLM_KEY_ENV));
    assert_eq!(codex.request_max_retries, Some(5));
    assert_eq!(codex.stream_idle_timeout_ms, Some(45_000));
    assert_eq!(settings.codex.model.as_deref(), Some("qwen3-coder"));
}

#[test]
fn gateway_mode_sends_everything_to_the_gateway_with_its_token() {
    let env = MapEnv::new()
        .with(
            "LLM_GATEWAY_URL",
            "http://agentsc-gateway.platform:8090/v1/",
        )
        .with(
            "LLM_GATEWAY_TOKEN_FILE",
            "/var/run/secrets/llm/gateway-token",
        )
        .with("LLM_API_KEY", "sk-provider-key-not-for-symphony")
        .with("LLM_MODEL", "claude-sonnet-4-5");
    let settings = parse(&env, json!({})).unwrap();
    let llm = settings.llm.clone().unwrap();
    assert_eq!(
        llm.gateway.as_ref().unwrap().url,
        "http://agentsc-gateway.platform:8090"
    );
    assert_eq!(
        llm.credential(),
        Some(&SecretSource::File(
            "/var/run/secrets/llm/gateway-token".into()
        ))
    );
    let api = api(&settings, LLM_BACKEND);
    assert_eq!(
        api.provider,
        ApiProvider::Anthropic,
        "the default wire in gateway mode"
    );
    assert_eq!(api.base_url, "http://agentsc-gateway.platform:8090");
    assert_eq!(
        api.api_key_file.as_deref(),
        Some(std::path::Path::new("/var/run/secrets/llm/gateway-token"))
    );
    assert_eq!(api.api_key_env, None, "no provider key in gateway mode");
    let codex = settings.codex.provider.clone().unwrap();
    assert_eq!(codex.base_url, "http://agentsc-gateway.platform:8090/v1");

    // OpenAI wire through the gateway.
    let env = env.with("LLM_PROVIDER", "openai");
    let settings = parse(&env, json!({})).unwrap();
    assert_eq!(
        api_url(&settings),
        "http://agentsc-gateway.platform:8090/v1"
    );
}

fn api_url(settings: &Settings) -> String {
    api(settings, LLM_BACKEND).base_url
}

#[test]
fn workflow_settings_win_over_the_contract() {
    let env = MapEnv::new()
        .with("LLM_PROVIDER", "openai")
        .with("LLM_API_KEY", "sk-0123456789")
        .with("LLM_MODEL", "gpt-5");
    let settings = parse(
        &env,
        json!({
            "agent": {"backend": "codex"},
            "codex": {"model": "gpt-5-codex",
                "provider": {"name": "ollama", "base_url": "http://127.0.0.1:11434/v1"}},
            "backends": {"llm": {"kind": "api", "provider": "anthropic", "model": "claude-x",
                "api_key_env": "MY_KEY"}},
        }),
    )
    .unwrap();
    assert_eq!(settings.agent.backend, "codex");
    assert_eq!(settings.codex.provider.as_ref().unwrap().name, "ollama");
    assert_eq!(settings.codex.model.as_deref(), Some("gpt-5-codex"));
    let llm = api(&settings, LLM_BACKEND);
    assert_eq!(llm.provider, ApiProvider::Anthropic);
    assert_eq!(llm.api_key_env.as_deref(), Some("MY_KEY"));
}

#[test]
fn claude_code_entries_take_the_contract_model_when_it_serves_anthropic() {
    let env = MapEnv::new()
        .with("LLM_PROVIDER", "anthropic")
        .with("LLM_API_KEY", "sk-ant-0123456789")
        .with("LLM_MODEL", "claude-sonnet-4-5");
    let settings = parse(&env, json!({"backends": {"cc": {"kind": "claude_code"}}})).unwrap();
    assert_eq!(
        settings.backend(Some("cc")).unwrap().model.as_deref(),
        Some("claude-sonnet-4-5")
    );
    let env = env.with("LLM_PROVIDER", "openai");
    let settings = parse(&env, json!({"backends": {"cc": {"kind": "claude_code"}}})).unwrap();
    assert_eq!(settings.backend(Some("cc")).unwrap().model, None);
}

#[test]
fn bad_contract_values_are_reported_by_variable() {
    for (vars, fragment) in [
        (
            vec![("LLM_PROVIDER", "gemini"), ("LLM_MODEL", "m")],
            "LLM_PROVIDER is invalid",
        ),
        (
            vec![("LLM_PROVIDER", "openai-compatible"), ("LLM_MODEL", "m")],
            "LLM_BASE_URL can't be blank",
        ),
        (vec![("LLM_PROVIDER", "openai")], "LLM_MODEL can't be blank"),
        (
            vec![
                ("LLM_PROVIDER", "openai"),
                ("LLM_MODEL", "m"),
                ("LLM_BASE_URL", "ftp://x"),
            ],
            "LLM_BASE_URL is invalid",
        ),
        (
            vec![
                ("LLM_PROVIDER", "openai"),
                ("LLM_MODEL", "m"),
                ("LLM_TIMEOUT_SECS", "0"),
            ],
            "LLM_TIMEOUT_SECS must be a positive integer",
        ),
        (
            vec![
                ("LLM_PROVIDER", "openai"),
                ("LLM_MODEL", "m"),
                ("LLM_MAX_RETRIES", "11"),
            ],
            "LLM_MAX_RETRIES must be an integer between 0 and 10",
        ),
        (
            vec![("LLM_GATEWAY_URL", "not a url"), ("LLM_MODEL", "m")],
            "LLM_GATEWAY_URL is invalid",
        ),
    ] {
        let env = vars
            .iter()
            .fold(MapEnv::new(), |env, (name, value)| env.with(*name, *value));
        match parse(&env, json!({})) {
            Err(ConfigError::InvalidWorkflowConfig(message)) => {
                assert!(message.contains(fragment), "{fragment}: {message}");
            }
            other => panic!("{fragment}: expected an error, got {other:?}"),
        }
    }
}

#[test]
fn secrets_are_read_when_used_and_trimmed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("key");
    std::fs::write(&file, "  sk-file-0123456789\n").unwrap();
    let env = MapEnv::new().with("K", "sk-env");
    assert_eq!(
        SecretSource::File(file.clone()).read(&env).unwrap(),
        "sk-file-0123456789"
    );
    std::fs::write(&file, "\n").unwrap();
    assert!(
        SecretSource::File(file)
            .read(&env)
            .unwrap_err()
            .contains("is empty")
    );
    assert_eq!(SecretSource::Env("K".into()).read(&env).unwrap(), "sk-env");
    let missing = SecretSource::Env("NOPE".into()).read(&env).unwrap_err();
    assert_eq!(missing, "the variable NOPE is not set");
}

#[test]
fn stray_variables_do_not_break_a_workflow_and_are_reported() {
    use symphony_core::config::llm::{describe, stray_variables};
    let env = MapEnv::new()
        .with("LLM_MODEL", "m")
        .with("LLM_API_KEY", "k");
    let settings = parse(&env, json!({})).unwrap();
    assert_eq!(settings.llm, None);
    assert_eq!(stray_variables(&env), vec!["LLM_API_KEY", "LLM_MODEL"]);
    assert_eq!(describe(&settings), None);
    let env = env.with("LLM_PROVIDER", "openai");
    assert!(stray_variables(&env).is_empty());
    let settings = parse(&env, json!({})).unwrap();
    let line = describe(&settings).unwrap();
    assert!(line.contains("agent.backend is `codex`"), "{line}");
    assert!(line.contains("codex uses it"), "{line}");
}
