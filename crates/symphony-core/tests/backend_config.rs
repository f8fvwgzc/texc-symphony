//! `backends:`, `agent.backend`, backend prompt templates and `tracker.kind: none`.

use serde_json::{Value, json};
use symphony_core::config::{
    self, ApiProvider, BackendKind, SandboxMode, Settings, validate_settings,
};
use symphony_core::prompt::render_backend_prompt;
use symphony_core::{ConfigError, Issue, MapEnv};

fn parse(value: Value) -> Result<Settings, ConfigError> {
    let Value::Object(map) = value else {
        panic!("expected an object")
    };
    config::parse(&map, &MapEnv::new())
}

fn invalid(value: Value) -> String {
    match parse(value) {
        Err(ConfigError::InvalidWorkflowConfig(message)) => message,
        other => panic!("expected invalid config, got {other:?}"),
    }
}

#[test]
fn codex_is_built_in_and_the_default() {
    let settings = parse(json!({})).unwrap();
    assert_eq!(settings.agent.backend, "codex");
    let codex = settings.backend(None).unwrap();
    assert_eq!(codex.kind, BackendKind::Codex);
    assert!(Settings::default().backend(Some("codex")).is_some());
    assert!(settings.backend(Some("missing")).is_none());
}

#[test]
fn an_openai_compatible_provider_needs_only_config() {
    let settings = parse(json!({
        "agent": {"backend": "local"},
        "backends": {"local": {
            "kind": "api", "provider": "openai", "base_url": "http://localhost:11434/v1",
            "model": "qwen2.5-coder:32b", "api_key_env": "$OLLAMA_KEY",
            "headers": {"X-Title": "symphony"}, "tools": ["read_file", "shell"],
            "max_steps": 12, "temperature": 0.2, "stream": false, "sandbox": "none", "unconfined": true,
            "network": true, "prompt_template": "Task:\n{{ prompt }}",
            "pricing": {"input_usd_per_million_tokens": 0.5, "output_usd_per_million_tokens": 1},
        }},
    }))
    .unwrap();
    let local = settings.backend(None).unwrap();
    assert_eq!(local.model.as_deref(), Some("qwen2.5-coder:32b"));
    assert_eq!(
        local.prompt_template.as_deref(),
        Some("Task:\n{{ prompt }}")
    );
    assert_eq!(
        local
            .pricing
            .as_ref()
            .unwrap()
            .output_usd_per_million_tokens,
        1.0
    );
    let BackendKind::Api(api) = &local.kind else {
        panic!("api backend")
    };
    assert_eq!(api.provider, ApiProvider::OpenAi);
    assert_eq!(api.base_url, "http://localhost:11434/v1");
    assert_eq!(api.api_key_env.as_deref(), Some("OLLAMA_KEY"));
    assert_eq!(api.headers["X-Title"], "symphony");
    assert_eq!(api.tools, vec!["read_file", "shell"]);
    assert_eq!(api.max_steps, 12);
    assert_eq!(api.temperature, Some(0.2));
    assert!(!api.stream && api.network);
    assert_eq!(api.sandbox, SandboxMode::None);
    // The host-side key never reaches agent children.
    assert!(
        settings
            .secret_environment_names()
            .contains(&"OLLAMA_KEY".to_owned())
    );
    assert!(settings.backend(Some("codex")).is_some());
}

#[test]
fn anthropic_and_claude_code_defaults() {
    let settings = parse(json!({"backends": {
        "sonnet": {"kind": "api", "provider": "anthropic", "model": "claude-sonnet-4-5",
            "api_key_env": "ANTHROPIC_API_KEY"},
        "cc": {"kind": "claude_code", "model": "sonnet", "allowed_tools": ["Read", "Bash(git:*)"]},
    }}))
    .unwrap();
    let BackendKind::Api(api) = settings.backend(Some("sonnet")).unwrap().kind else {
        panic!("api")
    };
    assert_eq!(api.base_url, "https://api.anthropic.com");
    assert_eq!(api.tools.len(), 6);
    assert!(api.stream);
    assert_eq!(api.sandbox, SandboxMode::Auto);
    let BackendKind::ClaudeCode(cc) = settings.backend(Some("cc")).unwrap().kind else {
        panic!("claude")
    };
    assert_eq!(cc.command, "claude");
    assert_eq!(cc.permission_mode, "acceptEdits");
    assert_eq!(cc.allowed_tools, vec!["Read", "Bash(git:*)"]);
    // The api backend reads its key host-side, so agent children never see it.
    assert!(
        settings
            .secret_environment_names()
            .contains(&"ANTHROPIC_API_KEY".to_owned())
    );
}

#[test]
fn bad_backends_are_reported_with_their_path() {
    let message = invalid(json!({
        "agent": {"backend": "nope"},
        "backends": {
            "a": {"kind": "api", "provider": "gemini", "model": "m"},
            "b": {"kind": "api", "provider": "openai"},
            "c": {"kind": "api", "provider": "openai", "model": "m", "base_url": "ftp://x",
                "tools": ["rm_rf"], "max_steps": 0, "sandbox": "jail"},
            "d": {"kind": "teleport"},
            "bad name": {"kind": "codex"},
            "e": {"kind": "claude_code", "permission_mode": "yolo"},
        },
    }));
    for fragment in [
        "backends.a.provider is invalid",
        "backends.b.model can't be blank",
        "backends.c.base_url is invalid",
        "backends.c.tools is invalid",
        "backends.c.max_steps must be greater than 0",
        "backends.c.sandbox is invalid",
        "backends.d.kind is invalid",
        "backends.bad name is invalid",
        "backends.e.permission_mode is invalid",
        "agent.backend must name an entry of backends",
    ] {
        assert!(message.contains(fragment), "{fragment}: {message}");
    }
}

#[test]
fn backend_prompt_templates_wrap_the_prompt() {
    let issue = Issue {
        identifier: Some("TASK-7".into()),
        ..Issue::default()
    };
    let text = render_backend_prompt(
        "[{{ issue.identifier }} attempt {{ attempt }}]\n{{ prompt }}",
        "Summarize the README.",
        &issue,
        Some(2),
    )
    .unwrap();
    assert_eq!(text, "[TASK-7 attempt 2]\nSummarize the README.");
    assert!(render_backend_prompt("{{ nope }}", "x", &issue, None).is_err());
}

#[test]
fn the_none_tracker_needs_no_credentials() {
    let settings = parse(json!({"tracker": {"kind": "none"}})).unwrap();
    validate_settings(&settings, &MapEnv::new()).unwrap();
    assert!(settings.active_states().is_empty());
}

#[test]
fn claude_code_extra_args_cannot_unpin_settings_or_mcp() {
    for flag in [
        "--setting-sources",
        "--settings=x.json",
        "--mcp-config",
        "--dangerously-skip-permissions",
    ] {
        let message = invalid(
            json!({"backends": {"cc": {"kind": "claude_code", "extra_args": [flag, "project"]}}}),
        );
        assert!(
            message.contains("backends.cc.extra_args must not set --setting-sources"),
            "{flag}: {message}"
        );
    }
    parse(json!({"backends": {"cc": {"kind": "claude_code", "extra_args": ["--add-dir", "/x"]}}}))
        .unwrap();
}
