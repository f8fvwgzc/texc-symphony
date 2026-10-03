//! Typed Codex client errors with stable snake_case tags (the Elixir reason atoms).
//!
//! `Display` renders `"<tag>"` or `"<tag>: <detail>"`; [`CodexError::tag`] returns the bare tag for
//! logs, metrics and API payloads. Payload-carrying variants render their JSON compactly.

use std::fmt;
use std::path::PathBuf;

use serde::{Serialize, Serializer};
use serde_json::Value;
use symphony_core::{IoReason, PathError};

/// Why a workspace path was refused as the Codex cwd (`{:invalid_workspace_cwd, kind, ...}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidWorkspaceCwd {
    /// `{:invalid_workspace_cwd, :workspace_root, canonical_path}`: the workspace is the root itself.
    WorkspaceRoot(PathBuf),
    /// `{:invalid_workspace_cwd, :symlink_escape, expanded_path, canonical_root}`: lexically under the root
    /// but resolving (through a symlink) outside of it. The first path is the expanded, unresolved path.
    SymlinkEscape(PathBuf, PathBuf),
    /// `{:invalid_workspace_cwd, :outside_workspace_root, canonical_path, canonical_root}`.
    OutsideWorkspaceRoot(PathBuf, PathBuf),
    /// `{:invalid_workspace_cwd, :path_unreadable, path, posix_reason}`: canonicalization failed.
    PathUnreadable(PathBuf, IoReason),
    /// `{:invalid_workspace_cwd, :empty_remote_workspace, worker_host}`.
    EmptyRemoteWorkspace(String),
    /// `{:invalid_workspace_cwd, :invalid_remote_workspace, worker_host, workspace}`: contains `\n`, `\r`
    /// or NUL.
    InvalidRemoteWorkspace(String, String),
}

impl InvalidWorkspaceCwd {
    /// The second-level tag (`workspace_root`, `symlink_escape`, ...).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::WorkspaceRoot(_) => "workspace_root",
            Self::SymlinkEscape(..) => "symlink_escape",
            Self::OutsideWorkspaceRoot(..) => "outside_workspace_root",
            Self::PathUnreadable(..) => "path_unreadable",
            Self::EmptyRemoteWorkspace(_) => "empty_remote_workspace",
            Self::InvalidRemoteWorkspace(..) => "invalid_remote_workspace",
        }
    }
}

impl fmt::Display for InvalidWorkspaceCwd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = self.kind();
        match self {
            Self::WorkspaceRoot(path) => write!(f, "{kind}: {}", path.display()),
            Self::SymlinkEscape(path, root) | Self::OutsideWorkspaceRoot(path, root) => {
                write!(f, "{kind}: {} (root {})", path.display(), root.display())
            }
            Self::PathUnreadable(path, reason) => {
                write!(f, "{kind}: {}: {reason}", path.display())
            }
            Self::EmptyRemoteWorkspace(host) => write!(f, "{kind}: worker_host={host}"),
            Self::InvalidRemoteWorkspace(host, workspace) => {
                write!(f, "{kind}: worker_host={host} workspace={workspace:?}")
            }
        }
    }
}

/// Why a turn stopped and needs a human: the issue should be *blocked*, not retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    /// Codex asked for operator input (`turn/*input*`, freeform tool prompts, MCP elicitations).
    InputRequired,
    /// Codex asked for an approval that the policy does not auto-grant.
    ApprovalRequired,
}

impl Blocker {
    /// The orchestrator's `blocker_error` text for this blocker.
    pub fn message(self) -> &'static str {
        match self {
            Self::InputRequired => "codex turn requires operator input",
            Self::ApprovalRequired => "codex turn requires approval",
        }
    }
}

/// Errors from starting a session or running a turn (`AppServer` `{:error, reason}` terms).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CodexError {
    /// The workspace cannot be used as the Codex cwd.
    #[error("invalid_workspace_cwd: {0}")]
    InvalidWorkspaceCwd(InvalidWorkspaceCwd),
    /// `bash` is not on `PATH` (local launch).
    #[error("bash_not_found")]
    BashNotFound,
    /// `ssh` is not on `PATH` (remote launch; returned by the runtime's launcher).
    #[error("ssh_not_found")]
    SshNotFound,
    /// A remote `worker_host` was requested but no [`crate::RemoteLauncher`] was supplied.
    #[error("remote_launcher_missing: worker_host={0}")]
    RemoteLauncherMissing(String),
    /// The OS refused to spawn the app-server process (Elixir crashed in `Port.open`).
    #[error("port_spawn_failed: {0}")]
    SpawnFailed(String),
    /// The default turn sandbox policy root could not be canonicalized.
    #[error("{0}")]
    SandboxPolicy(PathError),
    /// No response line for `read_timeout_ms` while awaiting a request's response.
    #[error("response_timeout")]
    ResponseTimeout,
    /// The response carried `error` (the error value) or neither `result` nor `error` (the whole message).
    #[error("response_error: {0}")]
    ResponseError(Value),
    /// The app-server process exited (any status, including 0; signal deaths are `128 + signal`).
    #[error("port_exit: {0}")]
    PortExit(i32),
    /// `thread/start` result without a usable `thread.id`.
    #[error("invalid_thread_payload: {0}")]
    InvalidThreadPayload(Value),
    /// `turn/start` result without a usable `turn.id` (Elixir leaked the raw result; see C.13 #5).
    #[error("invalid_turn_payload: {0}")]
    InvalidTurnPayload(Value),
    /// No stream line for `turn_timeout_ms` during a turn (a silence timeout).
    #[error("turn_timeout")]
    TurnTimeout,
    /// `turn/failed` with `params` (the params).
    #[error("turn_failed: {0}")]
    TurnFailed(Value),
    /// `turn/cancelled` with `params` (the params).
    #[error("turn_cancelled: {0}")]
    TurnCancelled(Value),
    /// Codex requested operator input (the whole message).
    #[error("turn_input_required: {0}")]
    TurnInputRequired(Value),
    /// Codex requested an approval that the policy does not auto-grant (the whole message).
    #[error("approval_required: {0}")]
    ApprovalRequired(Value),
}

impl CodexError {
    /// The stable snake_case reason tag (`port_exit`, `turn_input_required`, ...).
    pub fn tag(&self) -> &'static str {
        match self {
            Self::InvalidWorkspaceCwd(_) => "invalid_workspace_cwd",
            Self::BashNotFound => "bash_not_found",
            Self::SshNotFound => "ssh_not_found",
            Self::RemoteLauncherMissing(_) => "remote_launcher_missing",
            Self::SpawnFailed(_) => "port_spawn_failed",
            Self::SandboxPolicy(_) => "path_canonicalize_failed",
            Self::ResponseTimeout => "response_timeout",
            Self::ResponseError(_) => "response_error",
            Self::PortExit(_) => "port_exit",
            Self::InvalidThreadPayload(_) => "invalid_thread_payload",
            Self::InvalidTurnPayload(_) => "invalid_turn_payload",
            Self::TurnTimeout => "turn_timeout",
            Self::TurnFailed(_) => "turn_failed",
            Self::TurnCancelled(_) => "turn_cancelled",
            Self::TurnInputRequired(_) => "turn_input_required",
            Self::ApprovalRequired(_) => "approval_required",
        }
    }

    /// `Some` when the turn ended because Codex needs a human (the issue should be blocked, not retried).
    pub fn blocker(&self) -> Option<Blocker> {
        match self {
            Self::TurnInputRequired(_) => Some(Blocker::InputRequired),
            Self::ApprovalRequired(_) => Some(Blocker::ApprovalRequired),
            _ => None,
        }
    }

    /// The Codex message/params carried by the error, if any.
    pub fn payload(&self) -> Option<&Value> {
        match self {
            Self::ResponseError(v)
            | Self::InvalidThreadPayload(v)
            | Self::InvalidTurnPayload(v)
            | Self::TurnFailed(v)
            | Self::TurnCancelled(v)
            | Self::TurnInputRequired(v)
            | Self::ApprovalRequired(v) => Some(v),
            _ => None,
        }
    }
}

/// Serialized as its `Display` string (the tag plus detail).
impl Serialize for CodexError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
