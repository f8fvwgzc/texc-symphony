//! Per-issue workspaces and lifecycle hooks (`SymphonyElixir.Workspace`, B.10).
//!
//! Layout: `<root>/<workspace_key(identifier)>`.
//! - Local: `root` is `workspace.root` resolved against the `WORKFLOW.md` directory; the workspace path
//!   is canonical (symlinks resolved) and must be strictly inside the canonical root.
//! - Remote (SSH worker): `root` is used raw (`~` is resolved by the remote shell); the returned path is
//!   the remote `pwd -P`.
//!
//! Hooks run with `sh -lc` (locally, cwd = workspace) or `bash -lc` over ssh, bounded by
//! `hooks.timeout_ms`; on timeout the whole process group is killed. `after_create` and `before_run`
//! failures are fatal for the attempt; `after_run` and `before_remove` failures are logged and ignored.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use symphony_codex::launch::shell_escape;
use symphony_core::config::Settings;
use symphony_core::path_safety::{self, PathError};
use symphony_core::{IoReason, Issue, workspace_key};
use tokio::process::Command;

use crate::process::{self, CommandOutput, EnvPolicy, ProcessError};
use crate::ssh::{SshConfig, SshError, SshRunError};

/// Marker line printed by the remote prepare script (`__SYMPHONY_WORKSPACE__\t<created>\t<path>`).
pub const REMOTE_WORKSPACE_MARKER: &str = "__SYMPHONY_WORKSPACE__";
/// Marker printed (with exit status 3) when a remote workspace resolves outside the remote root.
pub const REMOTE_WORKSPACE_ESCAPE_MARKER: &str = "__SYMPHONY_WORKSPACE_ESCAPE__";
/// Log truncation for hook output (`sanitize_hook_output_for_log/2`).
pub const HOOK_LOG_MAX_BYTES: usize = 2_048;
/// Hook output kept in error `Display` strings (the error value keeps the full output).
const ERROR_OUTPUT_MAX_BYTES: usize = 512;

/// Workspace and hook failures. `Display` keeps the Elixir reason tags.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// `{:workspace_path_unreadable, path, reason}` (`empty`, `invalid_characters`, `not_absolute`,
    /// `invalid` or a POSIX reason).
    #[error("workspace_path_unreadable: {path}: {reason}")]
    PathUnreadable {
        /// The offending path.
        path: String,
        /// Reason tag.
        reason: String,
    },
    /// `{:workspace_equals_root, workspace, root}`.
    #[error("workspace_equals_root: {workspace} (root {root})")]
    EqualsRoot {
        /// Canonical workspace.
        workspace: String,
        /// Canonical root.
        root: String,
    },
    /// `{:workspace_symlink_escape, expanded_workspace, canonical_root}`.
    #[error("workspace_symlink_escape: {workspace} (root {root})")]
    SymlinkEscape {
        /// Expanded (unresolved) workspace.
        workspace: String,
        /// Canonical root.
        root: String,
    },
    /// `{:workspace_outside_root, canonical_workspace, canonical_root}`.
    #[error("workspace_outside_root: {workspace} (root {root})")]
    OutsideRoot {
        /// Canonical workspace.
        workspace: String,
        /// Canonical root.
        root: String,
    },
    /// `{:path_canonicalize_failed, path, reason}` while computing the workspace path.
    #[error("{0}")]
    Path(PathError),
    /// `{:workspace_hook_failed, hook, status, output}`.
    #[error(
        "workspace_hook_failed: hook={hook} status={status} output={}",
        short(output)
    )]
    HookFailed {
        /// Hook name (`after_create`, ...).
        hook: String,
        /// Exit status.
        status: i32,
        /// Full merged output.
        output: String,
    },
    /// `{:workspace_hook_timeout, hook, timeout_ms}`. Remote timeouts report the hook name too
    /// (Elixir reported `"remote_command"`, G12); remote prepare/remove scripts use `remote_command`.
    #[error("workspace_hook_timeout: hook={hook} timeout_ms={timeout_ms}")]
    HookTimeout {
        /// Hook name, or `remote_command`.
        hook: String,
        /// The timeout.
        timeout_ms: u64,
    },
    /// A hook process could not be started.
    #[error("workspace_hook_spawn_failed: hook={hook}: {reason}")]
    HookSpawn {
        /// Hook name.
        hook: String,
        /// OS error text.
        reason: String,
    },
    /// `{:workspace_prepare_failed, host, status, output}`.
    #[error(
        "workspace_prepare_failed: worker_host={host} status={status} output={}",
        short(output)
    )]
    PrepareFailed {
        /// Worker host.
        host: String,
        /// Exit status.
        status: i32,
        /// Merged output.
        output: String,
    },
    /// `{:workspace_prepare_failed, :invalid_output, output}`.
    #[error("workspace_prepare_failed: invalid_output output={}", short(output))]
    PrepareInvalidOutput {
        /// Merged output.
        output: String,
    },
    /// `{:workspace_remove_failed, host, status, output}`.
    #[error(
        "workspace_remove_failed: worker_host={host} status={status} output={}",
        short(output)
    )]
    RemoveFailed {
        /// Worker host.
        host: String,
        /// Exit status.
        status: i32,
        /// Merged output.
        output: String,
    },
    /// ssh missing / not spawnable.
    #[error("{0}")]
    Ssh(SshError),
    /// A local file-system operation failed (Elixir raised `File.Error`).
    #[error("workspace_io_failed: {op} {path}: {reason}")]
    Io {
        /// Operation (`mkdir_p`, `rm_rf`, ...).
        op: &'static str,
        /// Path.
        path: String,
        /// POSIX reason.
        reason: IoReason,
    },
}

fn short(output: &str) -> String {
    format!(
        "{:?}",
        process::truncate_for_log(output.trim(), ERROR_OUTPUT_MAX_BYTES)
    )
}

/// The issue a hook runs for (log context only; hooks get no issue env vars, D10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueContext {
    /// Tracker id.
    pub issue_id: Option<String>,
    /// Human identifier (also the workspace key source).
    pub identifier: Option<String>,
}

impl IssueContext {
    /// Context for a bare identifier.
    pub fn identifier(identifier: impl Into<String>) -> Self {
        Self {
            issue_id: None,
            identifier: Some(identifier.into()),
        }
    }

    fn log(&self) -> String {
        format!(
            "issue_id={} issue_identifier={}",
            self.issue_id.as_deref().unwrap_or("n/a"),
            self.identifier.as_deref().unwrap_or("issue")
        )
    }
}

impl From<&Issue> for IssueContext {
    fn from(issue: &Issue) -> Self {
        Self {
            issue_id: issue.id.clone(),
            identifier: issue.identifier.clone(),
        }
    }
}

/// Lexical/canonical containment check (`validate_local_workspace_path/2`).
pub fn validate_local_workspace_path(workspace: &Path, root: &Path) -> Result<(), WorkspaceError> {
    let expanded = path_safety::expand_path(workspace, None);
    let expanded_root = path_safety::expand_path(root, None);
    let unreadable = |err: PathError| {
        let PathError::CanonicalizeFailed { path, reason } = err;
        WorkspaceError::PathUnreadable {
            path: lossy(&path),
            reason: reason.to_string(),
        }
    };
    let canonical = path_safety::canonicalize(&expanded).map_err(unreadable)?;
    let canonical_root = path_safety::canonicalize(&expanded_root).map_err(unreadable)?;
    let (ws, root) = (lossy(&canonical), lossy(&canonical_root));
    if ws == root {
        return Err(WorkspaceError::EqualsRoot {
            workspace: ws,
            root,
        });
    }
    if format!("{ws}/").starts_with(&format!("{root}/")) {
        return Ok(());
    }
    if format!("{}/", lossy(&expanded)).starts_with(&format!("{}/", lossy(&expanded_root))) {
        return Err(WorkspaceError::SymlinkEscape {
            workspace: lossy(&expanded),
            root,
        });
    }
    Err(WorkspaceError::OutsideRoot {
        workspace: ws,
        root,
    })
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn io_error(op: &'static str, path: &Path, err: &io::Error) -> WorkspaceError {
    WorkspaceError::Io {
        op,
        path: lossy(path),
        reason: IoReason::from_io(err),
    }
}

/// `File.rm_rf/1`: removes a file, symlink or directory tree; a missing path is fine.
async fn rm_rf(path: &Path) -> Result<(), WorkspaceError> {
    let meta = match tokio::fs::symlink_metadata(path).await {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(io_error("rm_rf", path, &err)),
    };
    let result = if meta.is_dir() {
        tokio::fs::remove_dir_all(path).await
    } else {
        tokio::fs::remove_file(path).await
    };
    match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(io_error("rm_rf", path, &err)),
    }
}

/// `remote_shell_assign/2`: assigns `raw` to `$variable`, expanding a leading `~` remotely.
fn remote_shell_assign(variable: &str, raw: &str) -> String {
    [
        format!("{variable}={}", shell_escape(raw)),
        format!("case \"${variable}\" in"),
        format!("  '~') {variable}=\"$HOME\" ;;"),
        format!("  '~/'*) {variable}=\"$HOME/${{{variable}#\\~/}}\" ;;"),
        "esac".to_owned(),
    ]
    .join("\n")
}

/// Normalized SSH worker hosts: trimmed, blanks dropped, deduplicated in order.
///
/// Elixir used the raw list for scheduling and a trimmed list in the runner, so messy entries could
/// make the per-host counts and the chosen host disagree; both now use this list.
pub fn worker_hosts(settings: &Settings) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for host in &settings.worker.ssh_hosts {
        let trimmed = host.trim();
        if !trimmed.is_empty() && !hosts.iter().any(|h| h == trimmed) {
            hosts.push(trimmed.to_owned());
        }
    }
    hosts
}

/// Workspace operations for one settings snapshot (cheap to build; build one per operation).
#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    settings: Arc<Settings>,
    workflow_path: PathBuf,
    ssh: SshConfig,
    hook_env: EnvPolicy,
    remote_cleanup_concurrency: usize,
}

impl WorkspaceManager {
    /// A manager for `settings` loaded from `workflow_path`. Hooks inherit the process environment
    /// until [`WorkspaceManager::with_hook_env`] is used.
    pub fn new(settings: Arc<Settings>, workflow_path: impl Into<PathBuf>, ssh: SshConfig) -> Self {
        Self {
            settings,
            workflow_path: workflow_path.into(),
            ssh,
            hook_env: EnvPolicy::inherit(),
            remote_cleanup_concurrency: 4,
        }
    }

    /// Environment for hook processes (and the local `ssh` process of remote hooks).
    pub fn with_hook_env(mut self, hook_env: EnvPolicy) -> Self {
        self.hook_env = hook_env;
        self
    }

    /// Maximum number of SSH hosts cleaned concurrently by [`WorkspaceManager::remove_issue_workspaces`].
    pub fn with_remote_cleanup_concurrency(mut self, concurrency: usize) -> Self {
        self.remote_cleanup_concurrency = concurrency.max(1);
        self
    }

    /// The settings snapshot.
    pub fn settings(&self) -> &Arc<Settings> {
        &self.settings
    }

    /// `Config.local_workspace_root/0`.
    pub fn local_root(&self) -> PathBuf {
        self.settings.local_workspace_root(&self.workflow_path)
    }

    fn hook_timeout(&self) -> Duration {
        Duration::from_millis(self.settings.hooks.timeout_ms)
    }

    /// `workspace_path_for_issue/2`: canonical `<local root>/<key>`, or raw `<root>/<key>` remotely.
    pub fn workspace_path(
        &self,
        identifier: Option<&str>,
        worker_host: Option<&str>,
    ) -> Result<String, WorkspaceError> {
        let key = workspace_key(identifier);
        match worker_host {
            None => path_safety::canonicalize(self.local_root().join(&key))
                .map(|p| lossy(&p))
                .map_err(WorkspaceError::Path),
            Some(_) => {
                let root = self.settings.workspace.root.trim_end_matches('/');
                Ok(format!("{root}/{key}"))
            }
        }
    }

    fn validate_remote_path(workspace: &str) -> Result<(), WorkspaceError> {
        if workspace.trim().is_empty() {
            return Err(WorkspaceError::PathUnreadable {
                path: workspace.to_owned(),
                reason: "empty".into(),
            });
        }
        if workspace.contains(['\n', '\r', '\0']) {
            return Err(WorkspaceError::PathUnreadable {
                path: workspace.to_owned(),
                reason: "invalid_characters".into(),
            });
        }
        // `workspace_key` keeps `.` and `..` verbatim; remotely nothing canonicalizes the path locally.
        if workspace.ends_with("/.") || workspace.ends_with("/..") || workspace.contains("/../") {
            return Err(WorkspaceError::OutsideRoot {
                workspace: workspace.to_owned(),
                root: String::new(),
            });
        }
        Ok(())
    }

    /// `create_for_issue/2`: compute and validate the path, create or reuse the directory, and run
    /// `after_create` when it was newly created (a failed `after_create` removes the new directory, so
    /// the next attempt runs it again). Returns the workspace path.
    pub async fn create_for_issue(
        &self,
        issue: &IssueContext,
        worker_host: Option<&str>,
    ) -> Result<String, WorkspaceError> {
        let result = self.create_inner(issue, worker_host).await;
        if let Err(err) = &result
            && matches!(err, WorkspaceError::Io { .. })
        {
            tracing::error!(
                "Workspace creation failed {} worker_host={} error={err}",
                issue.log(),
                worker_host.unwrap_or("local")
            );
        }
        result
    }

    async fn create_inner(
        &self,
        issue: &IssueContext,
        worker_host: Option<&str>,
    ) -> Result<String, WorkspaceError> {
        let workspace = self.workspace_path(issue.identifier.as_deref(), worker_host)?;
        let (workspace, created) = match worker_host {
            None => {
                validate_local_workspace_path(Path::new(&workspace), &self.local_root())?;
                let created = self.ensure_local(Path::new(&workspace)).await?;
                (workspace, created)
            }
            Some(host) => {
                Self::validate_remote_path(&workspace)?;
                self.ensure_remote(&workspace, host).await?
            }
        };
        if created
            && let Some(command) = self.settings.hooks.after_create.clone()
            && let Err(err) = self
                .run_hook(&command, &workspace, issue, "after_create", worker_host)
                .await
        {
            self.cleanup_failed_new_workspace(&workspace, worker_host)
                .await;
            return Err(err);
        }
        Ok(workspace)
    }

    async fn ensure_local(&self, workspace: &Path) -> Result<bool, WorkspaceError> {
        match tokio::fs::metadata(workspace).await {
            Ok(meta) if meta.is_dir() => return Ok(false),
            _ => {}
        }
        rm_rf(workspace).await?;
        tokio::fs::create_dir_all(workspace)
            .await
            .map_err(|err| io_error("mkdir_p", workspace, &err))?;
        Ok(true)
    }

    async fn ensure_remote(
        &self,
        workspace: &str,
        host: &str,
    ) -> Result<(String, bool), WorkspaceError> {
        let root = self.settings.workspace.root.as_str();
        let script = [
            "set -eu".to_owned(),
            remote_shell_assign("workspace", workspace),
            remote_shell_assign("root", root),
            "if [ -d \"$workspace\" ]; then".to_owned(),
            "  created=0".to_owned(),
            "elif [ -e \"$workspace\" ]; then".to_owned(),
            "  rm -rf \"$workspace\"".to_owned(),
            "  mkdir -p \"$workspace\"".to_owned(),
            "  created=1".to_owned(),
            "else".to_owned(),
            "  mkdir -p \"$workspace\"".to_owned(),
            "  created=1".to_owned(),
            "fi".to_owned(),
            "cd \"$workspace\"".to_owned(),
            "workspace_real=\"$(pwd -P)\"".to_owned(),
            "root_real=\"$(cd \"$root\" && pwd -P)\"".to_owned(),
            "case \"$workspace_real\" in".to_owned(),
            "  \"$root_real\") escaped=1 ;;".to_owned(),
            "  \"$root_real\"/*) escaped=0 ;;".to_owned(),
            "  *) escaped=1 ;;".to_owned(),
            "esac".to_owned(),
            "if [ \"$escaped\" = 1 ]; then".to_owned(),
            format!(
                "  printf '%s\\t%s\\t%s\\n' '{REMOTE_WORKSPACE_ESCAPE_MARKER}' \"$workspace_real\" \"$root_real\""
            ),
            "  exit 3".to_owned(),
            "fi".to_owned(),
            format!(
                "printf '%s\\t%s\\t%s\\n' '{REMOTE_WORKSPACE_MARKER}' \"$created\" \"$workspace_real\""
            ),
        ]
        .join("\n");
        let out = self.run_remote(host, &script, "remote_command").await?;
        if out.status == 0 {
            return parse_remote_workspace_output(&out.output);
        }
        if let Some((workspace, root)) = parse_escape_output(&out.output) {
            return Err(WorkspaceError::OutsideRoot { workspace, root });
        }
        Err(WorkspaceError::PrepareFailed {
            host: host.to_owned(),
            status: out.status,
            output: out.output,
        })
    }

    async fn cleanup_failed_new_workspace(&self, workspace: &str, worker_host: Option<&str>) {
        match worker_host {
            None => {
                if let Err(err) = rm_rf(Path::new(workspace)).await {
                    tracing::warn!(
                        "Failed to remove partial workspace path={workspace} reason={err}"
                    );
                }
            }
            Some(host) => {
                let script = [
                    remote_shell_assign("workspace", workspace),
                    "rm -rf \"$workspace\"".to_owned(),
                ]
                .join("\n");
                match self.run_remote(host, &script, "remote_command").await {
                    Ok(out) if out.status == 0 => {}
                    result => tracing::warn!(
                        "Failed to remove partial workspace worker_host={host} result={result:?}"
                    ),
                }
            }
        }
    }

    /// `run_before_run_hook/3`: fatal on failure.
    pub async fn run_before_run_hook(
        &self,
        workspace: &str,
        issue: &IssueContext,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        match self.settings.hooks.before_run.clone() {
            None => Ok(()),
            Some(command) => {
                self.run_hook(&command, workspace, issue, "before_run", worker_host)
                    .await
            }
        }
    }

    /// `run_after_run_hook/3`: failures are logged and ignored.
    pub async fn run_after_run_hook(
        &self,
        workspace: &str,
        issue: &IssueContext,
        worker_host: Option<&str>,
    ) {
        self.run_after_run_hook_within(workspace, issue, worker_host, self.hook_timeout())
            .await;
    }

    /// `after_run` bounded by `min(hooks.timeout_ms, limit)` (used when a worker is being cancelled).
    pub async fn run_after_run_hook_within(
        &self,
        workspace: &str,
        issue: &IssueContext,
        worker_host: Option<&str>,
        limit: Duration,
    ) {
        if let Some(command) = self.settings.hooks.after_run.clone() {
            let limit = limit.min(self.hook_timeout());
            let _ = self
                .run_hook_with_timeout(&command, workspace, issue, "after_run", worker_host, limit)
                .await;
        }
    }

    async fn run_hook(
        &self,
        command: &str,
        workspace: &str,
        issue: &IssueContext,
        hook: &str,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        self.run_hook_with_timeout(
            command,
            workspace,
            issue,
            hook,
            worker_host,
            self.hook_timeout(),
        )
        .await
    }

    async fn run_hook_with_timeout(
        &self,
        command: &str,
        workspace: &str,
        issue: &IssueContext,
        hook: &str,
        worker_host: Option<&str>,
        limit: Duration,
    ) -> Result<(), WorkspaceError> {
        let timeout_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            "Running workspace hook hook={hook} {} workspace={workspace} worker_host={}",
            issue.log(),
            worker_host.unwrap_or("local")
        );
        let result = match worker_host {
            None => {
                let mut cmd = Command::new("sh");
                cmd.arg("-lc").arg(command).current_dir(workspace);
                self.hook_env.apply(&mut cmd);
                process::run_captured(cmd, limit).await.map_err(|err| match err {
                    ProcessError::Timeout => {
                        tracing::warn!(
                            "Workspace hook timed out hook={hook} {} workspace={workspace} worker_host=local timeout_ms={timeout_ms}",
                            issue.log()
                        );
                        WorkspaceError::HookTimeout {
                            hook: hook.to_owned(),
                            timeout_ms,
                        }
                    }
                    ProcessError::Spawn(reason) => WorkspaceError::HookSpawn {
                        hook: hook.to_owned(),
                        reason,
                    },
                })
            }
            Some(host) => {
                let script = format!("cd {} && {command}", shell_escape(workspace));
                self.run_remote_within(host, &script, hook, limit).await
            }
        }?;
        check_hook_result(&result, workspace, issue, hook)
    }

    async fn run_remote(
        &self,
        host: &str,
        script: &str,
        label: &str,
    ) -> Result<CommandOutput, WorkspaceError> {
        self.run_remote_within(host, script, label, self.hook_timeout())
            .await
    }

    async fn run_remote_within(
        &self,
        host: &str,
        script: &str,
        label: &str,
        limit: Duration,
    ) -> Result<CommandOutput, WorkspaceError> {
        self.ssh
            .run(host, script, limit, &self.hook_env)
            .await
            .map_err(|err| match err {
                SshRunError::Ssh(err) => WorkspaceError::Ssh(err),
                SshRunError::Timeout => {
                    let timeout_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
                    tracing::warn!(
                        "Workspace hook timed out hook={label} worker_host={host} timeout_ms={timeout_ms}"
                    );
                    WorkspaceError::HookTimeout {
                        hook: label.to_owned(),
                        timeout_ms,
                    }
                }
            })
    }

    async fn run_before_remove_hook(&self, workspace: &str, worker_host: Option<&str>) {
        let Some(command) = self.settings.hooks.before_remove.clone() else {
            return;
        };
        let context = IssueContext::identifier(
            Path::new(workspace)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        let result = match worker_host {
            None => {
                if !tokio::fs::metadata(workspace)
                    .await
                    .is_ok_and(|meta| meta.is_dir())
                {
                    return;
                }
                self.run_hook(&command, workspace, &context, "before_remove", None)
                    .await
            }
            Some(host) => {
                let script = [
                    remote_shell_assign("workspace", workspace),
                    "if [ -d \"$workspace\" ]; then".to_owned(),
                    "  cd \"$workspace\"".to_owned(),
                    format!("  {command}"),
                    "fi".to_owned(),
                ]
                .join("\n");
                match self.run_remote(host, &script, "before_remove").await {
                    Ok(out) => check_hook_result(&out, workspace, &context, "before_remove"),
                    Err(err) => Err(err),
                }
            }
        };
        // `ignore_hook_failure/1`
        let _ = result;
    }

    /// `remove/2`: removes a workspace (running `before_remove` first).
    ///
    /// Local: an existing path must validate against the local root (so the root itself is refused);
    /// a missing path is a no-op. Remote: `before_remove` (ignored on failure), then `rm -rf`.
    pub async fn remove(
        &self,
        workspace: &str,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        match worker_host {
            None => {
                if tokio::fs::symlink_metadata(workspace).await.is_err() {
                    return Ok(());
                }
                validate_local_workspace_path(Path::new(workspace), &self.local_root())?;
                self.remove_local(workspace).await
            }
            Some(host) => self.remove_remote(workspace, host).await,
        }
    }

    /// `remove_recorded/2`: removes a workspace path recorded by a run, even if `workspace.root` has
    /// changed since. Local paths must be absolute and are validated against their parent directory
    /// (guards against the leaf being an escaping symlink) **before** any hook runs.
    pub async fn remove_recorded(
        &self,
        workspace: &str,
        worker_host: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        match worker_host {
            Some(host) => self.remove_remote(workspace, host).await,
            None => {
                let path = Path::new(workspace);
                if !path.is_absolute() {
                    return Err(WorkspaceError::PathUnreadable {
                        path: workspace.to_owned(),
                        reason: "not_absolute".into(),
                    });
                }
                let parent = path.parent().unwrap_or(Path::new("/"));
                validate_local_workspace_path(path, parent)?;
                self.remove_local(workspace).await
            }
        }
    }

    async fn remove_local(&self, workspace: &str) -> Result<(), WorkspaceError> {
        self.run_before_remove_hook(workspace, None).await;
        rm_rf(Path::new(workspace)).await
    }

    async fn remove_remote(&self, workspace: &str, host: &str) -> Result<(), WorkspaceError> {
        self.run_before_remove_hook(workspace, Some(host)).await;
        let script = [
            remote_shell_assign("workspace", workspace),
            "rm -rf \"$workspace\"".to_owned(),
        ]
        .join("\n");
        let out = self.run_remote(host, &script, "remote_command").await?;
        if out.status == 0 {
            Ok(())
        } else {
            Err(WorkspaceError::RemoveFailed {
                host: host.to_owned(),
                status: out.status,
                output: out.output,
            })
        }
    }

    /// `remove_issue_workspaces/2`: removes `<root>/<key>` for `identifier` on `worker_host`; with no
    /// host it removes locally, or on **every** configured SSH host (concurrently, bounded — Elixir
    /// went host by host). Failures are logged and ignored. A missing identifier does nothing.
    pub async fn remove_issue_workspaces(
        &self,
        identifier: Option<&str>,
        worker_host: Option<&str>,
    ) {
        let Some(identifier) = identifier else {
            return;
        };
        let hosts: Vec<Option<String>> = match worker_host {
            Some(host) => vec![Some(host.to_owned())],
            None => {
                let hosts = worker_hosts(&self.settings);
                if hosts.is_empty() {
                    vec![None]
                } else {
                    hosts.into_iter().map(Some).collect()
                }
            }
        };
        futures::stream::iter(hosts)
            .map(|host| async move {
                let host = host.as_deref();
                match self.workspace_path(Some(identifier), host) {
                    Ok(workspace) => {
                        if let Err(err) = self.remove(&workspace, host).await {
                            tracing::warn!(
                                "Failed to remove workspace issue_identifier={identifier} workspace={workspace} worker_host={} reason={err}",
                                host.unwrap_or("local")
                            );
                        }
                    }
                    Err(err) => tracing::debug!(
                        "Skipping workspace removal issue_identifier={identifier}: {err}"
                    ),
                }
            })
            .buffer_unordered(self.remote_cleanup_concurrency)
            .collect::<Vec<()>>()
            .await;
    }
}

/// `handle_hook_command_result/4`.
fn check_hook_result(
    out: &CommandOutput,
    workspace: &str,
    issue: &IssueContext,
    hook: &str,
) -> Result<(), WorkspaceError> {
    if out.status == 0 {
        return Ok(());
    }
    tracing::warn!(
        "Workspace hook failed hook={hook} {} workspace={workspace} status={} output={:?}",
        issue.log(),
        out.status,
        process::truncate_for_log(&out.output, HOOK_LOG_MAX_BYTES)
    );
    Err(WorkspaceError::HookFailed {
        hook: hook.to_owned(),
        status: out.status,
        output: out.output.clone(),
    })
}

/// `parse_remote_workspace_output/1`: the first `__SYMPHONY_WORKSPACE__\t0|1\t<path>` line.
pub fn parse_remote_workspace_output(output: &str) -> Result<(String, bool), WorkspaceError> {
    output
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| {
            let mut parts = line.splitn(3, '\t');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(REMOTE_WORKSPACE_MARKER), Some(created @ ("0" | "1")), Some(path))
                    if !path.is_empty() =>
                {
                    Some((path.to_owned(), created == "1"))
                }
                _ => None,
            }
        })
        .ok_or_else(|| WorkspaceError::PrepareInvalidOutput {
            output: output.to_owned(),
        })
}

fn parse_escape_output(output: &str) -> Option<(String, String)> {
    output.lines().find_map(|line| {
        let mut parts = line.trim().splitn(3, '\t');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(REMOTE_WORKSPACE_ESCAPE_MARKER), Some(ws), Some(root)) => {
                Some((ws.to_owned(), root.to_owned()))
            }
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_output_parsing_matches_elixir() {
        assert_eq!(
            parse_remote_workspace_output("noise\n__SYMPHONY_WORKSPACE__\t1\t/remote/ws\n")
                .unwrap(),
            ("/remote/ws".into(), true)
        );
        assert_eq!(
            parse_remote_workspace_output("__SYMPHONY_WORKSPACE__\t0\t/a\tb").unwrap(),
            ("/a\tb".into(), false)
        );
        assert!(matches!(
            parse_remote_workspace_output("__SYMPHONY_WORKSPACE__\t2\t/x"),
            Err(WorkspaceError::PrepareInvalidOutput { .. })
        ));
        assert!(parse_remote_workspace_output("__SYMPHONY_WORKSPACE__\t1\t").is_err());
        assert_eq!(
            parse_escape_output("__SYMPHONY_WORKSPACE_ESCAPE__\t/etc\t/home/u/ws\n"),
            Some(("/etc".into(), "/home/u/ws".into()))
        );
    }

    #[test]
    fn remote_shell_assign_expands_home_remotely() {
        assert_eq!(
            remote_shell_assign("workspace", "~/w's"),
            "workspace='~/w'\"'\"'s'\ncase \"$workspace\" in\n  '~') workspace=\"$HOME\" ;;\n  '~/'*) workspace=\"$HOME/${workspace#\\~/}\" ;;\nesac"
        );
    }

    #[test]
    fn remote_paths_reject_blank_control_and_dot_segments() {
        assert!(WorkspaceManager::validate_remote_path("~/ws/MT-1").is_ok());
        assert!(matches!(
            WorkspaceManager::validate_remote_path(" "),
            Err(WorkspaceError::PathUnreadable { reason, .. }) if reason == "empty"
        ));
        assert!(matches!(
            WorkspaceManager::validate_remote_path("/a\nb"),
            Err(WorkspaceError::PathUnreadable { reason, .. }) if reason == "invalid_characters"
        ));
        assert!(WorkspaceManager::validate_remote_path("~/ws/..").is_err());
        assert!(WorkspaceManager::validate_remote_path("~/ws/.").is_err());
    }

    #[test]
    fn worker_hosts_are_trimmed_and_deduplicated() {
        let mut settings = Settings::default();
        settings.worker.ssh_hosts = vec![
            " worker-a".into(),
            "worker-a".into(),
            "".into(),
            "worker-b ".into(),
        ];
        assert_eq!(worker_hosts(&settings), vec!["worker-a", "worker-b"]);
    }
}
