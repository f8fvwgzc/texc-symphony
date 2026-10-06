//! Config, schema and workflow tests ported from `core_test.exs` and `workspace_and_config_test.exs`.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};
use support::{Harness, TestEnv, assert_invalid_config, default_workspace_root, write_workflow};
use symphony_core::config::{
    self, CodexProvider, CodexSettings, Settings, StringOrMap, WorkspaceSettings,
    default_turn_sandbox_policy, normalize_state_limits, validate_state_limits,
};
use symphony_core::error::TrackerConfigError;
use symphony_core::path_safety::{self, PathError, expand_path};
use symphony_core::workflow;
use symphony_core::{ConfigError, MapEnv};

fn obj(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("expected object, got {other:?}"),
    }
}

fn parse(value: Value, env: &MapEnv) -> Result<Settings, ConfigError> {
    config::parse(&obj(value), env)
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[test]
fn config_defaults_and_validation_checks() {
    let h = Harness::new();
    h.write(&[
        ("tracker_kind", json!("memory")),
        ("tracker_api_token", Value::Null),
        ("tracker_project_slug", Value::Null),
        ("poll_interval_ms", Value::Null),
        ("tracker_active_states", Value::Null),
        ("tracker_terminal_states", Value::Null),
        ("codex_command", Value::Null),
    ]);

    let config = h.store.settings();
    assert_eq!(config.polling.interval_ms, 30_000);
    assert_eq!(
        config.tracker.active_states,
        Some(strings(&["Todo", "In Progress"]))
    );
    assert_eq!(
        config.tracker.terminal_states,
        Some(strings(&[
            "Closed",
            "Cancelled",
            "Canceled",
            "Duplicate",
            "Done"
        ]))
    );
    assert_eq!(config.tracker.assignee, None);
    assert_eq!(config.agent.max_turns, 20);

    assert_invalid_config(
        h.write_and_validate(&[("poll_interval_ms", json!("invalid"))]),
        "polling.interval_ms",
    );

    h.write(&[("poll_interval_ms", json!(45_000))]);
    assert_eq!(h.store.settings().polling.interval_ms, 45_000);

    assert_invalid_config(
        h.write_and_validate(&[("max_turns", json!(0))]),
        "agent.max_turns",
    );

    h.write(&[("max_turns", json!(5))]);
    assert_eq!(h.store.settings().agent.max_turns, 5);

    assert_invalid_config(
        h.write_and_validate(&[("tracker_active_states", json!("Todo,  Review,"))]),
        "tracker.active_states",
    );

    assert_eq!(
        h.write_and_validate(&[
            ("tracker_api_token", json!("token")),
            ("tracker_project_slug", Value::Null)
        ]),
        Err(TrackerConfigError::MissingLinearProjectSlug.into())
    );
    assert_eq!(
        h.write_and_validate(&[
            ("tracker_api_token", json!("   ")),
            ("tracker_project_slug", json!("project"))
        ]),
        Err(TrackerConfigError::MissingLinearApiToken.into())
    );
    assert_eq!(
        h.write_and_validate(&[
            ("tracker_api_token", json!("token")),
            ("tracker_project_slug", json!(""))
        ]),
        Err(TrackerConfigError::MissingLinearProjectSlug.into())
    );

    for blank in ["", "   "] {
        let result = h.write_and_validate(&[
            ("tracker_project_slug", json!("project")),
            ("codex_command", json!(blank)),
        ]);
        assert_invalid_config(result.clone(), "codex.command");
        assert_invalid_config(result, "can't be blank");
    }

    assert_eq!(
        h.write_and_validate(&[("codex_command", json!("/bin/sh app-server"))]),
        Ok(())
    );
    assert_eq!(
        h.write_and_validate(&[("codex_approval_policy", json!("definitely-not-valid"))]),
        Ok(())
    );
    assert_eq!(
        h.write_and_validate(&[("codex_thread_sandbox", json!("unsafe-ish"))]),
        Ok(())
    );
    assert_eq!(
        h.write_and_validate(&[(
            "codex_turn_sandbox_policy",
            json!({"type": "workspaceWrite", "writableRoots": ["relative/path"]})
        )]),
        Ok(())
    );

    assert_invalid_config(
        h.write_and_validate(&[("codex_approval_policy", json!(123))]),
        "codex.approval_policy",
    );
    assert_invalid_config(
        h.write_and_validate(&[("codex_thread_sandbox", json!(123))]),
        "codex.thread_sandbox",
    );

    assert_eq!(
        h.write_and_validate(&[("tracker_kind", json!("123"))]),
        Err(ConfigError::UnsupportedTrackerKind("123".into()))
    );
}

#[test]
fn linear_api_token_resolves_from_linear_api_key_env_var() {
    let h = Harness::new();
    h.env.set("LINEAR_API_KEY", "test-linear-api-key");
    h.write(&[
        ("tracker_api_token", Value::Null),
        ("tracker_project_slug", json!("project")),
        ("codex_command", json!("/bin/sh app-server")),
    ]);
    let settings = h.store.settings();
    assert_eq!(
        settings.tracker.api_key.as_deref(),
        Some("test-linear-api-key")
    );
    assert_eq!(settings.tracker.project_slug.as_deref(), Some("project"));
    assert_eq!(h.store.force_reload(), Ok(()));
}

#[test]
fn linear_assignee_resolves_from_linear_assignee_env_var() {
    let h = Harness::new();
    h.env.set("LINEAR_ASSIGNEE", "dev@example.com");
    h.write(&[
        ("tracker_assignee", Value::Null),
        ("tracker_project_slug", json!("project")),
        ("codex_command", json!("/bin/sh app-server")),
    ]);
    assert_eq!(
        h.store.settings().tracker.assignee.as_deref(),
        Some("dev@example.com")
    );
}

#[test]
fn linear_assignee_from_env_must_not_be_blank() {
    let env = MapEnv::new().with("LINEAR_ASSIGNEE", "   ");
    let settings = parse(
        json!({"tracker": {"kind": "linear", "api_key": "t", "project_slug": "p"}}),
        &env,
    )
    .unwrap();
    // No trimming on the Linear path: the blank value survives parsing and fails validation.
    assert_eq!(settings.tracker.assignee.as_deref(), Some("   "));
    assert_eq!(
        config::validate_settings(&settings, &env),
        Err(TrackerConfigError::InvalidLinearAssignee.into())
    );
}

#[test]
fn workflow_file_path_defaults_to_workflow_md_in_the_current_working_directory_when_app_env_is_unset()
 {
    let h = Harness::new();
    h.store.clear_workflow_file_path();
    assert_eq!(
        h.store.workflow_file_path(),
        h.dir.path().join("WORKFLOW.md")
    );
}

#[test]
fn workflow_file_path_resolves_from_app_env_when_set() {
    let h = Harness::new();
    h.store.set_workflow_file_path("/tmp/app/WORKFLOW.md");
    assert_eq!(
        h.store.workflow_file_path(),
        Path::new("/tmp/app/WORKFLOW.md")
    );
}

#[test]
fn workflow_load_accepts_prompt_only_files_without_front_matter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("PROMPT_ONLY_WORKFLOW.md");
    fs::write(&path, "Prompt only\n").unwrap();
    let loaded = workflow::load(&path).unwrap();
    assert!(loaded.config.is_empty());
    assert_eq!(loaded.prompt, "Prompt only");
    assert_eq!(loaded.prompt_template, "Prompt only");
}

#[test]
fn workflow_load_accepts_unterminated_front_matter_with_an_empty_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("UNTERMINATED_WORKFLOW.md");
    fs::write(&path, "---\ntracker:\n  kind: linear\n").unwrap();
    let loaded = workflow::load(&path).unwrap();
    assert_eq!(
        Value::Object(loaded.config),
        json!({"tracker": {"kind": "linear"}})
    );
    assert_eq!(loaded.prompt, "");
    assert_eq!(loaded.prompt_template, "");
}

#[test]
fn workflow_load_rejects_non_map_front_matter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("INVALID_FRONT_MATTER_WORKFLOW.md");
    fs::write(&path, "---\n- not-a-map\n---\nPrompt body\n").unwrap();
    assert_eq!(
        workflow::load(&path),
        Err(ConfigError::WorkflowFrontMatterNotAMap)
    );
    assert_eq!(
        workflow::parse("---\nscalar\n---\nx"),
        Err(ConfigError::WorkflowFrontMatterNotAMap)
    );
}

#[test]
fn workflow_load_reports_missing_files_and_parse_errors() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("MISSING.md");
    match workflow::load(&missing) {
        Err(ConfigError::MissingWorkflowFile { path, reason }) => {
            assert_eq!(path, missing);
            assert_eq!(reason.name(), "enoent");
        }
        other => panic!("unexpected {other:?}"),
    }
    match workflow::load(dir.path()) {
        Err(ConfigError::MissingWorkflowFile { reason, .. }) => assert_eq!(reason.name(), "eisdir"),
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        workflow::parse("---\ntracker: [\n---\nBroken prompt\n"),
        Err(ConfigError::WorkflowParseError(_))
    ));
}

#[test]
fn workflow_parse_edge_cases() {
    // Comment-only front matter behaves like an empty map (yamerl parity).
    let loaded = workflow::parse("---\n# only a comment\n---\nBody\n").unwrap();
    assert!(loaded.config.is_empty());
    assert_eq!(loaded.prompt, "Body");
    // CRLF is normalized to LF in the prompt; the prompt is trimmed.
    let loaded =
        workflow::parse("---\r\ntracker:\r\n  kind: memory\r\n---\r\n\r\nLine 1\r\nLine 2\r\n\r\n")
            .unwrap();
    assert_eq!(loaded.config["tracker"]["kind"], "memory");
    assert_eq!(loaded.prompt, "Line 1\nLine 2");
    // The opener must be exactly `---` on the first line.
    let loaded = workflow::parse("--- \ntracker: x\n---\n").unwrap();
    assert!(loaded.config.is_empty());
    assert_eq!(loaded.prompt, "--- \ntracker: x\n---");
    // Empty front matter.
    let loaded = workflow::parse("---\n---\nHi").unwrap();
    assert!(loaded.config.is_empty());
    assert_eq!(loaded.prompt, "Hi");
}

#[test]
fn current_workflow_md_file_is_valid_and_complete() {
    let loaded = workflow::load(&fixture("WORKFLOW.md")).unwrap();
    let tracker = loaded.config["tracker"].as_object().unwrap();
    assert_eq!(tracker["kind"], "linear");
    assert!(tracker["provider"]["project_slug"].is_string());
    assert!(tracker["active_states"].is_array());
    assert!(tracker["terminal_states"].is_array());
    let hooks = loaded.config["hooks"].as_object().unwrap();
    let after_create = hooks["after_create"].as_str().unwrap();
    assert!(after_create.contains("git clone --depth 1 https://github.com/openai/symphony ."));
    assert!(after_create.contains("cd elixir && mise trust"));
    assert!(after_create.contains("mise exec -- mix deps.get"));
    assert!(
        hooks["before_remove"]
            .as_str()
            .unwrap()
            .contains("cd elixir && mise exec -- mix workspace.before_remove")
    );
    assert!(!loaded.prompt.trim().is_empty());
    assert_eq!(
        symphony_core::prompt::workflow_prompt(&loaded),
        loaded.prompt
    );

    let env = MapEnv::new().with("LINEAR_API_KEY", "test-linear-api-key");
    let snapshot =
        symphony_core::workflow_store::load_snapshot(&fixture("WORKFLOW.md"), &env).unwrap();
    let settings = &snapshot.settings;
    assert_eq!(settings.workspace.root, "~/code/symphony-workspaces");
    assert_eq!(
        settings.codex.approval_policy,
        StringOrMap::String("never".into())
    );
    assert_eq!(settings.polling.interval_ms, 5_000);
    assert_eq!(
        settings.codex.turn_sandbox_policy,
        Some(obj(
            json!({"type": "workspaceWrite", "networkAccess": true})
        ))
    );
}

#[test]
fn startup_fixture_boots_with_the_memory_tracker() {
    let snapshot = symphony_core::workflow_store::load_snapshot(
        &fixture("startup_workflow.md"),
        &MapEnv::new(),
    )
    .unwrap();
    assert_eq!(snapshot.settings.tracker.kind.as_deref(), Some("memory"));
    assert_eq!(snapshot.settings.codex.command, "codex app-server");
    assert_eq!(snapshot.workflow.prompt, "Test workflow.");
}

#[test]
fn config_reads_defaults_for_optional_settings() {
    let h = Harness::new();
    h.write(&[
        ("tracker_kind", json!("memory")),
        ("workspace_root", Value::Null),
        ("max_concurrent_agents", Value::Null),
        ("codex_approval_policy", Value::Null),
        ("codex_thread_sandbox", Value::Null),
        ("codex_turn_sandbox_policy", Value::Null),
        ("codex_turn_timeout_ms", Value::Null),
        ("codex_read_timeout_ms", Value::Null),
        ("codex_stall_timeout_ms", Value::Null),
        ("tracker_api_token", Value::Null),
        ("tracker_project_slug", Value::Null),
    ]);

    let config = h.store.settings();
    assert_eq!(
        config.tracker.endpoint.as_deref(),
        Some("https://api.linear.app/graphql")
    );
    assert_eq!(config.tracker.api_key, None);
    assert_eq!(config.tracker.project_slug, None);
    assert!(config.tracker.required_labels.is_empty());
    assert_eq!(config.workspace.root, default_workspace_root());
    assert_eq!(config.worker.max_concurrent_agents_per_host, None);
    assert_eq!(config.agent.max_concurrent_agents, 10);
    assert_eq!(config.codex.command, "codex app-server");
    assert_eq!(
        config.codex.approval_policy.to_value(),
        json!({"granular": {"sandbox_approval": false, "rules": false, "mcp_elicitations": false}})
    );
    assert_eq!(config.codex.thread_sandbox, "workspace-write");

    let canonical_default_root = path_safety::canonicalize(default_workspace_root()).unwrap();
    assert_eq!(
        config
            .resolve_runtime_turn_sandbox_policy(None, false, None)
            .unwrap(),
        default_turn_sandbox_policy(&canonical_default_root.to_string_lossy())
    );
    assert_eq!(
        Value::Object(default_turn_sandbox_policy("/root")),
        json!({
            "type": "workspaceWrite",
            "writableRoots": ["/root"],
            "readOnlyAccess": {"type": "fullAccess"},
            "networkAccess": false,
            "excludeTmpdirEnvVar": false,
            "excludeSlashTmp": false
        })
    );
    assert_eq!(config.codex.turn_timeout_ms, 3_600_000);
    assert_eq!(config.codex.read_timeout_ms, 5_000);
    assert_eq!(config.codex.stall_timeout_ms, 300_000);

    h.write(&[(
        "tracker_required_labels",
        json!([" Symphony ", "SYMPHONY", "JavaScript"]),
    )]);
    assert_eq!(
        h.store.settings().tracker.required_labels,
        strings(&["symphony", "javascript"])
    );

    h.write(&[("tracker_required_labels", json!([" "]))]);
    assert_eq!(h.store.settings().tracker.required_labels, strings(&[""]));

    h.write(&[(
        "codex_command",
        json!("codex --config 'model=\"gpt-5.5\"' app-server"),
    )]);
    assert_eq!(
        h.store.settings().codex.command,
        "codex --config 'model=\"gpt-5.5\"' app-server"
    );

    let explicit_root = tempfile::tempdir().unwrap();
    let explicit_workspace = explicit_root.path().join("MT-EXPLICIT");
    let explicit_cache = explicit_workspace.join("cache");
    fs::create_dir_all(&explicit_cache).unwrap();
    let ws = explicit_workspace.to_string_lossy().into_owned();
    let cache = explicit_cache.to_string_lossy().into_owned();
    h.write(&[
        (
            "workspace_root",
            json!(explicit_root.path().to_string_lossy()),
        ),
        ("codex_approval_policy", json!("on-request")),
        ("codex_thread_sandbox", json!("workspace-write")),
        (
            "codex_turn_sandbox_policy",
            json!({"type": "workspaceWrite", "writableRoots": [ws, cache]}),
        ),
    ]);
    let config = h.store.settings();
    assert_eq!(
        config.codex.approval_policy,
        StringOrMap::String("on-request".into())
    );
    assert_eq!(config.codex.thread_sandbox, "workspace-write");
    assert_eq!(
        Value::Object(
            config
                .resolve_runtime_turn_sandbox_policy(Some(&ws), false, None)
                .unwrap()
        ),
        json!({"type": "workspaceWrite", "writableRoots": [ws, cache]})
    );

    assert_invalid_config(
        h.write_and_validate(&[("tracker_active_states", json!(","))]),
        "tracker.active_states",
    );
    assert_invalid_config(
        h.write_and_validate(&[("max_concurrent_agents", json!("bad"))]),
        "agent.max_concurrent_agents",
    );
    assert_invalid_config(
        h.write_and_validate(&[("worker_max_concurrent_agents_per_host", json!(0))]),
        "worker.max_concurrent_agents_per_host",
    );
    for key in ["turn_timeout_ms", "read_timeout_ms", "stall_timeout_ms"] {
        let override_key: &'static str = match key {
            "turn_timeout_ms" => "codex_turn_timeout_ms",
            "read_timeout_ms" => "codex_read_timeout_ms",
            _ => "codex_stall_timeout_ms",
        };
        assert_invalid_config(
            h.write_and_validate(&[(override_key, json!("bad"))]),
            &format!("codex.{key}"),
        );
    }

    let result = h.write_and_validate(&[
        ("tracker_active_states", json!({"todo": true})),
        ("tracker_terminal_states", json!({"done": true})),
        ("poll_interval_ms", json!({"bad": true})),
        ("workspace_root", json!(123)),
        ("max_retry_backoff_ms", json!(0)),
        (
            "max_concurrent_agents_by_state",
            json!({"Todo": "1", "Review": 0, "Done": "bad"}),
        ),
        ("hook_timeout_ms", json!(0)),
        ("observability_enabled", json!("maybe")),
        ("observability_refresh_ms", json!({"bad": true})),
        ("observability_render_interval_ms", json!({"bad": true})),
        ("server_port", json!(-1)),
        ("server_host", json!(123)),
    ]);
    for fragment in [
        "tracker.active_states is invalid",
        "tracker.terminal_states is invalid",
        "polling.interval_ms is invalid",
        "workspace.root is invalid",
        "agent.max_retry_backoff_ms must be greater than 0",
        "agent.max_concurrent_agents_by_state limits must be positive integers",
        "hooks.timeout_ms must be greater than 0",
        "observability.dashboard_enabled is invalid",
        "observability.refresh_ms is invalid",
        "observability.render_interval_ms is invalid",
        "server.port must be greater than or equal to 0",
        "server.host is invalid",
    ] {
        assert_invalid_config(result.clone(), fragment);
    }

    assert_eq!(
        h.write_and_validate(&[("codex_approval_policy", json!(""))]),
        Ok(())
    );
    assert_eq!(
        h.store.settings().codex.approval_policy,
        StringOrMap::String(String::new())
    );

    assert_eq!(
        h.write_and_validate(&[("codex_thread_sandbox", json!(""))]),
        Ok(())
    );
    assert_eq!(h.store.settings().codex.thread_sandbox, "");

    assert_invalid_config(
        h.write_and_validate(&[("codex_turn_sandbox_policy", json!("bad"))]),
        "codex.turn_sandbox_policy",
    );

    h.write(&[
        ("codex_approval_policy", json!("future-policy")),
        ("codex_thread_sandbox", json!("future-sandbox")),
        (
            "codex_turn_sandbox_policy",
            json!({"type": "futureSandbox", "nested": {"flag": true}}),
        ),
    ]);
    let config = h.store.settings();
    assert_eq!(
        config.codex.approval_policy,
        StringOrMap::String("future-policy".into())
    );
    assert_eq!(config.codex.thread_sandbox, "future-sandbox");
    assert_eq!(h.store.force_reload(), Ok(()));
    assert_eq!(
        Value::Object(
            config
                .resolve_runtime_turn_sandbox_policy(None, false, None)
                .unwrap()
        ),
        json!({"type": "futureSandbox", "nested": {"flag": true}})
    );

    h.write(&[("codex_command", json!("codex app-server"))]);
    assert_eq!(h.store.settings().codex.command, "codex app-server");
}

#[test]
fn config_resolves_var_references_for_env_backed_secret_and_path_values() {
    let env = TestEnv::default();
    env.set("SYMP_WORKSPACE_ROOT_1", "/tmp/symphony-workspace-root");
    env.set("SYMP_LINEAR_API_KEY_1", "resolved-secret");
    let h = Harness::with_env(env);
    h.write(&[
        ("tracker_api_token", json!("$SYMP_LINEAR_API_KEY_1")),
        ("workspace_root", json!("$SYMP_WORKSPACE_ROOT_1")),
        ("codex_command", json!("~/bin/codex app-server")),
    ]);
    let config = h.store.settings();
    assert_eq!(config.tracker.api_key.as_deref(), Some("resolved-secret"));
    assert_eq!(config.tracker.provider["api_key"], "$SYMP_LINEAR_API_KEY_1");
    assert_eq!(
        config.tracker.secret_environment_names,
        strings(&["LINEAR_API_KEY", "SYMP_LINEAR_API_KEY_1"])
    );
    assert_eq!(
        config.secret_environment_names(),
        strings(&["LINEAR_API_KEY", "SYMP_LINEAR_API_KEY_1"])
    );
    assert_eq!(config.workspace.root, "/tmp/symphony-workspace-root");
    assert_eq!(config.codex.command, "~/bin/codex app-server");
}

#[test]
fn schema_preserves_adapter_owned_provider_config_while_keeping_linear_aliases_compatible() {
    let settings = parse(
        json!({"tracker": {"kind": "linear", "provider": {
            "endpoint": "https://linear.example.test/graphql",
            "api_key": "provider-token",
            "project_slug": "provider-project",
            "extra": {"team": "platform"}
        }}}),
        &MapEnv::new(),
    )
    .unwrap();
    assert_eq!(
        settings.tracker.endpoint.as_deref(),
        Some("https://linear.example.test/graphql")
    );
    assert_eq!(settings.tracker.api_key.as_deref(), Some("provider-token"));
    assert_eq!(
        settings.tracker.project_slug.as_deref(),
        Some("provider-project")
    );
    assert_eq!(
        settings.tracker.secret_environment_names,
        strings(&["LINEAR_API_KEY"])
    );
    assert_eq!(
        Value::Object(settings.tracker.provider.clone()),
        json!({
            "endpoint": "https://linear.example.test/graphql",
            "api_key": "provider-token",
            "project_slug": "provider-project",
            "assignee": null,
            "extra": {"team": "platform"}
        })
    );
}

#[test]
fn provider_keys_win_over_legacy_flat_linear_keys() {
    let settings = parse(
        json!({"tracker": {"kind": "linear", "endpoint": "https://flat.example/graphql", "api_key": "flat",
            "project_slug": "flat-project", "provider": {"api_key": "provider", "project_slug": null}}}),
        &MapEnv::new(),
    )
    .unwrap();
    assert_eq!(settings.tracker.api_key.as_deref(), Some("provider"));
    // `project_slug: null` is dropped before casting, so the flat alias fills it.
    assert_eq!(
        settings.tracker.project_slug.as_deref(),
        Some("flat-project")
    );
    assert_eq!(
        settings.tracker.endpoint.as_deref(),
        Some("https://flat.example/graphql")
    );
}

#[test]
fn linear_adapter_rejects_invalid_provider_values_without_crashing_config_parsing() {
    let env = MapEnv::new();
    let cases = [
        (
            json!({"api_key": 123, "project_slug": "project"}),
            TrackerConfigError::MissingLinearApiToken,
        ),
        (
            json!({"api_key": "token", "project_slug": "project", "endpoint": 123}),
            TrackerConfigError::InvalidLinearEndpoint,
        ),
        (
            json!({"api_key": "token", "project_slug": "project", "assignee": 123}),
            TrackerConfigError::InvalidLinearAssignee,
        ),
    ];
    for (provider, expected) in cases {
        let settings = parse(
            json!({"tracker": {"kind": "linear", "provider": provider}}),
            &env,
        )
        .unwrap();
        assert_eq!(
            config::validate_settings(&settings, &env),
            Err(expected.into())
        );
    }
}

#[test]
fn schema_does_not_inject_linear_defaults_before_an_adapter_is_selected() {
    let env = MapEnv::new();
    let settings = parse(json!({"tracker": {"kind": "future-tracker"}}), &env).unwrap();
    assert_eq!(settings.tracker.endpoint, None);
    assert_eq!(settings.tracker.api_key, None);
    assert_eq!(settings.tracker.active_states, None);
    assert_eq!(settings.tracker.terminal_states, None);
    assert!(settings.tracker.provider.is_empty());
    assert_eq!(
        config::validate_settings(&settings, &env),
        Err(ConfigError::UnsupportedTrackerKind("future-tracker".into()))
    );
    let no_kind = parse(json!({}), &env).unwrap();
    assert_eq!(
        config::validate_settings(&no_kind, &env),
        Err(ConfigError::MissingTrackerKind)
    );
}

#[test]
fn config_no_longer_resolves_legacy_env_references() {
    let env = TestEnv::default();
    env.set("SYMP_WORKSPACE_ROOT_2", "/tmp/symphony-workspace-root");
    env.set("SYMP_LINEAR_API_KEY_2", "resolved-secret");
    let h = Harness::with_env(env);
    h.write(&[
        ("tracker_api_token", json!("env:SYMP_LINEAR_API_KEY_2")),
        ("workspace_root", json!("env:SYMP_WORKSPACE_ROOT_2")),
    ]);
    let config = h.store.settings();
    assert_eq!(
        config.tracker.api_key.as_deref(),
        Some("env:SYMP_LINEAR_API_KEY_2")
    );
    assert_eq!(config.workspace.root, "env:SYMP_WORKSPACE_ROOT_2");
}

#[test]
fn config_supports_per_state_max_concurrent_agent_overrides() {
    let h = Harness::new();
    let workflow = "---\ntracker:\n  kind: memory\nagent:\n  max_concurrent_agents: 10\n  max_concurrent_agents_by_state:\n    todo: 1\n    \"In Progress\": 4\n    \"In Review\": 2\n---\n";
    // A raw write without an explicit reload is picked up through stamp detection.
    fs::write(h.path(), workflow).unwrap();

    let settings = h.store.settings();
    assert_eq!(settings.agent.max_concurrent_agents, 10);
    assert_eq!(settings.max_concurrent_agents_for_state(Some("Todo")), 1);
    assert_eq!(
        settings.max_concurrent_agents_for_state(Some("In Progress")),
        4
    );
    assert_eq!(
        settings.max_concurrent_agents_for_state(Some("In Review")),
        2
    );
    assert_eq!(settings.max_concurrent_agents_for_state(Some("Closed")), 10);
    assert_eq!(settings.max_concurrent_agents_for_state(None), 10);

    assert_eq!(
        h.write_and_validate(&[("worker_max_concurrent_agents_per_host", json!(2))]),
        Ok(())
    );
    assert_eq!(
        h.store.settings().worker.max_concurrent_agents_per_host,
        Some(2)
    );
}

#[test]
fn per_state_limits_reject_the_whole_config_on_one_bad_entry() {
    let env = MapEnv::new();
    let err = parse(
        json!({"agent": {"max_concurrent_agents_by_state": {"": 1, "todo": 0, "x": "1"}}}),
        &env,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ConfigError::InvalidWorkflowConfig(
            "agent.max_concurrent_agents_by_state state names must not be blank, \
             agent.max_concurrent_agents_by_state limits must be positive integers, \
             agent.max_concurrent_agents_by_state limits must be positive integers"
                .into()
        )
    );
    assert_eq!(
        parse(
            json!({"agent": {"max_concurrent_agents_by_state": "x"}}),
            &env
        )
        .unwrap_err(),
        ConfigError::InvalidWorkflowConfig(
            "agent.max_concurrent_agents_by_state is invalid".into()
        )
    );
}

#[test]
fn schema_helpers_cover_custom_type_and_state_limit_validation() {
    assert_eq!(
        StringOrMap::cast(&json!("value")),
        Some(StringOrMap::String("value".into()))
    );
    assert_eq!(
        StringOrMap::cast(&json!({"a": 1})),
        Some(StringOrMap::Map(obj(json!({"a": 1}))))
    );
    assert_eq!(StringOrMap::cast(&json!(123)), None);
    assert_eq!(StringOrMap::cast(&json!(["a"])), None);
    assert_eq!(
        StringOrMap::Map(obj(json!({"a": 1}))),
        StringOrMap::Map(obj(json!({"a": 1})))
    );
    assert_ne!(
        StringOrMap::Map(obj(json!({"a": 1}))),
        StringOrMap::Map(obj(json!({"a": 2})))
    );

    assert!(normalize_state_limits(&Map::new()).is_empty());
    let normalized = normalize_state_limits(&obj(json!({" In Progress ": 2, "todo": 1})));
    let expected: BTreeMap<String, Value> = [
        ("todo".to_string(), json!(1)),
        ("in progress".to_string(), json!(2)),
    ]
    .into_iter()
    .collect();
    assert_eq!(normalized, expected);

    let limits = normalize_state_limits(&obj(json!({"": 1, "todo": 0})));
    assert_eq!(
        validate_state_limits(&limits),
        vec![
            "state names must not be blank",
            "limits must be positive integers"
        ]
    );
    let limits = normalize_state_limits(&obj(json!({"   ": 1})));
    assert_eq!(
        validate_state_limits(&limits),
        vec!["state names must not be blank"]
    );
}

#[test]
fn schema_parse_normalizes_policy_keys_and_env_backed_fallbacks() {
    let env = MapEnv::new()
        .with("SYMP_EMPTY_SECRET_1", "")
        .with("LINEAR_API_KEY", "fallback-linear-token");

    let settings = parse(
        json!({
            "tracker": {"kind": "linear", "api_key": "$SYMP_EMPTY_SECRET_1"},
            "workspace": {"root": "$SYMP_MISSING_WORKSPACE_1"},
            "codex": {"approval_policy": {"reject": {"sandbox_approval": true}}}
        }),
        &env,
    )
    .unwrap();
    // An empty env value beats the LINEAR_API_KEY fallback.
    assert_eq!(settings.tracker.api_key, None);
    assert_eq!(settings.workspace.root, default_workspace_root());
    assert_eq!(
        settings.codex.approval_policy.to_value(),
        json!({"reject": {"sandbox_approval": true}})
    );

    let settings = parse(
        json!({
            "tracker": {"kind": "linear", "api_key": "$SYMP_MISSING_SECRET_1"},
            "workspace": {"root": ""}
        }),
        &env,
    )
    .unwrap();
    assert_eq!(
        settings.tracker.api_key.as_deref(),
        Some("fallback-linear-token")
    );
    assert_eq!(settings.workspace.root, default_workspace_root());

    // `$1bad` is not a reference and is kept literally; whitespace is not trimmed on the Linear path.
    let settings = parse(
        json!({"tracker": {"kind": "linear", "api_key": "$1bad"}}),
        &env,
    )
    .unwrap();
    assert_eq!(settings.tracker.api_key.as_deref(), Some("$1bad"));
    assert_eq!(
        settings.tracker.secret_environment_names,
        strings(&["LINEAR_API_KEY"])
    );
    let settings = parse(
        json!({"workspace": {"root": "$EMPTY_ROOT"}}),
        &MapEnv::new().with("EMPTY_ROOT", ""),
    )
    .unwrap();
    assert_eq!(settings.workspace.root, default_workspace_root());
}

fn sandbox_settings(policy: Option<Value>, root: &str) -> Settings {
    Settings {
        codex: CodexSettings {
            turn_sandbox_policy: policy.map(obj),
            ..CodexSettings::default()
        },
        workspace: WorkspaceSettings { root: root.into() },
        ..Settings::default()
    }
}

#[test]
fn continuation_delay_defaults_to_one_second_and_must_be_positive() {
    let env = MapEnv::new();
    let agent = |agent: Value| {
        parse(json!({"tracker": {"kind": "memory"}, "agent": agent}), &env)
            .map(|settings| settings.agent.continuation_delay_ms)
    };
    assert_eq!(agent(json!({})).unwrap(), 1_000);
    assert_eq!(
        agent(json!({"continuation_delay_ms": 15000})).unwrap(),
        15_000
    );
    match agent(json!({"continuation_delay_ms": 0})) {
        Err(ConfigError::InvalidWorkflowConfig(message)) => {
            assert!(message.contains("agent.continuation_delay_ms must be greater than 0"));
        }
        other => panic!("expected an invalid config, got {other:?}"),
    }
}

#[test]
fn codex_model_and_provider_are_optional_and_validated() {
    let env = MapEnv::new();
    let codex = |codex: Value| {
        parse(json!({"tracker": {"kind": "memory"}, "codex": codex}), &env)
            .map(|settings| settings.codex)
    };

    let defaults = codex(json!({})).unwrap();
    assert_eq!(defaults.model, None);
    assert_eq!(defaults.provider, None);

    let settings = codex(json!({
        "model": "qwen3-coder",
        "provider": {
            "name": "ollama",
            "base_url": "http://127.0.0.1:11434/v1",
            "api_key_env": "OLLAMA_API_KEY"
        }
    }))
    .unwrap();
    assert_eq!(settings.model.as_deref(), Some("qwen3-coder"));
    assert_eq!(
        settings.provider,
        Some(CodexProvider {
            name: "ollama".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            api_key_env: Some("OLLAMA_API_KEY".into()),
        })
    );

    for (bad, fragment) in [
        (json!({"model": "two words"}), "codex.model is invalid"),
        (json!({"provider": "ollama"}), "codex.provider is invalid"),
        (
            json!({"provider": {"base_url": "https://api.example.com/v1"}}),
            "codex.provider.name can't be blank",
        ),
        (
            json!({"provider": {"name": "a.b", "base_url": "https://api.example.com/v1"}}),
            "codex.provider.name is invalid",
        ),
        (
            json!({"provider": {"name": "x"}}),
            "codex.provider.base_url can't be blank",
        ),
        (
            json!({"provider": {"name": "x", "base_url": "ftp://example.com"}}),
            "codex.provider.base_url is invalid",
        ),
        (
            json!({"provider": {"name": "x", "base_url": "https://e.com/v1", "api_key_env": "sk-123"}}),
            "codex.provider.api_key_env is invalid",
        ),
    ] {
        match codex(bad) {
            Err(ConfigError::InvalidWorkflowConfig(message)) => {
                assert!(
                    message.contains(fragment),
                    "{message:?} should contain {fragment:?}"
                );
            }
            other => panic!("expected invalid config containing {fragment:?}, got {other:?}"),
        }
    }
}

#[test]
fn schema_resolves_sandbox_policies_from_explicit_and_default_workspaces() {
    let explicit = json!({"type": "workspaceWrite", "writableRoots": ["/tmp/explicit"]});
    assert_eq!(
        Value::Object(
            sandbox_settings(Some(explicit.clone()), "/tmp/ignored")
                .resolve_turn_sandbox_policy(None)
        ),
        explicit
    );
    let default_root = expand_path(default_workspace_root(), None);
    assert_eq!(
        sandbox_settings(None, "").resolve_turn_sandbox_policy(None),
        default_turn_sandbox_policy(&default_root.to_string_lossy())
    );
    assert_eq!(
        sandbox_settings(None, "/tmp/ignored").resolve_turn_sandbox_policy(Some("/tmp/workspace")),
        default_turn_sandbox_policy("/tmp/workspace")
    );
}

#[test]
fn schema_keeps_workspace_roots_raw_while_sandbox_helpers_expand_only_for_local_use() {
    let settings = parse(
        json!({"workspace": {"root": "~/.symphony-workspaces"}, "codex": {}}),
        &MapEnv::new(),
    )
    .unwrap();
    assert_eq!(settings.workspace.root, "~/.symphony-workspaces");
    assert_eq!(
        settings.resolve_turn_sandbox_policy(None),
        default_turn_sandbox_policy(&expand_path("~/.symphony-workspaces", None).to_string_lossy())
    );
    assert_eq!(
        settings
            .resolve_runtime_turn_sandbox_policy(None, true, None)
            .unwrap(),
        default_turn_sandbox_policy("~/.symphony-workspaces")
    );
}

#[test]
fn runtime_sandbox_policy_resolution_passes_explicit_policies_through_unchanged() {
    let h = Harness::new();
    let test_root = tempfile::tempdir().unwrap();
    let workspace_root = test_root.path().join("workspaces");
    let issue_workspace = workspace_root.join("MT-100");
    fs::create_dir_all(&issue_workspace).unwrap();
    let ws = issue_workspace.to_string_lossy().into_owned();

    h.write(&[
        ("workspace_root", json!(workspace_root.to_string_lossy())),
        (
            "codex_turn_sandbox_policy",
            json!({"type": "workspaceWrite", "writableRoots": ["relative/path"], "networkAccess": true}),
        ),
    ]);
    let runtime = h.store.codex_runtime_settings(Some(&ws), false).unwrap();
    assert_eq!(
        Value::Object(runtime.turn_sandbox_policy),
        json!({"type": "workspaceWrite", "writableRoots": ["relative/path"], "networkAccess": true})
    );

    h.write(&[
        ("workspace_root", json!(workspace_root.to_string_lossy())),
        (
            "codex_turn_sandbox_policy",
            json!({"type": "futureSandbox", "nested": {"flag": true}}),
        ),
    ]);
    let runtime = h.store.codex_runtime_settings(Some(&ws), false).unwrap();
    assert_eq!(
        Value::Object(runtime.turn_sandbox_policy),
        json!({"type": "futureSandbox", "nested": {"flag": true}})
    );
    assert_eq!(runtime.thread_sandbox, "workspace-write");
}

#[test]
fn runtime_sandbox_policy_resolution_defaults_when_omitted_and_ignores_workspace_for_explicit_policies()
 {
    let h = Harness::new();
    let test_root = tempfile::tempdir().unwrap();
    let workspace_root = test_root.path().join("workspaces");
    fs::create_dir_all(workspace_root.join("MT-101")).unwrap();
    h.write(&[("workspace_root", json!(workspace_root.to_string_lossy()))]);

    let settings = h.store.settings();
    let canonical_root = path_safety::canonicalize(&workspace_root).unwrap();
    let default_policy = settings
        .resolve_runtime_turn_sandbox_policy(None, false, None)
        .unwrap();
    assert_eq!(default_policy["type"], "workspaceWrite");
    assert_eq!(
        default_policy["writableRoots"],
        json!([canonical_root.to_string_lossy()])
    );
    assert_eq!(
        settings
            .resolve_runtime_turn_sandbox_policy(Some(""), false, None)
            .unwrap(),
        default_policy
    );

    let mut read_only = (*settings).clone();
    read_only.codex.turn_sandbox_policy =
        Some(obj(json!({"type": "readOnly", "networkAccess": true})));
    assert_eq!(
        Value::Object(
            read_only
                .resolve_runtime_turn_sandbox_policy(Some("123"), false, None)
                .unwrap()
        ),
        json!({"type": "readOnly", "networkAccess": true})
    );
}

#[test]
fn runtime_sandbox_policy_resolves_relative_roots_against_the_workflow_directory() {
    let h = Harness::new();
    fs::create_dir_all(h.dir.path().join("relative-workspaces")).unwrap();
    h.write(&[("workspace_root", json!("relative-workspaces"))]);
    let runtime = h.store.codex_runtime_settings(None, false).unwrap();
    let expected = path_safety::canonicalize(h.dir.path().join("relative-workspaces")).unwrap();
    assert_eq!(
        runtime.turn_sandbox_policy["writableRoots"],
        json!([expected.to_string_lossy()])
    );
    assert_eq!(
        h.store.local_workspace_root(),
        h.dir.path().join("relative-workspaces")
    );
}

#[test]
fn runtime_sandbox_policy_surfaces_canonicalize_failures() {
    let settings = sandbox_settings(None, "/tmp");
    let long = format!("/tmp/{}", "a".repeat(300));
    assert!(matches!(
        settings.resolve_runtime_turn_sandbox_policy(Some(&long), false, None),
        Err(PathError::CanonicalizeFailed { .. })
    ));
}

#[test]
fn workflow_prompt_is_used_when_building_base_prompt() {
    let h = Harness::new();
    h.write(&[(
        "prompt",
        json!("Workflow prompt body used as codex instruction."),
    )]);
    assert_eq!(
        h.store.workflow_prompt(),
        "Workflow prompt body used as codex instruction."
    );
    h.write(&[("prompt", json!("   "))]);
    assert_eq!(
        h.store.workflow_prompt(),
        symphony_core::prompt::DEFAULT_PROMPT_TEMPLATE
    );
}

#[test]
fn local_workspace_root_resolves_from_the_workflow_directory() {
    let settings = sandbox_settings(None, "relative-workspaces");
    assert_eq!(
        settings.local_workspace_root(Path::new("/srv/flows/WORKFLOW.md")),
        Path::new("/srv/flows/relative-workspaces")
    );
    let settings = sandbox_settings(None, "/abs/root/../ws");
    assert_eq!(
        settings.local_workspace_root(Path::new("/srv/WORKFLOW.md")),
        Path::new("/abs/ws")
    );
}

#[test]
fn server_port_override_wins() {
    let mut settings = Settings::default();
    assert_eq!(settings.server_port(None), None);
    settings.server.port = Some(4000);
    assert_eq!(settings.server_port(None), Some(4000));
    assert_eq!(settings.server_port(Some(0)), Some(0));
}

#[test]
fn ecto_cast_rules_for_scalars() {
    let env = MapEnv::new();
    let settings = parse(
        json!({
            "polling": {"interval_ms": "+5"},
            "server": {"port": "8080", "host": ""},
            "observability": {"dashboard_enabled": "0"},
            "unknown_section": {"x": 1},
            "agent": {"unknown_key": true, "max_turns": "7"}
        }),
        &env,
    )
    .unwrap();
    assert_eq!(settings.polling.interval_ms, 5);
    assert_eq!(settings.server.port, Some(8080));
    assert_eq!(settings.server.host, "");
    assert!(!settings.observability.dashboard_enabled);
    assert_eq!(settings.agent.max_turns, 7);

    let invalid = |value: Value| match parse(value, &env) {
        Err(ConfigError::InvalidWorkflowConfig(message)) => message,
        other => panic!("expected invalid config, got {other:?}"),
    };
    assert_eq!(invalid(json!({"tracker": "x"})), "tracker is invalid");
    assert_eq!(
        invalid(json!({"codex": {"command": 12}})),
        "codex.command is invalid"
    );
    assert_eq!(
        invalid(json!({"polling": {"interval_ms": 1.5}})),
        "polling.interval_ms is invalid"
    );
    assert_eq!(
        invalid(json!({"polling": {"interval_ms": " 5"}})),
        "polling.interval_ms is invalid"
    );
    assert_eq!(
        invalid(json!({"polling": {"interval_ms": true}})),
        "polling.interval_ms is invalid"
    );
    assert_eq!(
        invalid(json!({"observability": {"dashboard_enabled": "TRUE"}})),
        "observability.dashboard_enabled is invalid"
    );
    assert_eq!(
        invalid(json!({"worker": {"ssh_hosts": "a"}})),
        "worker.ssh_hosts is invalid"
    );
    assert_eq!(
        invalid(json!({"hooks": {"after_create": 5}})),
        "hooks.after_create is invalid"
    );
    assert_eq!(
        invalid(json!({"tracker": {"kind": 5}})),
        "tracker.kind is invalid"
    );
    assert_eq!(
        invalid(json!({"tracker": {"provider": "x"}})),
        "tracker.provider is invalid"
    );
    assert_eq!(
        invalid(json!({"tracker": {"required_labels": [null]}})),
        "tracker.required_labels is invalid"
    );
    assert_eq!(
        invalid(json!({"codex": {"stall_timeout_ms": -1}})),
        "codex.stall_timeout_ms must be greater than or equal to 0"
    );
    assert_eq!(
        invalid(json!({"server": {"port": 70000}})),
        "server.port must be less than or equal to 65535"
    );
    assert_eq!(
        invalid(json!({"agent": {"max_turns": 0, "max_concurrent_agents": "bad"}})),
        "agent.max_concurrent_agents is invalid, agent.max_turns must be greater than 0"
    );

    // Zero stall timeout is valid (disables stall detection).
    assert_eq!(
        parse(json!({"codex": {"stall_timeout_ms": 0}}), &env)
            .unwrap()
            .codex
            .stall_timeout_ms,
        0
    );
    // `null` behaves like an absent key everywhere, including nested provider maps.
    let settings = parse(
        json!({"polling": {"interval_ms": null}, "tracker": {"kind": "github", "provider": {"a": null, "b": {"c": null}}}}),
        &env,
    )
    .unwrap();
    assert_eq!(settings.polling.interval_ms, 30_000);
    assert_eq!(Value::Object(settings.tracker.provider), json!({"b": {}}));
}

#[test]
fn yaml_front_matter_scalars_and_keys() {
    let loaded = workflow::parse(
        "---\ntracker:\n  kind: memory\n  required_labels: [yes, \"No\"]\nagent:\n  max_concurrent_agents_by_state:\n    1: 2\n---\n",
    )
    .unwrap();
    let settings = config::parse(&loaded.config, &MapEnv::new()).unwrap();
    assert_eq!(settings.tracker.required_labels, strings(&["yes", "no"]));
    assert_eq!(
        settings.agent.max_concurrent_agents_by_state.get("1"),
        Some(&2)
    );
    // Duplicate keys are rejected instead of silently keeping one value.
    assert!(matches!(
        workflow::parse("---\npolling:\n  interval_ms: 1\n  interval_ms: 2\n---\n"),
        Err(ConfigError::WorkflowParseError(_))
    ));
}

#[test]
fn tracker_settings_debug_redacts_secrets() {
    let env = MapEnv::new();
    let settings = parse(
        json!({"tracker": {"kind": "linear", "api_key": "super-secret", "provider": {"token": "also-secret", "team": "x"}}}),
        &env,
    )
    .unwrap();
    let debug = format!("{:?}", settings.tracker);
    assert!(!debug.contains("super-secret"), "{debug}");
    assert!(!debug.contains("also-secret"), "{debug}");
    assert!(debug.contains("<redacted>"));
    assert!(debug.contains("\"x\""));
}

#[test]
fn config_error_messages_match_elixir_format_config_error() {
    assert_eq!(
        ConfigError::InvalidWorkflowConfig("polling.interval_ms is invalid".into()).user_message(),
        "Invalid WORKFLOW.md config: polling.interval_ms is invalid"
    );
    assert_eq!(
        ConfigError::WorkflowFrontMatterNotAMap.user_message(),
        "Failed to parse WORKFLOW.md: workflow front matter must decode to a map"
    );
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.md");
    let err = workflow::load(&missing).unwrap_err();
    assert_eq!(
        err.user_message(),
        format!("Missing WORKFLOW.md at {}: :enoent", missing.display())
    );
    assert_eq!(err.tag(), "missing_workflow_file");
    assert_eq!(
        ConfigError::from(TrackerConfigError::MissingLinearProjectSlug).user_message(),
        "Invalid WORKFLOW.md config: :missing_linear_project_slug"
    );
    assert_eq!(
        ConfigError::UnsupportedTrackerKind("123".into()).user_message(),
        "Invalid WORKFLOW.md config: {:unsupported_tracker_kind, \"123\"}"
    );
    assert_eq!(
        ConfigError::UnsupportedTrackerKind("123".into()).to_string(),
        "unsupported_tracker_kind: \"123\""
    );
    assert_eq!(
        TrackerConfigError::MissingGithubToken.to_string(),
        "missing_github_token"
    );
    assert_eq!(
        ConfigError::MissingTrackerKind.to_string(),
        "missing_tracker_kind"
    );
}

#[test]
fn write_workflow_helper_renders_parseable_yaml_for_every_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("WORKFLOW.md");
    write_workflow(
        &path,
        &[
            ("hook_after_create", json!("echo one\necho two")),
            ("hook_before_remove", json!("echo bye")),
            ("worker_ssh_hosts", json!(["worker-01:2200"])),
            ("server_port", json!(0)),
        ],
    );
    let snapshot = symphony_core::workflow_store::load_snapshot(&path, &MapEnv::new()).unwrap();
    let settings = &snapshot.settings;
    assert_eq!(
        settings.hooks.after_create.as_deref(),
        Some("echo one\necho two\n")
    );
    assert_eq!(settings.hooks.before_remove.as_deref(), Some("echo bye\n"));
    assert_eq!(settings.hooks.timeout_ms, 60_000);
    assert_eq!(settings.worker.ssh_hosts, strings(&["worker-01:2200"]));
    assert_eq!(settings.server.port, Some(0));
    assert_eq!(settings.server.host, "127.0.0.1");
    assert_eq!(snapshot.workflow.prompt, support::WORKFLOW_PROMPT);
}
