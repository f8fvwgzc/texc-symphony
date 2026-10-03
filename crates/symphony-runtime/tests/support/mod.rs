//! Shared test harness for symphony-runtime (the Rust twin of `test/support/test_support.exs`):
//! `WORKFLOW.md` writing with Elixir-like defaults (memory tracker), issue builders, fixture paths and
//! polling helpers. Included by the in-crate unit tests and by the integration tests.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use symphony_core::{Issue, WorkflowStore};
use tempfile::TempDir;

/// Prompt written by [`TestWorkflow`].
pub const WORKFLOW_PROMPT: &str = "You are an agent for this repository.";

/// Default front matter (JSON is valid YAML): memory tracker, Elixir test defaults.
pub fn default_config(workspace_root: &Path) -> Value {
    json!({
        "tracker": {
            "kind": "memory",
            "active_states": ["Todo", "In Progress"],
            "terminal_states": ["Closed", "Cancelled", "Canceled", "Duplicate", "Done"],
        },
        "polling": {"interval_ms": 30000},
        "workspace": {"root": workspace_root.to_string_lossy()},
        "agent": {
            "max_concurrent_agents": 10,
            "max_turns": 20,
            "max_retry_backoff_ms": 300000,
        },
        "codex": {
            "command": "codex app-server",
            "thread_sandbox": "workspace-write",
            "turn_timeout_ms": 3600000,
            "read_timeout_ms": 5000,
            "stall_timeout_ms": 300000,
        },
        "hooks": {"timeout_ms": 60000},
        "observability": {"dashboard_enabled": false},
    })
}

/// Recursively merges `patch` into `base` (objects merge, everything else replaces; `null` removes).
pub fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(base), Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    base.remove(key);
                } else {
                    merge(base.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
        }
        (base, patch) => *base = patch.clone(),
    }
}

/// A temp dir holding `WORKFLOW.md`, `workspaces/` and a started [`WorkflowStore`].
pub struct TestWorkflow {
    pub dir: TempDir,
    pub path: PathBuf,
    pub store: Arc<WorkflowStore>,
}

impl TestWorkflow {
    /// Writes the defaults merged with `overrides` and starts the store.
    pub fn new(overrides: Value) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("WORKFLOW.md");
        fs::create_dir_all(dir.path().join("workspaces")).expect("workspaces dir");
        let mut config = default_config(&dir.path().join("workspaces"));
        merge(&mut config, &overrides);
        write_workflow(&path, &config, WORKFLOW_PROMPT);
        let store = WorkflowStore::start(Some(path.clone())).expect("valid workflow");
        Self { dir, path, store }
    }

    /// Rewrites the workflow (defaults merged with `overrides`).
    pub fn rewrite(&self, overrides: Value) {
        let mut config = default_config(&self.workspace_root());
        merge(&mut config, &overrides);
        write_workflow(&self.path, &config, WORKFLOW_PROMPT);
    }

    /// `<dir>/<rel>`.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// The configured workspace root (`<dir>/workspaces`).
    pub fn workspace_root(&self) -> PathBuf {
        self.dir.path().join("workspaces")
    }

    /// The canonical workspace root (macOS temp dirs live under a `/var` -> `/private/var` symlink).
    pub fn canonical_root(&self) -> PathBuf {
        fs::canonicalize(self.workspace_root()).expect("canonical root")
    }
}

/// Writes `---\n<json>\n---\n<prompt>`.
pub fn write_workflow(path: &Path, config: &Value, prompt: &str) {
    let text = format!(
        "---\n{}\n---\n{prompt}\n",
        serde_json::to_string_pretty(config).expect("json")
    );
    fs::write(path, text).expect("write workflow");
}

/// A dispatchable issue `id`/`identifier` in `state`.
pub fn issue(id: &str, identifier: &str, state: &str) -> Issue {
    Issue {
        id: Some(id.to_owned()),
        identifier: Some(identifier.to_owned()),
        title: Some(format!("Issue {identifier}")),
        description: Some("Body".into()),
        state: Some(state.to_owned()),
        url: Some(format!("https://example.org/issues/{identifier}")),
        dispatchable: true,
        ..Issue::default()
    }
}

/// Path of a fixture shipped with this crate (`tests/fixtures/<rel>`).
pub fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

/// Path of a fake app-server shipped with symphony-codex (`crates/symphony-codex/tests/fixtures/codex`).
pub fn codex_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../symphony-codex/tests/fixtures/codex")
        .join(name)
}

/// `'...'` shell quoting.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Copies `source` to `dir/name` and makes it executable (fixtures may lose their mode bits).
pub fn executable_copy(source: &Path, dir: &Path, name: &str) -> PathBuf {
    let target = dir.join(name);
    fs::copy(source, &target).expect("copy fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    target
}

/// Polls `check` every 10 ms until it returns true (panics after `limit`).
pub async fn eventually(limit: Duration, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + limit;
    while !check() {
        assert!(
            std::time::Instant::now() < deadline,
            "condition not met within {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Reads a file, or "" when missing.
pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}
