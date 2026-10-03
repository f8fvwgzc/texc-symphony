//! `symphony workspace before-remove` (Elixir `mix workspace.before_remove`, blueprint F.1.3).
//!
//! Closes the open GitHub pull requests of a workspace's branch when the workspace is removed
//! (the tracker issue reached a terminal state without merge). Meant for the `hooks.before_remove`
//! script, so it never fails: no branch, no `gh`, no `gh` login, a failed listing or a failed close
//! all end with exit 0.
//!
//! Commands are found on `PATH` at call time and run through a [`CommandRunner`], so tests inject
//! fake `gh`/`git` without touching the process environment. Unlike the Mix task, PR numbers are
//! read from **stdout only** (Elixir merged stderr, so warnings could be mistaken for numbers).

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Repository used when `--repo` is not given (Elixir default).
pub const DEFAULT_REPO: &str = "openai/symphony";

/// Outcome of one external command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Exit status 0.
    Success {
        /// Standard output.
        stdout: String,
    },
    /// Non-zero exit (or killed by a signal: `status` is `None`).
    Failure {
        /// Exit code.
        status: Option<i32>,
        /// Standard output followed by standard error.
        output: String,
    },
    /// The executable is not on `PATH` (or could not be started).
    NotFound,
}

/// Runs external commands.
pub trait CommandRunner {
    /// `true` when `command` resolves to an executable on `PATH`.
    fn available(&self, command: &str) -> bool;
    /// Runs `command args...` in the current directory.
    fn run(&self, command: &str, args: &[&str]) -> CommandOutcome;
}

/// [`CommandRunner`] over real processes, resolving commands against a `PATH` value.
#[derive(Debug, Clone)]
pub struct SystemRunner {
    path: Option<OsString>,
}

impl SystemRunner {
    /// Resolves against the process `PATH`.
    pub fn from_env() -> Self {
        Self {
            path: std::env::var_os("PATH"),
        }
    }

    /// Resolves against an explicit `PATH` value.
    pub fn with_path(path: Option<OsString>) -> Self {
        Self { path }
    }

    fn find(&self, command: &str) -> Option<PathBuf> {
        let path = self.path.as_ref()?;
        std::env::split_paths(path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(command))
            .find(|candidate| is_executable(candidate))
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

impl CommandRunner for SystemRunner {
    fn available(&self, command: &str) -> bool {
        self.find(command).is_some()
    }

    fn run(&self, command: &str, args: &[&str]) -> CommandOutcome {
        let Some(executable) = self.find(command) else {
            return CommandOutcome::NotFound;
        };
        let mut process = std::process::Command::new(executable);
        process.args(args).stdin(Stdio::null());
        if let Some(path) = &self.path {
            process.env("PATH", path);
        }
        match process.output() {
            Ok(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                if output.status.success() {
                    CommandOutcome::Success { stdout }
                } else {
                    let mut combined = stdout;
                    combined.push_str(&String::from_utf8_lossy(&output.stderr));
                    CommandOutcome::Failure {
                        status: output.status.code(),
                        output: combined,
                    }
                }
            }
            Err(_) => CommandOutcome::NotFound,
        }
    }
}

/// Closes the open PRs of `branch` (or the current branch) in `repo`. Messages go to `out`
/// (`Closed PR #N for branch B`) and `err` (`Failed to close PR #N for branch B: exit S...`).
pub fn run(
    branch: Option<String>,
    repo: &str,
    runner: &dyn CommandRunner,
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    let Some(branch) = branch.or_else(|| current_branch(runner)) else {
        return;
    };
    if !runner.available("gh") {
        return;
    }
    if !matches!(
        runner.run("gh", &["auth", "status"]),
        CommandOutcome::Success { .. }
    ) {
        return;
    }
    for number in open_pull_requests(runner, repo, &branch) {
        let comment = format!(
            "Closing because the tracker issue for branch {branch} entered a terminal state without merge."
        );
        match runner.run(
            "gh",
            &[
                "pr",
                "close",
                &number,
                "--repo",
                repo,
                "--comment",
                &comment,
            ],
        ) {
            CommandOutcome::Success { .. } => {
                let _ = writeln!(out, "Closed PR #{number} for branch {branch}");
            }
            CommandOutcome::Failure { status, output } => {
                let status = status.map_or_else(|| "signal".to_owned(), |code| code.to_string());
                let trimmed = output.trim();
                let suffix = if trimmed.is_empty() {
                    String::new()
                } else {
                    format!(" output={trimmed:?}")
                };
                let _ = writeln!(
                    err,
                    "Failed to close PR #{number} for branch {branch}: exit {status}{suffix}"
                );
            }
            CommandOutcome::NotFound => {
                let _ = writeln!(
                    err,
                    "Failed to close PR #{number} for branch {branch}: gh not found"
                );
            }
        }
    }
}

fn current_branch(runner: &dyn CommandRunner) -> Option<String> {
    match runner.run("git", &["branch", "--show-current"]) {
        CommandOutcome::Success { stdout } => {
            Some(stdout.trim().to_owned()).filter(|branch| !branch.is_empty())
        }
        _ => None,
    }
}

fn open_pull_requests(runner: &dyn CommandRunner, repo: &str, branch: &str) -> Vec<String> {
    let args = [
        "pr",
        "list",
        "--repo",
        repo,
        "--head",
        branch,
        "--state",
        "open",
        "--json",
        "number",
        "--jq",
        ".[].number",
    ];
    match runner.run("gh", &args) {
        CommandOutcome::Success { stdout } => stdout
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Scripted runner mirroring the Elixir fake `gh`/`git` scripts.
    struct Fake {
        gh: bool,
        git_branch: Option<&'static str>,
        auth_ok: bool,
        list: Result<&'static str, i32>,
        close_102: CommandOutcome,
        log: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                gh: true,
                git_branch: Some("feature/workpad\n"),
                auth_ok: true,
                list: Ok("101\n102\n"),
                close_102: CommandOutcome::Failure {
                    status: Some(17),
                    output: "boom\n".into(),
                },
                log: RefCell::new(Vec::new()),
            }
        }

        fn run_task(&self, branch: Option<&str>, repo: &str) -> (String, String) {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            run(branch.map(str::to_owned), repo, self, &mut out, &mut err);
            (
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        }

        fn gh_log(&self) -> String {
            self.log.borrow().join("\n")
        }
    }

    impl CommandRunner for Fake {
        fn available(&self, command: &str) -> bool {
            command != "gh" || self.gh
        }

        fn run(&self, command: &str, args: &[&str]) -> CommandOutcome {
            match command {
                "git" => match self.git_branch {
                    Some(branch) => CommandOutcome::Success {
                        stdout: branch.into(),
                    },
                    None => CommandOutcome::NotFound,
                },
                "gh" if self.gh => {
                    self.log.borrow_mut().push(args.join(" "));
                    match args {
                        ["auth", "status"] if self.auth_ok => CommandOutcome::Success {
                            stdout: String::new(),
                        },
                        ["auth", "status"] => CommandOutcome::Failure {
                            status: Some(1),
                            output: String::new(),
                        },
                        ["pr", "list", ..] => match self.list {
                            Ok(stdout) => CommandOutcome::Success {
                                stdout: stdout.into(),
                            },
                            Err(code) => CommandOutcome::Failure {
                                status: Some(code),
                                output: String::new(),
                            },
                        },
                        ["pr", "close", "101", ..] => CommandOutcome::Success {
                            stdout: String::new(),
                        },
                        ["pr", "close", "102", ..] => self.close_102.clone(),
                        _ => CommandOutcome::Failure {
                            status: Some(99),
                            output: String::new(),
                        },
                    }
                }
                _ => CommandOutcome::NotFound,
            }
        }
    }

    #[test]
    fn no_op_when_branch_is_unavailable() {
        let mut fake = Fake::new();
        fake.git_branch = None;
        assert_eq!(
            fake.run_task(None, DEFAULT_REPO),
            (String::new(), String::new())
        );
        assert_eq!(fake.gh_log(), "");
    }

    #[test]
    fn no_op_when_gh_is_unavailable() {
        let mut fake = Fake::new();
        fake.gh = false;
        assert_eq!(
            fake.run_task(Some("feature/no-gh"), DEFAULT_REPO),
            (String::new(), String::new())
        );
    }

    #[test]
    fn uses_the_current_branch_when_the_branch_option_is_omitted() {
        let fake = Fake::new();
        let (out, err) = fake.run_task(None, DEFAULT_REPO);
        assert!(out.contains("Closed PR #101 for branch feature/workpad"));
        assert!(err.contains(
            "Failed to close PR #102 for branch feature/workpad: exit 17 output=\"boom\""
        ));
        let log = fake.gh_log();
        assert!(log.contains(
            "pr list --repo openai/symphony --head feature/workpad --state open --json number --jq .[].number"
        ));
        assert!(log.contains("pr close 101 --repo openai/symphony"));
        assert!(log.contains("pr close 102 --repo openai/symphony"));
        assert!(log.contains(
            "--comment Closing because the tracker issue for branch feature/workpad entered a terminal state without merge."
        ));
    }

    #[test]
    fn closes_prs_for_an_explicit_branch_and_is_idempotent() {
        let fake = Fake::new();
        for _ in 0..2 {
            let (out, err) = fake.run_task(Some("feature/workpad"), "o/r");
            assert!(out.contains("Closed PR #101 for branch feature/workpad"));
            assert!(err.contains("Failed to close PR #102 for branch feature/workpad"));
        }
        let log = fake.gh_log();
        assert!(log.contains("auth status"));
        assert!(log.contains("pr close 101 --repo o/r"));
    }

    #[test]
    fn close_failures_without_output_have_no_output_suffix() {
        let mut fake = Fake::new();
        fake.close_102 = CommandOutcome::Failure {
            status: Some(17),
            output: "  \n".into(),
        };
        let (_, err) = fake.run_task(Some("feature/no-output"), DEFAULT_REPO);
        assert!(err.contains("Failed to close PR #102 for branch feature/no-output: exit 17\n"));
        assert!(!err.contains("output="));
    }

    #[test]
    fn no_op_when_the_pr_list_fails() {
        let mut fake = Fake::new();
        fake.list = Err(1);
        assert_eq!(
            fake.run_task(None, DEFAULT_REPO),
            (String::new(), String::new())
        );
        let log = fake.gh_log();
        assert!(log.contains("auth status"));
        assert!(log.contains("pr list"));
        assert!(!log.contains("pr close"));
    }

    #[test]
    fn no_op_when_the_current_branch_is_blank() {
        let mut fake = Fake::new();
        fake.git_branch = Some("\n");
        assert_eq!(
            fake.run_task(None, DEFAULT_REPO),
            (String::new(), String::new())
        );
        assert_eq!(fake.gh_log(), "");
    }

    #[test]
    fn no_op_when_gh_auth_is_unavailable() {
        let mut fake = Fake::new();
        fake.auth_ok = false;
        assert_eq!(
            fake.run_task(None, DEFAULT_REPO),
            (String::new(), String::new())
        );
        let log = fake.gh_log();
        assert!(log.contains("auth status"));
        assert!(!log.contains("pr list"));
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_resolves_commands_on_the_given_path_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hello");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"hi $1\"\necho oops >&2\nexit \"${2:-0}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let runner = SystemRunner::with_path(Some(dir.path().as_os_str().to_owned()));
        assert!(runner.available("hello"));
        assert!(!runner.available("definitely-not-a-command"));
        assert_eq!(
            runner.run("hello", &["there"]),
            CommandOutcome::Success {
                stdout: "hi there\n".into()
            }
        );
        assert_eq!(
            runner.run("hello", &["x", "3"]),
            CommandOutcome::Failure {
                status: Some(3),
                output: "hi x\noops\n".into()
            }
        );
        let empty = SystemRunner::with_path(Some(OsString::new()));
        assert_eq!(empty.run("hello", &[]), CommandOutcome::NotFound);
        assert!(!SystemRunner::with_path(None).available("sh"));
    }
}
