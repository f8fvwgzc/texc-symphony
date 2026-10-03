//! Typed errors for configuration and workflow loading.
//!
//! `Display` keeps the Elixir snake_case reason tags (`missing_linear_api_token`,
//! `invalid_workflow_config: ...`) because log lines and API payloads embed them.
//! [`ConfigError::user_message`] reproduces Elixir's `Config.format_config_error/1` strings.

use std::fmt;
use std::io;
use std::path::PathBuf;

/// POSIX-style reason for an I/O failure (`enoent`, `eacces`, ...), mirroring the Elixir atoms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoReason {
    kind: io::ErrorKind,
    name: &'static str,
}

impl IoReason {
    /// Builds the reason from an `io::Error`, preferring the raw errno when available.
    pub fn from_io(err: &io::Error) -> Self {
        let name = err
            .raw_os_error()
            .and_then(errno_name)
            .unwrap_or_else(|| kind_name(err.kind()));
        Self {
            kind: err.kind(),
            name,
        }
    }

    /// Builds a reason with an explicit tag (used for synthetic errors such as the symlink-loop guard).
    pub fn named(kind: io::ErrorKind, name: &'static str) -> Self {
        Self { kind, name }
    }

    /// The `std::io::ErrorKind` of the failure.
    pub fn kind(&self) -> io::ErrorKind {
        self.kind
    }

    /// The lowercase POSIX tag, e.g. `"enoent"`.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

impl fmt::Display for IoReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

#[cfg(unix)]
fn errno_name(code: i32) -> Option<&'static str> {
    let name = match code {
        libc::ENOENT => "enoent",
        libc::EACCES => "eacces",
        libc::EPERM => "eperm",
        libc::ENOTDIR => "enotdir",
        libc::EISDIR => "eisdir",
        libc::ENAMETOOLONG => "enametoolong",
        libc::ELOOP => "eloop",
        libc::EEXIST => "eexist",
        libc::ENOSPC => "enospc",
        libc::EROFS => "erofs",
        libc::EBUSY => "ebusy",
        libc::EINVAL => "einval",
        libc::EIO => "eio",
        libc::EMFILE => "emfile",
        libc::ENFILE => "enfile",
        libc::ENOTEMPTY => "enotempty",
        libc::EXDEV => "exdev",
        libc::ENOMEM => "enomem",
        libc::EAGAIN => "eagain",
        _ => return None,
    };
    Some(name)
}

#[cfg(not(unix))]
fn errno_name(_code: i32) -> Option<&'static str> {
    None
}

fn kind_name(kind: io::ErrorKind) -> &'static str {
    match kind {
        io::ErrorKind::NotFound => "enoent",
        io::ErrorKind::PermissionDenied => "eacces",
        io::ErrorKind::AlreadyExists => "eexist",
        io::ErrorKind::IsADirectory => "eisdir",
        io::ErrorKind::NotADirectory => "enotdir",
        io::ErrorKind::InvalidFilename => "enametoolong",
        io::ErrorKind::InvalidInput => "einval",
        io::ErrorKind::InvalidData => "einval",
        _ => "eio",
    }
}

/// Adapter-level tracker configuration failures (Elixir `validate_config/1` atoms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[allow(missing_docs)] // each variant is documented by its stable tag
pub enum TrackerConfigError {
    #[error("invalid_linear_endpoint")]
    InvalidLinearEndpoint,
    #[error("missing_linear_api_token")]
    MissingLinearApiToken,
    #[error("missing_linear_project_slug")]
    MissingLinearProjectSlug,
    #[error("invalid_linear_assignee")]
    InvalidLinearAssignee,
    #[error("missing_github_active_states")]
    MissingGithubActiveStates,
    #[error("missing_github_terminal_states")]
    MissingGithubTerminalStates,
    #[error("invalid_github_states")]
    InvalidGithubStates,
    #[error("invalid_github_api_url")]
    InvalidGithubApiUrl,
    #[error("missing_github_repo")]
    MissingGithubRepo,
    #[error("invalid_github_repo")]
    InvalidGithubRepo,
    #[error("missing_github_token")]
    MissingGithubToken,
    #[error("missing_gitlab_active_states")]
    MissingGitlabActiveStates,
    #[error("missing_gitlab_terminal_states")]
    MissingGitlabTerminalStates,
    #[error("invalid_gitlab_states")]
    InvalidGitlabStates,
    #[error("invalid_gitlab_api_url")]
    InvalidGitlabApiUrl,
    #[error("missing_gitlab_project_path")]
    MissingGitlabProjectPath,
    #[error("invalid_gitlab_project_path")]
    InvalidGitlabProjectPath,
    #[error("missing_gitlab_api_key")]
    MissingGitlabApiKey,
    #[error("missing_jira_active_states")]
    MissingJiraActiveStates,
    #[error("missing_jira_terminal_states")]
    MissingJiraTerminalStates,
    #[error("invalid_jira_states")]
    InvalidJiraStates,
    #[error("invalid_jira_base_url")]
    InvalidJiraBaseUrl,
    #[error("missing_jira_email")]
    MissingJiraEmail,
    #[error("missing_jira_api_token")]
    MissingJiraApiToken,
    #[error("missing_jira_project_key")]
    MissingJiraProjectKey,
    #[error("missing_asana_active_states")]
    MissingAsanaActiveStates,
    #[error("missing_asana_terminal_states")]
    MissingAsanaTerminalStates,
    #[error("invalid_asana_states")]
    InvalidAsanaStates,
    #[error("invalid_asana_endpoint")]
    InvalidAsanaEndpoint,
    #[error("missing_asana_api_key")]
    MissingAsanaApiKey,
    #[error("missing_asana_project_gid")]
    MissingAsanaProjectGid,
}

impl TrackerConfigError {
    /// The stable snake_case tag (identical to `Display`).
    pub fn tag(&self) -> String {
        self.to_string()
    }
}

/// Workflow loading, config casting and preflight validation failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Front matter failed schema casting/validation; the message lists `"dotted.path message"` errors
    /// joined with `", "`.
    #[error("invalid_workflow_config: {0}")]
    InvalidWorkflowConfig(String),
    /// `WORKFLOW.md` could not be read.
    #[error("missing_workflow_file: {}: {reason}", path.display())]
    MissingWorkflowFile {
        /// The path that was read.
        path: PathBuf,
        /// POSIX reason (`enoent`, `eisdir`, ...).
        reason: IoReason,
    },
    /// The YAML front matter did not parse.
    #[error("workflow_parse_error: {0}")]
    WorkflowParseError(String),
    /// The front matter decoded to something other than a map.
    #[error("workflow_front_matter_not_a_map")]
    WorkflowFrontMatterNotAMap,
    /// The workflow file could not be stat'ed/read while checking for changes (Elixir returns the bare
    /// posix atom here, e.g. `:enoent`).
    #[error("workflow_file_unreadable: {}: {reason}", path.display())]
    WorkflowFileUnreadable {
        /// The path that was checked.
        path: PathBuf,
        /// POSIX reason.
        reason: IoReason,
    },
    /// `tracker.kind` is absent.
    #[error("missing_tracker_kind")]
    MissingTrackerKind,
    /// `tracker.kind` is not one of the supported adapters.
    #[error("unsupported_tracker_kind: {0:?}")]
    UnsupportedTrackerKind(String),
    /// The selected adapter rejected its settings.
    #[error(transparent)]
    Tracker(#[from] TrackerConfigError),
}

impl ConfigError {
    /// Stable snake_case tag of the error class.
    pub fn tag(&self) -> String {
        match self {
            Self::InvalidWorkflowConfig(_) => "invalid_workflow_config".into(),
            Self::MissingWorkflowFile { .. } => "missing_workflow_file".into(),
            Self::WorkflowParseError(_) => "workflow_parse_error".into(),
            Self::WorkflowFrontMatterNotAMap => "workflow_front_matter_not_a_map".into(),
            Self::WorkflowFileUnreadable { reason, .. } => reason.name().into(),
            Self::MissingTrackerKind => "missing_tracker_kind".into(),
            Self::UnsupportedTrackerKind(_) => "unsupported_tracker_kind".into(),
            Self::Tracker(err) => err.tag(),
        }
    }

    /// Operator-facing message, byte-compatible with Elixir's `Config.format_config_error/1` where the
    /// reason has a stable textual form.
    pub fn user_message(&self) -> String {
        match self {
            Self::InvalidWorkflowConfig(message) => {
                format!("Invalid WORKFLOW.md config: {message}")
            }
            Self::MissingWorkflowFile { path, reason } => {
                format!("Missing WORKFLOW.md at {}: :{reason}", path.display())
            }
            Self::WorkflowParseError(reason) => format!("Failed to parse WORKFLOW.md: {reason}"),
            Self::WorkflowFrontMatterNotAMap => {
                "Failed to parse WORKFLOW.md: workflow front matter must decode to a map".into()
            }
            Self::WorkflowFileUnreadable { reason, .. } => {
                format!("Invalid WORKFLOW.md config: :{reason}")
            }
            Self::MissingTrackerKind => "Invalid WORKFLOW.md config: :missing_tracker_kind".into(),
            Self::UnsupportedTrackerKind(kind) => {
                format!("Invalid WORKFLOW.md config: {{:unsupported_tracker_kind, {kind:?}}}")
            }
            Self::Tracker(err) => format!("Invalid WORKFLOW.md config: :{err}"),
        }
    }
}
