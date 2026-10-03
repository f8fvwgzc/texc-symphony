//! Child processes for hooks and remote commands.
//!
//! Elixir ran hooks with `System.cmd("sh", ["-lc", ...], stderr_to_stdout: true)` inside a Task and, on
//! timeout, killed only the Erlang task: the `sh` process (and anything it started) kept running (G5).
//! Here every child runs in its **own process group** and the whole group gets `SIGKILL` on timeout or
//! when the waiting future is dropped (worker abort, shutdown).

use std::process::Stdio;
use std::time::Duration;

use symphony_codex::launch::valid_secret_names;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{Instant, timeout, timeout_at};

/// How long output is still drained after the child exited (grandchildren may keep the pipe open).
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(250);
/// How long a killed child is awaited before giving up on reaping it.
const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Exit status and merged stdout/stderr of a finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Merged stdout + stderr (lossy UTF-8).
    pub output: String,
    /// Exit status; signal deaths are `128 + signal`.
    pub status: i32,
}

/// Failure of [`run_captured`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    /// The command exceeded its timeout; its process group was killed.
    #[error("timeout")]
    Timeout,
    /// The command could not be started.
    #[error("spawn_failed: {0}")]
    Spawn(String),
}

/// Environment handed to hook (and remote-hook `ssh`) processes.
///
/// Elixir passed the full orchestrator environment, including tracker secrets, to hooks while Codex
/// had them unset (D10). The default here strips the tracker secret variables from hooks too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvPolicy {
    remove: Vec<String>,
    set: Vec<(String, String)>,
}

impl EnvPolicy {
    /// Inherit the process environment unchanged.
    pub fn inherit() -> Self {
        Self::default()
    }

    /// Remove `names` (invalid env names are ignored) from the child environment.
    pub fn strip<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            remove: valid_secret_names(names),
            set: Vec::new(),
        }
    }

    /// Also set `name=value` in the child (applied before removals).
    pub fn with_var(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.set.push((name.into(), value.into()));
        self
    }

    /// Names removed from the child environment.
    pub fn removed(&self) -> &[String] {
        &self.remove
    }

    /// Applies the policy to `cmd`.
    pub fn apply(&self, cmd: &mut Command) {
        for (name, value) in &self.set {
            cmd.env(name, value);
        }
        for name in &self.remove {
            cmd.env_remove(name);
        }
    }
}

/// Kills the child's process group when dropped, unless disarmed.
struct GroupGuard {
    pid: Option<u32>,
}

impl GroupGuard {
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            kill_process_group(pid);
        }
    }
}

/// `SIGKILL` to the process group led by `pid` (ESRCH ignored).
#[cfg(unix)]
pub(crate) fn kill_process_group(pid: u32) {
    use rustix::process::{Pid, Signal};
    if let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) {
        let _ = rustix::process::kill_process_group(pid, Signal::KILL);
    }
}

#[cfg(not(unix))]
pub(crate) fn kill_process_group(_pid: u32) {}

#[cfg(unix)]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(-1)
}

#[cfg(not(unix))]
fn exit_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

#[cfg(unix)]
fn merged_output_pipe(cmd: &mut Command) -> std::io::Result<tokio::net::unix::pipe::Receiver> {
    let (sender, receiver) = tokio::net::unix::pipe::pipe()?;
    let write_end = sender.into_blocking_fd()?;
    let stderr_end = write_end.try_clone()?;
    cmd.stdout(Stdio::from(write_end));
    cmd.stderr(Stdio::from(stderr_end));
    Ok(receiver)
}

/// Runs `cmd` to completion with stdout and stderr merged into one stream, bounded by `limit`.
///
/// - stdin is `/dev/null`; the child leads a new process group (`process_group(0)`), `kill_on_drop`.
/// - On timeout the whole group gets `SIGKILL`, the child is reaped and [`ProcessError::Timeout`] is
///   returned. Dropping the returned future also kills the group.
/// - After a normal exit, output is drained for a short grace period; processes the command left
///   running in the background are **not** killed (Elixir parity for successful hooks).
#[cfg(unix)]
pub async fn run_captured(
    mut cmd: Command,
    limit: Duration,
) -> Result<CommandOutput, ProcessError> {
    let deadline = Instant::now() + limit;
    cmd.stdin(Stdio::null()).kill_on_drop(true).process_group(0);
    let mut receiver =
        merged_output_pipe(&mut cmd).map_err(|e| ProcessError::Spawn(e.to_string()))?;
    let spawned = cmd.spawn();
    // Close the parent's copies of the pipe's write end so EOF arrives when the children exit.
    drop(cmd);
    let mut child = spawned.map_err(|e| ProcessError::Spawn(e.to_string()))?;
    let mut guard = GroupGuard { pid: child.id() };

    let mut output = Vec::new();
    let mut buf = [0u8; 8192];
    let mut eof = false;
    let status = loop {
        tokio::select! {
            read = receiver.read(&mut buf), if !eof => match read {
                Ok(0) | Err(_) => eof = true,
                Ok(n) => output.extend_from_slice(&buf[..n]),
            },
            status = child.wait() => break status,
            () = tokio::time::sleep_until(deadline) => {
                if let Some(pid) = child.id() {
                    kill_process_group(pid);
                }
                let _ = child.start_kill();
                let _ = timeout(REAP_TIMEOUT, child.wait()).await;
                guard.disarm();
                return Err(ProcessError::Timeout);
            }
        }
    };
    guard.disarm();
    let status = match status {
        Ok(status) => exit_code(status),
        Err(err) => return Err(ProcessError::Spawn(err.to_string())),
    };
    let drain_until = (Instant::now() + OUTPUT_DRAIN_GRACE).min(deadline.max(Instant::now()));
    while !eof {
        match timeout_at(drain_until, receiver.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => eof = true,
            Ok(Ok(n)) => output.extend_from_slice(&buf[..n]),
        }
    }
    Ok(CommandOutput {
        output: String::from_utf8_lossy(&output).into_owned(),
        status,
    })
}

/// Non-unix fallback: separate pipes concatenated (no process groups).
#[cfg(not(unix))]
pub async fn run_captured(
    mut cmd: Command,
    limit: Duration,
) -> Result<CommandOutput, ProcessError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| ProcessError::Spawn(e.to_string()))?;
    match timeout(limit, child.wait_with_output()).await {
        Err(_) => Err(ProcessError::Timeout),
        Ok(Err(err)) => Err(ProcessError::Spawn(err.to_string())),
        Ok(Ok(out)) => {
            let mut bytes = out.stdout;
            bytes.extend_from_slice(&out.stderr);
            Ok(CommandOutput {
                output: String::from_utf8_lossy(&bytes).into_owned(),
                status: exit_code(out.status),
            })
        }
    }
}

/// Truncates hook output for logs: the first `max_bytes` bytes plus `"... (truncated)"`
/// (`sanitize_hook_output_for_log/2`; the error value keeps the full output).
pub fn truncate_for_log(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_owned();
    }
    let mut cut = max_bytes;
    while !output.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}... (truncated)", &output[..cut])
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[tokio::test]
    async fn captures_merged_output_and_status() {
        let out = run_captured(sh("echo out; echo err >&2; exit 3"), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(out.status, 3);
        assert!(out.output.contains("out\n"));
        assert!(out.output.contains("err\n"));
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > '{}'; wait", pidfile.to_string_lossy());
        let started = std::time::Instant::now();
        let err = run_captured(sh(&script), Duration::from_millis(300))
            .await
            .unwrap_err();
        assert_eq!(err, ProcessError::Timeout);
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The grandchild `sleep` was in the group and must be gone (give the kernel a moment).
        let mut alive = true;
        for _ in 0..50 {
            alive =
                rustix::process::test_kill_process(rustix::process::Pid::from_raw(pid).unwrap())
                    .is_ok();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive, "background sleep survived the hook timeout");
    }

    #[tokio::test]
    async fn background_children_holding_the_pipe_do_not_block_completion() {
        let started = std::time::Instant::now();
        let out = run_captured(sh("echo hi; (sleep 5 &) ; exit 0"), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.status, 0);
        assert!(out.output.starts_with("hi"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn env_policy_drops_invalid_names() {
        let policy = EnvPolicy::strip(["GOOD_NAME", "bad-name", "GOOD_NAME"]);
        assert_eq!(policy.removed(), ["GOOD_NAME".to_owned()]);
    }

    #[test]
    fn truncation_matches_elixir_marker() {
        assert_eq!(truncate_for_log("abc", 5), "abc");
        assert_eq!(truncate_for_log("abcdef", 3), "abc... (truncated)");
        assert_eq!(truncate_for_log("ééé", 3), "é... (truncated)");
    }
}
