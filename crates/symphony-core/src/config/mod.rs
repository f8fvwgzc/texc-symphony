//! Runtime configuration from `WORKFLOW.md` front matter (`SymphonyElixir.Config` + `Config.Schema`).
//!
//! [`parse`] casts the decoded front matter with Ecto semantics into [`Settings`];
//! [`validate_settings`] is the dispatch preflight (tracker kind + adapter checks). Both run on every
//! (re)load inside [`crate::WorkflowStore`].

mod cast;
pub mod schema;
pub mod tracker;
pub mod value;

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub use cast::{cast_boolean, cast_integer, cast_string_list};
pub use schema::{
    AgentSettings, CodexSettings, HooksSettings, ObservabilitySettings, PollingSettings,
    ServerSettings, Settings, StringOrMap, TrackerSettings, WorkerSettings, WorkspaceSettings,
    default_approval_policy, normalize_state_limits, parse, validate_state_limits,
};
pub use tracker::{
    AsanaSettings, GitHubSettings, GitLabSettings, JiraSettings, LinearSettings,
    SUPPORTED_TRACKER_KINDS, resolve_asana, resolve_github, resolve_gitlab, resolve_jira,
    resolve_linear, secret_environment_names,
};

use crate::env::EnvSource;
use crate::error::ConfigError;
use crate::issue::normalize_state;
use crate::path_safety::{self, PathError};

/// `Config.validate_settings/1`: `tracker.kind` must be present, then the adapter validates its settings.
pub fn validate_settings(settings: &Settings, env: &dyn EnvSource) -> Result<(), ConfigError> {
    if settings.tracker.kind.is_none() {
        return Err(ConfigError::MissingTrackerKind);
    }
    tracker::validate_tracker(&settings.tracker, env)
}

/// The default Codex turn sandbox policy for a writable root (camelCase keys are passed to Codex).
pub fn default_turn_sandbox_policy(writable_root: &str) -> Map<String, Value> {
    let mut read_only = Map::new();
    read_only.insert("type".into(), Value::String("fullAccess".into()));
    let mut policy = Map::new();
    policy.insert("type".into(), Value::String("workspaceWrite".into()));
    policy.insert(
        "writableRoots".into(),
        Value::Array(vec![Value::String(writable_root.to_owned())]),
    );
    policy.insert("readOnlyAccess".into(), Value::Object(read_only));
    policy.insert("networkAccess".into(), Value::Bool(false));
    policy.insert("excludeTmpdirEnvVar".into(), Value::Bool(false));
    policy.insert("excludeSlashTmp".into(), Value::Bool(false));
    policy
}

/// Effective Codex runtime settings for one session (`Config.codex_runtime_settings/2`).
#[derive(Clone, Debug, PartialEq)]
pub struct CodexRuntimeSettings {
    /// `codex.approval_policy`.
    pub approval_policy: StringOrMap,
    /// `codex.thread_sandbox`.
    pub thread_sandbox: String,
    /// Explicit policy, or the default policy rooted at the workspace.
    pub turn_sandbox_policy: Map<String, Value>,
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl Settings {
    /// `Config.max_concurrent_agents_for_state/1`: per-state cap (trim + lowercase lookup), else the
    /// global `agent.max_concurrent_agents`.
    pub fn max_concurrent_agents_for_state(&self, state: Option<&str>) -> u32 {
        state
            .and_then(|s| {
                self.agent
                    .max_concurrent_agents_by_state
                    .get(&normalize_state(s))
            })
            .copied()
            .unwrap_or(self.agent.max_concurrent_agents)
    }

    /// `Config.server_port/0`: the CLI override (if any) wins over `server.port`.
    pub fn server_port(&self, override_port: Option<u16>) -> Option<u16> {
        override_port.or(self.server.port)
    }

    /// `Config.local_workspace_root/0`: `workspace.root` expanded (`~`, `.`/`..`) relative to the directory
    /// of the (expanded) workflow file.
    pub fn local_workspace_root(&self, workflow_file: &Path) -> PathBuf {
        let workflow = path_safety::expand_path(workflow_file, None);
        let dir = workflow
            .parent()
            .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
        path_safety::expand_path(&self.workspace.root, Some(&dir))
    }

    fn sandbox_root<'a>(&'a self, workspace: Option<&'a str>) -> &'a str {
        match workspace {
            Some(ws) if !ws.is_empty() => ws,
            _ => &self.workspace.root,
        }
    }

    fn expand_local_root(root: &str, base_dir: Option<&Path>) -> PathBuf {
        if root.is_empty() {
            path_safety::expand_path(
                std::env::temp_dir().join(schema::DEFAULT_WORKSPACE_DIR_NAME),
                None,
            )
        } else {
            path_safety::expand_path(root, base_dir)
        }
    }

    /// `Schema.resolve_turn_sandbox_policy/2`: explicit policy unchanged, else the default policy rooted at
    /// the workspace (or `workspace.root`), expanded against the process CWD without canonicalization.
    pub fn resolve_turn_sandbox_policy(&self, workspace: Option<&str>) -> Map<String, Value> {
        if let Some(policy) = &self.codex.turn_sandbox_policy {
            return policy.clone();
        }
        let root = Self::expand_local_root(self.sandbox_root(workspace), None);
        default_turn_sandbox_policy(&path_string(&root))
    }

    /// `Schema.resolve_runtime_turn_sandbox_policy/3`.
    ///
    /// An explicit policy is returned unchanged (the workspace is ignored). Otherwise the root is the
    /// non-empty `workspace`, else `workspace.root`:
    /// - `remote == true`: used raw (e.g. `~/.symphony-remote-workspaces` stays literal);
    /// - local: expanded (relative roots against `base_dir` when given — normally the workflow directory —
    ///   else the process CWD, which is the Elixir behaviour) and canonicalized.
    pub fn resolve_runtime_turn_sandbox_policy(
        &self,
        workspace: Option<&str>,
        remote: bool,
        base_dir: Option<&Path>,
    ) -> Result<Map<String, Value>, PathError> {
        if let Some(policy) = &self.codex.turn_sandbox_policy {
            return Ok(policy.clone());
        }
        let root = self.sandbox_root(workspace);
        if remote {
            return Ok(default_turn_sandbox_policy(root));
        }
        let canonical = path_safety::canonicalize(Self::expand_local_root(root, base_dir))?;
        Ok(default_turn_sandbox_policy(&path_string(&canonical)))
    }

    /// `Config.codex_runtime_settings/2`.
    pub fn codex_runtime_settings(
        &self,
        workspace: Option<&str>,
        remote: bool,
        base_dir: Option<&Path>,
    ) -> Result<CodexRuntimeSettings, PathError> {
        Ok(CodexRuntimeSettings {
            approval_policy: self.codex.approval_policy.clone(),
            thread_sandbox: self.codex.thread_sandbox.clone(),
            turn_sandbox_policy: self
                .resolve_runtime_turn_sandbox_policy(workspace, remote, base_dir)?,
        })
    }

    /// Env var names to strip from the Codex child for the configured tracker.
    pub fn secret_environment_names(&self) -> Vec<String> {
        tracker::secret_environment_names(&self.tracker)
    }

    /// Active states, or an empty list when unset.
    pub fn active_states(&self) -> &[String] {
        self.tracker.active_states.as_deref().unwrap_or_default()
    }

    /// Terminal states, or an empty list when unset.
    pub fn terminal_states(&self) -> &[String] {
        self.tracker.terminal_states.as_deref().unwrap_or_default()
    }
}
