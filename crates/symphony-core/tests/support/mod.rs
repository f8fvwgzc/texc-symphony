//! Shared test harness: the Rust twin of `test/support/test_support.exs` (`write_workflow_file!/2` with
//! the same defaults and YAML rendering) plus an in-memory, mutable environment.

#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde_json::{Value, json};
use symphony_core::workflow_store::WorkflowStoreOptions;
use symphony_core::{EnvSource, WorkflowStore};

/// Default prompt written by the harness.
pub const WORKFLOW_PROMPT: &str = "You are an agent for this repository.";

/// A mutable environment shared between the test and the store.
#[derive(Debug, Default)]
pub struct TestEnv {
    vars: RwLock<HashMap<String, String>>,
}

impl TestEnv {
    pub fn set(&self, name: &str, value: &str) {
        self.vars.write().unwrap().insert(name.into(), value.into());
    }

    pub fn remove(&self, name: &str) {
        self.vars.write().unwrap().remove(name);
    }
}

impl EnvSource for TestEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.vars.read().unwrap().get(name).cloned()
    }
}

pub fn default_workspace_root() -> String {
    std::env::temp_dir()
        .join("symphony_workspaces")
        .to_string_lossy()
        .into_owned()
}

fn defaults() -> Vec<(&'static str, Value)> {
    vec![
        ("tracker_kind", json!("linear")),
        ("tracker_endpoint", json!("https://api.linear.app/graphql")),
        ("tracker_api_token", json!("token")),
        ("tracker_project_slug", json!("project")),
        ("tracker_assignee", Value::Null),
        ("tracker_required_labels", json!([])),
        ("tracker_active_states", json!(["Todo", "In Progress"])),
        (
            "tracker_terminal_states",
            json!(["Closed", "Cancelled", "Canceled", "Duplicate", "Done"]),
        ),
        ("poll_interval_ms", json!(30_000)),
        ("workspace_root", json!(default_workspace_root())),
        ("worker_ssh_hosts", json!([])),
        ("worker_max_concurrent_agents_per_host", Value::Null),
        ("max_concurrent_agents", json!(10)),
        ("max_turns", json!(20)),
        ("max_retry_backoff_ms", json!(300_000)),
        ("max_concurrent_agents_by_state", json!({})),
        ("codex_command", json!("codex app-server")),
        (
            "codex_approval_policy",
            json!({"granular": {"sandbox_approval": false, "rules": false, "mcp_elicitations": false}}),
        ),
        ("codex_thread_sandbox", json!("workspace-write")),
        ("codex_turn_sandbox_policy", Value::Null),
        ("codex_turn_timeout_ms", json!(3_600_000)),
        ("codex_read_timeout_ms", json!(5_000)),
        ("codex_stall_timeout_ms", json!(300_000)),
        ("hook_after_create", Value::Null),
        ("hook_before_run", Value::Null),
        ("hook_after_run", Value::Null),
        ("hook_before_remove", Value::Null),
        ("hook_timeout_ms", json!(60_000)),
        ("observability_enabled", json!(true)),
        ("observability_refresh_ms", json!(1_000)),
        ("observability_render_interval_ms", json!(16)),
        ("server_port", Value::Null),
        ("server_host", Value::Null),
        ("prompt", json!(WORKFLOW_PROMPT)),
    ]
}

/// `yaml_value/1` from test_support.exs: double-quoted strings (only `"` escaped), bare ints/bools,
/// `null`, flow lists and flow maps.
pub fn yaml_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", s.replace('"', "\\\"")),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(yaml_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!(
                    "{}: {}",
                    yaml_value(&Value::String(k.clone())),
                    yaml_value(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn hook_entry(name: &str, command: &Value) -> Option<String> {
    let command = command.as_str()?;
    let indented = command
        .split('\n')
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!("  {name}: |\n{indented}"))
}

/// Renders a workflow file exactly like `workflow_content/1` in test_support.exs.
pub fn workflow_content(overrides: &[(&str, Value)]) -> String {
    let mut config: HashMap<&str, Value> = defaults().into_iter().collect();
    for (key, value) in overrides {
        assert!(config.contains_key(key), "unknown workflow override {key}");
        config.insert(key, value.clone());
    }
    let get = |key: &str| config[key].clone();
    let y = |key: &str| yaml_value(&config[key]);

    let mut sections: Vec<String> = vec![
        "---".into(),
        "tracker:".into(),
        format!("  kind: {}", y("tracker_kind")),
        format!("  endpoint: {}", y("tracker_endpoint")),
        format!("  api_key: {}", y("tracker_api_token")),
        format!("  project_slug: {}", y("tracker_project_slug")),
        format!("  assignee: {}", y("tracker_assignee")),
        format!("  required_labels: {}", y("tracker_required_labels")),
        format!("  active_states: {}", y("tracker_active_states")),
        format!("  terminal_states: {}", y("tracker_terminal_states")),
        "polling:".into(),
        format!("  interval_ms: {}", y("poll_interval_ms")),
        "workspace:".into(),
        format!("  root: {}", y("workspace_root")),
    ];

    let hosts = get("worker_ssh_hosts");
    let per_host = get("worker_max_concurrent_agents_per_host");
    let hosts_empty = hosts.is_null() || hosts.as_array().is_some_and(Vec::is_empty);
    if !(hosts_empty && per_host.is_null()) {
        let mut worker = vec!["worker:".to_string()];
        if !hosts_empty {
            worker.push(format!("  ssh_hosts: {}", yaml_value(&hosts)));
        }
        if !per_host.is_null() {
            worker.push(format!(
                "  max_concurrent_agents_per_host: {}",
                yaml_value(&per_host)
            ));
        }
        sections.push(worker.join("\n"));
    }

    sections.extend([
        "agent:".into(),
        format!("  max_concurrent_agents: {}", y("max_concurrent_agents")),
        format!("  max_turns: {}", y("max_turns")),
        format!("  max_retry_backoff_ms: {}", y("max_retry_backoff_ms")),
        format!(
            "  max_concurrent_agents_by_state: {}",
            y("max_concurrent_agents_by_state")
        ),
        "codex:".into(),
        format!("  command: {}", y("codex_command")),
        format!("  approval_policy: {}", y("codex_approval_policy")),
        format!("  thread_sandbox: {}", y("codex_thread_sandbox")),
        format!("  turn_sandbox_policy: {}", y("codex_turn_sandbox_policy")),
        format!("  turn_timeout_ms: {}", y("codex_turn_timeout_ms")),
        format!("  read_timeout_ms: {}", y("codex_read_timeout_ms")),
        format!("  stall_timeout_ms: {}", y("codex_stall_timeout_ms")),
    ]);

    let hooks = ["after_create", "before_run", "after_run", "before_remove"];
    let mut hook_lines = vec![
        "hooks:".to_string(),
        format!("  timeout_ms: {}", y("hook_timeout_ms")),
    ];
    for name in hooks {
        if let Some(entry) = hook_entry(name, &get(&format!("hook_{name}"))) {
            hook_lines.push(entry);
        }
    }
    sections.push(hook_lines.join("\n"));

    sections.push(
        [
            "observability:".to_string(),
            format!("  dashboard_enabled: {}", y("observability_enabled")),
            format!("  refresh_ms: {}", y("observability_refresh_ms")),
            format!(
                "  render_interval_ms: {}",
                y("observability_render_interval_ms")
            ),
        ]
        .join("\n"),
    );

    let port = get("server_port");
    let host = get("server_host");
    if !(port.is_null() && host.is_null()) {
        let mut server = vec!["server:".to_string()];
        if !port.is_null() {
            server.push(format!("  port: {}", yaml_value(&port)));
        }
        if !host.is_null() {
            server.push(format!("  host: {}", yaml_value(&host)));
        }
        sections.push(server.join("\n"));
    }

    sections.push("---".into());
    sections.push(get("prompt").as_str().unwrap_or_default().to_owned());
    let sections: Vec<String> = sections.into_iter().filter(|s| !s.is_empty()).collect();
    sections.join("\n") + "\n"
}

/// `write_workflow_file!/2` without the reload.
pub fn write_workflow(path: &Path, overrides: &[(&str, Value)]) {
    fs::write(path, workflow_content(overrides)).unwrap();
}

/// A temp dir with a default `WORKFLOW.md`, a mutable env and a running store on it.
pub struct Harness {
    pub dir: tempfile::TempDir,
    pub env: Arc<TestEnv>,
    pub store: Arc<WorkflowStore>,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_env(TestEnv::default())
    }

    pub fn with_env(env: TestEnv) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        write_workflow(&path, &[]);
        let env = Arc::new(env);
        let store = WorkflowStore::start_with(WorkflowStoreOptions {
            path: Some(path),
            cwd: dir.path().to_path_buf(),
            env: env.clone(),
        })
        .expect("default workflow boots");
        Self { dir, env, store }
    }

    pub fn path(&self) -> PathBuf {
        self.store.workflow_file_path()
    }

    /// `write_workflow_file!(Workflow.workflow_file_path(), overrides)` (writes, then force-reloads and
    /// ignores the result).
    pub fn write(&self, overrides: &[(&str, Value)]) {
        write_workflow(&self.path(), overrides);
        let _ = self.store.force_reload();
    }

    /// Writes and returns the force-reload result (`Config.validate!/0`).
    pub fn write_and_validate(
        &self,
        overrides: &[(&str, Value)],
    ) -> Result<(), symphony_core::ConfigError> {
        write_workflow(&self.path(), overrides);
        self.store.force_reload()
    }
}

/// `assert message =~ fragment` for an `invalid_workflow_config` error.
pub fn assert_invalid_config(result: Result<(), symphony_core::ConfigError>, fragment: &str) {
    match result {
        Err(symphony_core::ConfigError::InvalidWorkflowConfig(message)) => {
            assert!(
                message.contains(fragment),
                "{message:?} should contain {fragment:?}"
            );
        }
        other => panic!("expected invalid_workflow_config containing {fragment:?}, got {other:?}"),
    }
}
