//! SSH transport for remote workers (`SymphonyElixir.SSH`, B.11).
//!
//! - argv: `[-F $SYMPHONY_SSH_CONFIG] -T [-p PORT] DESTINATION "bash -lc '<command>'"`; the remote
//!   shell command is a **single** argv element (ssh re-joins it for the remote shell).
//! - `host:port` shorthand is split into `-p PORT` (OpenSSH would treat it as a hostname); bracketed
//!   IPv6 (`root@[::1]:2200`) keeps its brackets; an unbracketed `::1:2200` is passed unchanged.
//! - No `BatchMode`/`ConnectTimeout`/... options are added: everything comes from the user's ssh config
//!   or `-F`. Callers bound every remote command with a timeout.
//!
//! Unlike Elixir, the ssh executable and config file are explicit [`SshConfig`] values (read from the
//! environment once by [`SshConfig::from_env`]), so tests can point at a fake `ssh` without mutating
//! the process environment.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use symphony_codex::launch::{find_executable, shell_escape};
use symphony_codex::{CodexError, RemoteLauncher};
use tokio::process::Command;

use crate::process::{self, CommandOutput, EnvPolicy, ProcessError};

/// Env var naming an ssh config file passed as `ssh -F <path>`.
pub const SSH_CONFIG_ENV: &str = "SYMPHONY_SSH_CONFIG";

/// SSH transport failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SshError {
    /// `ssh` is not on `PATH` (`{:error, :ssh_not_found}`).
    #[error("ssh_not_found")]
    NotFound,
    /// The ssh process could not be started.
    #[error("ssh_spawn_failed: {0}")]
    Spawn(String),
}

/// How to invoke `ssh`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshConfig {
    /// Explicit executable; `None` searches `ssh` on `PATH` at call time.
    pub executable: Option<PathBuf>,
    /// `-F <file>` (from `SYMPHONY_SSH_CONFIG`; blank means none).
    pub config_file: Option<String>,
}

impl SshConfig {
    /// Reads `SYMPHONY_SSH_CONFIG` from the process environment; `ssh` is looked up on `PATH`.
    pub fn from_env() -> Self {
        Self {
            executable: None,
            config_file: std::env::var(SSH_CONFIG_ENV)
                .ok()
                .filter(|value| !value.is_empty()),
        }
    }

    /// Uses `executable` instead of searching `PATH` (tests point this at a fake ssh).
    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    /// Sets (or clears) the `-F` config file.
    pub fn with_config_file(mut self, config_file: Option<String>) -> Self {
        self.config_file = config_file.filter(|value| !value.is_empty());
        self
    }

    /// The ssh executable (`ssh_executable/0`).
    pub fn resolve_executable(&self) -> Result<PathBuf, SshError> {
        match &self.executable {
            Some(path) => Ok(path.clone()),
            None => find_executable("ssh").ok_or(SshError::NotFound),
        }
    }

    /// The argument vector for running `command` on `host` (`ssh_args/2`).
    pub fn args(&self, host: &str, command: &str) -> Vec<String> {
        let target = SshTarget::parse(host);
        let mut args = Vec::new();
        if let Some(config) = &self.config_file {
            args.push("-F".to_owned());
            args.push(config.clone());
        }
        args.push("-T".to_owned());
        if let Some(port) = target.port {
            args.push("-p".to_owned());
            args.push(port);
        }
        args.push(target.destination);
        args.push(remote_shell_command(command));
        args
    }

    /// A `tokio` command running `command` on `host` (stdio and process group are left to the caller).
    pub fn command(&self, host: &str, command: &str) -> Result<Command, SshError> {
        let mut cmd = Command::new(self.resolve_executable()?);
        cmd.args(self.args(host, command));
        Ok(cmd)
    }

    /// `SSH.run/3` with an explicit timeout: runs `command` on `host`, stdout and stderr merged.
    ///
    /// The local ssh process runs in its own process group, which is killed on timeout (Elixir left
    /// the `ssh` process running), and gets the environment selected by `env`.
    pub async fn run(
        &self,
        host: &str,
        command: &str,
        timeout: Duration,
        env: &EnvPolicy,
    ) -> Result<CommandOutput, SshRunError> {
        let mut cmd = self.command(host, command).map_err(SshRunError::Ssh)?;
        env.apply(&mut cmd);
        process::run_captured(cmd, timeout)
            .await
            .map_err(|err| match err {
                ProcessError::Timeout => SshRunError::Timeout,
                ProcessError::Spawn(reason) => SshRunError::Ssh(SshError::Spawn(reason)),
            })
    }
}

/// Failure of [`SshConfig::run`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SshRunError {
    /// ssh missing or not spawnable.
    #[error(transparent)]
    Ssh(SshError),
    /// The remote command exceeded its timeout (the local ssh process group was killed).
    #[error("timeout")]
    Timeout,
}

/// A parsed `host[:port]` worker target (`parse_target/1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    /// `[user@]host` (brackets kept for IPv6).
    pub destination: String,
    /// Port from a `:<digits>` suffix, when the destination allows it.
    pub port: Option<String>,
}

impl SshTarget {
    /// Trims `target`, then splits a trailing `:<digits>` into a port when the remaining destination is
    /// non-empty and either contains no `:` or is bracketed (`[` and `]`).
    pub fn parse(target: &str) -> Self {
        let trimmed = target.trim();
        let whole = || Self {
            destination: trimmed.to_owned(),
            port: None,
        };
        let Some((destination, port)) = trimmed.rsplit_once(':') else {
            return whole();
        };
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return whole();
        }
        let bracketed = destination.contains('[') && destination.contains(']');
        if destination.is_empty() || (destination.contains(':') && !bracketed) {
            return whole();
        }
        Self {
            destination: destination.to_owned(),
            port: Some(port.to_owned()),
        }
    }
}

/// `remote_shell_command/1`: `bash -lc '<escaped command>'`.
pub fn remote_shell_command(command: &str) -> String {
    format!("bash -lc {}", shell_escape(command))
}

/// [`RemoteLauncher`] for Codex sessions on SSH workers (`SSH.start_port/3`).
///
/// The codex session configures stdio, the process group, `kill_on_drop` and the removal of secret
/// variables on the returned command.
#[derive(Debug, Clone, Default)]
pub struct SshLauncher {
    config: SshConfig,
}

impl SshLauncher {
    /// A launcher using `config`.
    pub fn new(config: SshConfig) -> Self {
        Self { config }
    }

    /// As an `Arc<dyn RemoteLauncher>` for [`symphony_codex::StartOptions`].
    pub fn shared(config: SshConfig) -> Arc<dyn RemoteLauncher> {
        Arc::new(Self::new(config))
    }
}

impl RemoteLauncher for SshLauncher {
    fn command(&self, worker_host: &str, remote_command: &str) -> Result<Command, CodexError> {
        self.config
            .command(worker_host, remote_command)
            .map_err(|err| match err {
                SshError::NotFound => CodexError::SshNotFound,
                SshError::Spawn(reason) => CodexError::SpawnFailed(reason),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(destination: &str, port: Option<&str>) -> SshTarget {
        SshTarget {
            destination: destination.into(),
            port: port.map(Into::into),
        }
    }

    #[test]
    fn parses_host_port_targets_like_elixir() {
        assert_eq!(
            SshTarget::parse("localhost:2222"),
            target("localhost", Some("2222"))
        );
        assert_eq!(
            SshTarget::parse(" root@127.0.0.1:2200 "),
            target("root@127.0.0.1", Some("2200"))
        );
        assert_eq!(
            SshTarget::parse("root@[::1]:2200"),
            target("root@[::1]", Some("2200"))
        );
        assert_eq!(SshTarget::parse("::1:2200"), target("::1:2200", None));
        assert_eq!(SshTarget::parse("worker-a"), target("worker-a", None));
        assert_eq!(SshTarget::parse(":22"), target(":22", None));
        assert_eq!(SshTarget::parse("host:"), target("host:", None));
        assert_eq!(SshTarget::parse("host:2x"), target("host:2x", None));
    }

    #[test]
    fn args_put_config_tty_port_destination_and_one_command_element() {
        let config = SshConfig::default().with_config_file(Some("/tmp/cfg".into()));
        assert_eq!(
            config.args("localhost:2222", "echo hi"),
            vec![
                "-F",
                "/tmp/cfg",
                "-T",
                "-p",
                "2222",
                "localhost",
                "bash -lc 'echo hi'"
            ]
        );
        assert_eq!(
            SshConfig::default().args("::1:2200", "x"),
            vec!["-T", "::1:2200", "bash -lc 'x'"]
        );
        assert_eq!(
            SshConfig::default()
                .with_config_file(Some(String::new()))
                .config_file,
            None
        );
    }

    #[test]
    fn remote_shell_command_escapes_embedded_single_quotes() {
        assert_eq!(
            remote_shell_command("printf 'hello'"),
            "bash -lc 'printf '\"'\"'hello'\"'\"''"
        );
    }

    #[test]
    fn missing_executable_is_ssh_not_found() {
        let config = SshConfig::default();
        // Only meaningful when ssh is absent; with an explicit executable the lookup is skipped.
        let explicit = config.clone().with_executable("/nonexistent/ssh");
        assert_eq!(
            explicit.resolve_executable(),
            Ok(PathBuf::from("/nonexistent/ssh"))
        );
    }
}
