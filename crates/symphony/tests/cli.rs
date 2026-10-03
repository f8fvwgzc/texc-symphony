//! Process-level CLI behaviour of the `symphony` binary (Elixir `cli_test.exs`,
//! `workspace_before_remove_test.exs`, and the release smoke contract).

use std::path::Path;

use assert_cmd::Command;

const ACK: &str = "--i-understand-that-this-will-be-running-without-the-usual-guardrails";
const BANNER_FIRST_LINE: &str = "This Symphony implementation is a low key engineering preview.";
const USAGE: &str = "Usage: symphony [--logs-root <path>] [--port <port>] [path-to-WORKFLOW.md]";

/// The binary with a scrubbed environment (no SYMPHONY_* fallbacks leak in from the host).
fn symphony(cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("symphony").unwrap();
    cmd.current_dir(cwd);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("SYMPHONY_") {
            cmd.env_remove(name);
        }
    }
    cmd
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn no_arguments_print_the_guardrails_banner_and_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let output = symphony(dir.path()).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(err.contains(BANNER_FIRST_LINE), "{err}");
    assert!(err.contains("Codex will run without any guardrails."));
    assert!(err.contains(ACK));
    assert!(err.starts_with("\u{1b}[31m\u{1b}[1m╭"));
    assert!(stdout(&output).is_empty());
}

#[test]
fn bad_arguments_print_usage_and_exit_1_even_without_the_ack_flag() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["--wat"],
        vec!["a.md", "b.md"],
        vec!["--port", "nope"],
        vec![ACK, "--logs-root", " "],
    ] {
        let output = symphony(dir.path()).args(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let err = stderr(&output);
        assert!(err.starts_with(USAGE), "{args:?}: {err}");
        assert!(err.contains("symphony --help"));
    }
}

#[test]
fn version_and_help_exit_0() {
    let dir = tempfile::tempdir().unwrap();
    let output = symphony(dir.path()).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        stdout(&output).trim(),
        format!("symphony {}", symphony_cli::version::version())
    );
    assert!(stdout(&output).contains(env!("CARGO_PKG_VERSION")));
    let output = symphony(dir.path()).arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = stdout(&output);
    for flag in [
        "--logs-root",
        "--port",
        "--host",
        "--db-path",
        "--no-db",
        ACK,
    ] {
        assert!(help.contains(flag), "{flag} missing from help:\n{help}");
    }
}

#[test]
fn missing_workflow_file_is_reported_with_its_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let output = symphony(dir.path())
        .args([ACK, "missing.md"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let expected = dir.path().canonicalize().unwrap().join("missing.md");
    let err = stderr(&output);
    assert!(err.starts_with("Workflow file not found: "), "{err}");
    assert!(err.trim_end().ends_with("missing.md"));
    assert!(
        err.contains(&expected.display().to_string())
            || err.contains(&dir.path().display().to_string()),
        "{err}"
    );
    // The default path is ./WORKFLOW.md, also checked before anything starts.
    let output = symphony(dir.path()).arg(ACK).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("WORKFLOW.md"));
    // A directory is not a regular file.
    std::fs::create_dir(dir.path().join("dir.md")).unwrap();
    let output = symphony(dir.path()).args([ACK, "dir.md"]).output().unwrap();
    assert!(stderr(&output).starts_with("Workflow file not found: "));
}

#[test]
fn an_invalid_workflow_refuses_to_boot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\ntracker:\n  kind: linear\n  project_slug: proj\n---\nprompt\n",
    )
    .unwrap();
    let output = symphony(dir.path())
        .env_remove("LINEAR_API_KEY")
        .arg(ACK)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let err = stderr(&output);
    assert!(
        err.contains("Failed to start Symphony with workflow"),
        "{err}"
    );
    assert!(err.contains("missing_linear_api_token"), "{err}");

    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\npolling:\n  interval_ms: 0\n---\n",
    )
    .unwrap();
    let output = symphony(dir.path()).arg(ACK).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("polling.interval_ms must be greater than 0"));
}

#[test]
fn invalid_environment_fallbacks_are_named() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\ntracker:\n  kind: memory\n---\n",
    )
    .unwrap();
    let output = symphony(dir.path())
        .env("SYMPHONY_PORT", "http")
        .arg(ACK)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Invalid SYMPHONY_PORT=\"http\""));
    let output = symphony(dir.path())
        .env("SYMPHONY_LOG_FORMAT", "xml")
        .arg(ACK)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("Invalid SYMPHONY_LOG_FORMAT=\"xml\""));
}

#[cfg(unix)]
mod before_remove {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::*;

    fn script(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    struct Harness {
        dir: tempfile::TempDir,
        bin: PathBuf,
        log: PathBuf,
    }

    impl Harness {
        /// Fake `gh` (Elixir test defaults) and `git` printing `branch`.
        fn new(branch: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let log = dir.path().join("gh.log");
            script(
                &bin,
                "gh",
                r#"printf '%s\n' "$*" >> "$GH_LOG"
case "$1 $2" in
  "auth status") exit 0 ;;
  "pr list") printf '101\n102\n'; exit 0 ;;
  "pr close")
    if [ "$3" = "101" ]; then exit 0; fi
    if [ "$3" = "102" ]; then echo boom >&2; exit 17; fi ;;
esac
exit 99
"#,
            );
            script(&bin, "git", &format!("printf '{branch}\\n'\n"));
            Self { dir, bin, log }
        }

        fn run(&self, args: &[&str], path: Option<&Path>) -> std::process::Output {
            let mut cmd = symphony(self.dir.path());
            cmd.env("GH_LOG", &self.log)
                .env(
                    "PATH",
                    path.map_or_else(|| self.bin.clone(), Path::to_path_buf),
                )
                .args(["workspace", "before-remove"])
                .args(args);
            cmd.output().unwrap()
        }

        fn gh_log(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    #[test]
    fn closes_open_prs_of_the_current_branch_and_tolerates_failures() {
        let h = Harness::new("feature/workpad");
        let output = h.run(&[], None);
        assert!(output.status.success());
        assert!(stdout(&output).contains("Closed PR #101 for branch feature/workpad"));
        assert!(stderr(&output).contains(
            "Failed to close PR #102 for branch feature/workpad: exit 17 output=\"boom\""
        ));
        let log = h.gh_log();
        assert!(log.contains("auth status"));
        assert!(log.contains(
            "pr list --repo openai/symphony --head feature/workpad --state open --json number --jq .[].number"
        ));
        assert!(log.contains("pr close 101 --repo openai/symphony"));
        assert!(log.contains("pr close 102 --repo openai/symphony"));
    }

    #[test]
    fn explicit_branch_and_repo() {
        let h = Harness::new("ignored");
        let output = h.run(&["--branch", "feature/x", "--repo", "o/r"], None);
        assert!(output.status.success());
        assert!(stdout(&output).contains("Closed PR #101 for branch feature/x"));
        assert!(h.gh_log().contains("pr close 101 --repo o/r"));
    }

    #[test]
    fn no_op_without_gh_or_branch() {
        let h = Harness::new("feature/no-gh");
        let empty = h.dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let output = h.run(&["--branch", "feature/no-gh"], Some(&empty));
        assert!(output.status.success());
        assert_eq!(stdout(&output), "");
        let output = h.run(&[], Some(&empty));
        assert!(output.status.success());
        assert_eq!(stdout(&output), "");
        assert_eq!(h.gh_log(), "");
    }

    #[test]
    fn blank_branch_never_calls_gh() {
        let h = Harness::new("");
        let output = h.run(&[], None);
        assert!(output.status.success());
        assert_eq!(stdout(&output), "");
        assert_eq!(h.gh_log(), "");
    }

    #[test]
    fn help_and_invalid_options() {
        let h = Harness::new("x");
        let output = h.run(&["--help"], None);
        assert!(output.status.success());
        assert!(stdout(&output).contains("symphony workspace before-remove"));
        let output = h.run(&["--wat"], None);
        assert_eq!(output.status.code(), Some(1));
        assert!(stderr(&output).contains("Invalid option(s): [{\"--wat\", nil}]"));
        assert_eq!(h.gh_log(), "");
    }
}
