//! Launch: workspace cwd validation and command construction (C.1).
//!
//! - Local: `bash -lc "[unset S1 S2 && ]exec <codex.command>"` with `cwd` = the canonical workspace and the
//!   secret env vars removed from the child env (the in-shell `unset` also strips values the login
//!   profile re-exports).
//! - Remote: the runtime supplies a [`RemoteLauncher`] (SSH) that turns
//!   `cd '<ws>' && [unset ... && ]exec <codex.command>` into a local process.

use std::env;
use std::path::{Path, PathBuf};

use symphony_core::config::CodexSettings;
use symphony_core::env::{uniq, valid_env_name};
use symphony_core::path_safety::{self, PathError};

use crate::error::{CodexError, InvalidWorkspaceCwd};

/// Builds the local process that runs a remote command on a worker host (the SSH hook point).
///
/// The session configures stdio (piped), `process_group(0)`, `kill_on_drop(true)`, extra env and the
/// removal of secret env vars on the returned command, so implementations only set program and args.
pub trait RemoteLauncher: Send + Sync {
    /// The command that runs `remote_command` (shell code; run it with `bash -lc`) on `worker_host`
    /// (`host` or `host:port`). Return [`CodexError::SshNotFound`] when `ssh` is missing.
    fn command(
        &self,
        worker_host: &str,
        remote_command: &str,
    ) -> Result<tokio::process::Command, CodexError>;
}

/// `shell_escape/1`: single-quote `value`, escaping embedded quotes as `'"'"'`.
pub fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Keeps names matching `^[A-Za-z_][A-Za-z0-9_]*$` (others are silently dropped), deduplicated.
pub fn valid_secret_names<I, S>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    uniq(
        names
            .into_iter()
            .map(Into::into)
            .filter(|name| valid_env_name(name)),
    )
}

/// A TOML basic string, as Codex parses `--config key=value` values.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `codex.command` followed by the `--config` overrides for `codex.model` and `codex.provider`.
///
/// The overrides are appended, so the command must end with the `app-server` subcommand (or its
/// own flags). Custom providers always use the Responses API: Codex accepts no other wire format.
pub fn codex_command(codex: &CodexSettings) -> String {
    let mut overrides = Vec::new();
    if let Some(model) = &codex.model {
        overrides.push(format!("model={}", toml_string(model)));
    }
    if let Some(provider) = &codex.provider {
        let key = format!("model_providers.{}", provider.name);
        overrides.push(format!("model_provider={}", toml_string(&provider.name)));
        overrides.push(format!("{key}.name={}", toml_string(&provider.name)));
        overrides.push(format!(
            "{key}.base_url={}",
            toml_string(&provider.base_url)
        ));
        overrides.push(format!("{key}.wire_api=\"responses\""));
        if let Some(env_key) = &provider.api_key_env {
            overrides.push(format!("{key}.env_key={}", toml_string(env_key)));
        }
    }
    overrides
        .iter()
        .fold(codex.command.clone(), |command, value| {
            format!("{command} --config {}", shell_escape(value))
        })
}

fn join_parts(parts: [Option<String>; 3]) -> String {
    parts.into_iter().flatten().collect::<Vec<_>>().join(" && ")
}

fn unset_part(secret_names: &[String]) -> Option<String> {
    (!secret_names.is_empty()).then(|| format!("unset {}", secret_names.join(" ")))
}

/// `local_launch_command/1`: `[unset NAMES && ]exec <codex_command>` (the command is raw shell code).
pub fn local_launch_command(codex_command: &str, secret_names: &[String]) -> String {
    join_parts([
        None,
        unset_part(secret_names),
        Some(format!("exec {codex_command}")),
    ])
}

/// `remote_launch_command/2`: `cd '<ws>' && [unset NAMES && ]exec <codex_command>`.
pub fn remote_launch_command(
    workspace: &str,
    codex_command: &str,
    secret_names: &[String],
) -> String {
    join_parts([
        Some(format!("cd {}", shell_escape(workspace))),
        unset_part(secret_names),
        Some(format!("exec {codex_command}")),
    ])
}

/// `System.find_executable/1` for a bare program name: the first executable file on `PATH`.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn unreadable(err: PathError) -> InvalidWorkspaceCwd {
    let PathError::CanonicalizeFailed { path, reason } = err;
    InvalidWorkspaceCwd::PathUnreadable(path, reason)
}

/// Local `validate_workspace_cwd/2`: the canonical workspace must be strictly inside the canonical
/// `workspace_root` (already expanded against the workflow directory). Returns the canonical path,
/// which is then used as the cwd everywhere (including the JSON `cwd` fields).
pub fn validate_local_workspace(
    workspace: &str,
    workspace_root: &Path,
) -> Result<PathBuf, InvalidWorkspaceCwd> {
    let expanded = path_safety::expand_path(workspace, None);
    let expanded_root = path_safety::expand_path(workspace_root, None);
    let canonical = path_safety::canonicalize(&expanded).map_err(unreadable)?;
    let canonical_root = path_safety::canonicalize(&expanded_root).map_err(unreadable)?;

    let canonical_str = lossy(&canonical);
    let canonical_root_str = lossy(&canonical_root);
    if canonical_str == canonical_root_str {
        return Err(InvalidWorkspaceCwd::WorkspaceRoot(canonical));
    }
    if format!("{canonical_str}/").starts_with(&format!("{canonical_root_str}/")) {
        return Ok(canonical);
    }
    if format!("{}/", lossy(&expanded)).starts_with(&format!("{}/", lossy(&expanded_root))) {
        return Err(InvalidWorkspaceCwd::SymlinkEscape(expanded, canonical_root));
    }
    Err(InvalidWorkspaceCwd::OutsideWorkspaceRoot(
        canonical,
        canonical_root,
    ))
}

/// Remote `validate_workspace_cwd/2`: non-blank and free of `\n`, `\r` and NUL; used verbatim.
pub fn validate_remote_workspace(
    workspace: &str,
    worker_host: &str,
) -> Result<String, InvalidWorkspaceCwd> {
    if workspace.trim().is_empty() {
        return Err(InvalidWorkspaceCwd::EmptyRemoteWorkspace(
            worker_host.to_owned(),
        ));
    }
    if workspace.contains(['\n', '\r', '\0']) {
        return Err(InvalidWorkspaceCwd::InvalidRemoteWorkspace(
            worker_host.to_owned(),
            workspace.to_owned(),
        ));
    }
    Ok(workspace.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_command_appends_model_and_provider_overrides() {
        use symphony_core::config::CodexProvider;

        let mut codex = CodexSettings::default();
        assert_eq!(codex_command(&codex), "codex app-server");

        codex.model = Some("qwen3-coder".into());
        codex.provider = Some(CodexProvider {
            name: "ollama".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            api_key_env: Some("OLLAMA_API_KEY".into()),
        });
        assert_eq!(
            codex_command(&codex),
            concat!(
                "codex app-server",
                " --config 'model=\"qwen3-coder\"'",
                " --config 'model_provider=\"ollama\"'",
                " --config 'model_providers.ollama.name=\"ollama\"'",
                " --config 'model_providers.ollama.base_url=\"http://127.0.0.1:11434/v1\"'",
                " --config 'model_providers.ollama.wire_api=\"responses\"'",
                " --config 'model_providers.ollama.env_key=\"OLLAMA_API_KEY\"'",
            )
        );

        // Quotes cannot break out of the TOML string or the shell word.
        codex.provider = None;
        codex.model = Some(r#"a"b'c\d"#.into());
        assert_eq!(
            codex_command(&codex),
            r#"codex app-server --config 'model="a\"b'"'"'c\\d"'"#
        );
    }

    #[test]
    fn launch_commands_match_elixir() {
        let names = valid_secret_names([
            "LINEAR_API_KEY",
            "bad-name",
            "1BAD",
            "_OK",
            "LINEAR_API_KEY",
        ]);
        assert_eq!(names, vec!["LINEAR_API_KEY".to_owned(), "_OK".to_owned()]);
        assert_eq!(
            local_launch_command("codex app-server", &names),
            "unset LINEAR_API_KEY _OK && exec codex app-server"
        );
        assert_eq!(
            local_launch_command("codex app-server", &[]),
            "exec codex app-server"
        );
        assert_eq!(
            remote_launch_command("/w/it's", "codex app-server", &["A".into()]),
            "cd '/w/it'\"'\"'s' && unset A && exec codex app-server"
        );
        assert_eq!(
            shell_escape("printf 'hello'"),
            "'printf '\"'\"'hello'\"'\"''"
        );
    }

    #[test]
    fn remote_workspaces_are_checked_for_blank_and_control_characters() {
        assert_eq!(
            validate_remote_workspace("  ", "h"),
            Err(InvalidWorkspaceCwd::EmptyRemoteWorkspace("h".into()))
        );
        assert_eq!(
            validate_remote_workspace("/a\nb", "h"),
            Err(InvalidWorkspaceCwd::InvalidRemoteWorkspace(
                "h".into(),
                "/a\nb".into()
            ))
        );
        assert_eq!(validate_remote_workspace("~/ws", "h"), Ok("~/ws".into()));
    }

    #[test]
    fn find_executable_locates_sh() {
        assert!(find_executable("sh").is_some());
        assert!(find_executable("definitely-not-a-real-binary-xyz").is_none());
    }
}
