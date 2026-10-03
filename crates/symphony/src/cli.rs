//! Command-line evaluation (Elixir `SymphonyElixir.CLI`, blueprint A.2.1 / A.6), extended with
//! `--host`, `--db-path`/`--no-db`, environment fallbacks, `--help`, `--version` and the
//! `workspace before-remove` subcommand.
//!
//! [`evaluate`] is pure: the working directory, the environment and the file check are injected
//! through [`CliContext`], so every rule is unit-tested without touching the process state.
//!
//! Check order (A.6): parse/usage, then the guardrails acknowledgement, then `--logs-root`, then the
//! port, then the workflow file. Parsing happens first, so bad arguments print the usage line even
//! without the acknowledgement flag. A flag always wins over its environment variable.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use symphony_core::path_safety::expand_path;

use crate::version::version;

/// The Elixir usage line, printed verbatim on any argument error.
pub const USAGE: &str =
    "Usage: symphony [--logs-root <path>] [--port <port>] [path-to-WORKFLOW.md]";
/// Printed after [`USAGE`]: the Rust CLI has more options than the Elixir usage line lists.
pub const USAGE_HINT: &str = "Run `symphony --help` for all options.";
/// The acknowledgement flag (without the leading dashes).
pub const ACK_FLAG: &str = "i-understand-that-this-will-be-running-without-the-usual-guardrails";

/// Environment variable naming the workflow file (the positional argument wins).
pub const ENV_WORKFLOW: &str = "SYMPHONY_WORKFLOW";
/// Environment variable with the HTTP bind host (`--host` wins; it wins over `server.host`).
pub const ENV_HOST: &str = "SYMPHONY_HOST";
/// Environment variable with the HTTP port (`--port` wins; it wins over `server.port`).
pub const ENV_PORT: &str = "SYMPHONY_PORT";
/// Environment variable with the log root (`--logs-root` wins).
pub const ENV_LOGS_ROOT: &str = "SYMPHONY_LOGS_ROOT";

const BANNER_LINES: [&str; 4] = [
    "This Symphony implementation is a low key engineering preview.",
    "Codex will run without any guardrails.",
    "SymphonyElixir is not a supported product and is presented as-is.",
    "To proceed, start with `--i-understand-that-this-will-be-running-without-the-usual-guardrails` CLI argument",
];

/// The red guardrails box printed (to stderr) when the acknowledgement flag is missing. Byte-for-byte
/// the Elixir `acknowledgement_banner/0` (`\e[31m\e[1m` ... `\e[0m`).
pub fn acknowledgement_banner() -> String {
    let width = BANNER_LINES
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0);
    let border = "─".repeat(width + 2);
    let mut lines = Vec::with_capacity(BANNER_LINES.len() + 4);
    lines.push(format!("╭{border}╮"));
    let spacer = format!("│ {} │", " ".repeat(width));
    lines.push(spacer.clone());
    for line in BANNER_LINES {
        let pad = width - line.chars().count();
        lines.push(format!("│ {line}{} │", " ".repeat(pad)));
    }
    lines.push(spacer);
    lines.push(format!("╰{border}╯"));
    format!("\u{1b}[31m\u{1b}[1m{}\u{1b}[0m", lines.join("\n"))
}

#[derive(Debug, Parser)]
#[command(
    name = "symphony",
    about = "Turn tracker work into isolated, autonomous coding-agent runs.",
    long_about = "Turn tracker work into isolated, autonomous coding-agent runs.\n\n\
        Symphony polls the tracker configured in WORKFLOW.md, gives every eligible issue its own \
        workspace and drives a Codex app-server session in it.\n\n\
        Other commands:\n  symphony workspace before-remove [--branch <name>] [--repo <owner/name>]",
    disable_help_subcommand = true,
    args_override_self = true
)]
struct RunArgs {
    /// Acknowledge that Codex runs without the usual guardrails (required).
    #[arg(long = ACK_FLAG)]
    ack: bool,
    /// Log root; logs go to <path>/log/symphony.log [env: SYMPHONY_LOGS_ROOT] [default: .]
    #[arg(long = "logs-root", value_name = "path")]
    logs_root: Option<String>,
    /// HTTP port for the dashboard and API; 0 picks a free port [env: SYMPHONY_PORT] [default: server.port]
    #[arg(long, value_name = "port")]
    port: Option<u16>,
    /// HTTP bind address [env: SYMPHONY_HOST] [default: server.host, 127.0.0.1]
    #[arg(long, value_name = "addr")]
    host: Option<String>,
    /// SQLite run-history database (`:memory:` for an in-memory one) [env: SYMPHONY_DB_PATH] [default: ./data/symphony.db]
    #[arg(long = "db-path", value_name = "path", conflicts_with = "no_db")]
    db_path: Option<String>,
    /// Disable the run-history database
    #[arg(long = "no-db")]
    no_db: bool,
    /// Workflow file [env: SYMPHONY_WORKFLOW] [default: ./WORKFLOW.md]
    #[arg(value_name = "path-to-WORKFLOW.md")]
    workflow: Option<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "symphony workspace",
    about = "Workspace hook helpers.",
    disable_help_subcommand = true,
    args_override_self = true
)]
struct WorkspaceArgs {
    #[command(subcommand)]
    command: WorkspaceCommand,
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Close open GitHub PRs for the current branch before workspace removal.
    #[command(
        long_about = "Closes open pull requests for the current Git branch.\n\n\
            This command is intended for use from the `before_remove` workspace hook. It never \
            fails the hook: without a branch, without `gh`, or without a `gh` login it does nothing.\n\n\
            Usage:\n\n    symphony workspace before-remove\n    \
            symphony workspace before-remove --branch feature/my-branch\n    \
            symphony workspace before-remove --repo openai/symphony"
    )]
    BeforeRemove {
        /// Branch whose PRs are closed [default: `git branch --show-current`]
        #[arg(long, value_name = "name")]
        branch: Option<String>,
        /// GitHub repository
        #[arg(long, value_name = "owner/name", default_value = crate::before_remove::DEFAULT_REPO)]
        repo: String,
        /// Ignored (the Mix task ignored positional arguments too).
        #[arg(hide = true)]
        rest: Vec<String>,
    },
}

/// Run-history database selection from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbChoice {
    /// No flag: `SYMPHONY_DB_PATH` / the store default decides.
    FromEnv,
    /// `--db-path <path>`.
    Path(String),
    /// `--no-db`.
    Disabled,
}

/// A validated `symphony [...] WORKFLOW.md` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// Absolute (lexically expanded) workflow path; verified to be a regular file.
    pub workflow: PathBuf,
    /// Absolute log root (logs go to `<root>/log/symphony.log`).
    pub logs_root: PathBuf,
    /// Port override (flag, else env); `None` defers to `server.port`.
    pub port: Option<u16>,
    /// Host override (flag, else env); `None` defers to `server.host`.
    pub host: Option<String>,
    /// Database selection.
    pub db: DbChoice,
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the orchestrator.
    Run(Invocation),
    /// `symphony workspace before-remove`.
    BeforeRemove {
        /// `--branch`.
        branch: Option<String>,
        /// `--repo`.
        repo: String,
    },
}

/// Why evaluation stopped before running anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliExit {
    /// `--help`: text for stdout, exit 0.
    Help(String),
    /// `--version`: text for stdout, exit 0.
    Version(String),
    /// Bad arguments: [`USAGE`] + [`USAGE_HINT`], exit 1.
    Usage,
    /// Missing acknowledgement flag: [`acknowledgement_banner`], exit 1.
    Banner,
    /// The workflow path is not a regular file, exit 1.
    WorkflowNotFound(PathBuf),
    /// An environment fallback holds an unusable value, exit 1.
    InvalidEnv {
        /// Variable name.
        name: &'static str,
        /// Its value.
        value: String,
        /// What was expected.
        expected: &'static str,
    },
    /// Unknown option of a subcommand (Mix: `Invalid option(s): [{"--wat", nil}]`), exit 1.
    InvalidOption(String),
}

impl CliExit {
    /// Process exit code.
    pub fn exit_code(&self) -> u8 {
        match self {
            CliExit::Help(_) | CliExit::Version(_) => 0,
            _ => 1,
        }
    }

    /// `true` when [`CliExit::message`] goes to stdout (help and version); errors go to stderr.
    pub fn is_stdout(&self) -> bool {
        matches!(self, CliExit::Help(_) | CliExit::Version(_))
    }

    /// The text to print (without a trailing newline).
    pub fn message(&self) -> String {
        match self {
            CliExit::Help(text) | CliExit::Version(text) => text.trim_end().to_owned(),
            CliExit::Usage => format!("{USAGE}\n{USAGE_HINT}"),
            CliExit::Banner => acknowledgement_banner(),
            CliExit::WorkflowNotFound(path) => {
                format!("Workflow file not found: {}", path.display())
            }
            CliExit::InvalidEnv {
                name,
                value,
                expected,
            } => format!("Invalid {name}={value:?}: expected {expected}"),
            CliExit::InvalidOption(text) => text.clone(),
        }
    }
}

/// The process facts [`evaluate`] depends on.
pub struct CliContext<'a> {
    /// Directory relative paths expand against (the process CWD).
    pub cwd: PathBuf,
    /// Environment lookup.
    pub env: &'a dyn Fn(&str) -> Option<String>,
    /// `File.regular?/1` (follows symlinks; directories are not regular).
    pub file_regular: &'a dyn Fn(&Path) -> bool,
}

impl CliContext<'_> {
    fn env_value(&self, name: &str) -> Option<String> {
        (self.env)(name)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }
}

/// `File.regular?/1` on the real file system.
pub fn file_regular(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Evaluate `args` (including the program name in `args[0]`).
pub fn evaluate(args: &[OsString], ctx: &CliContext<'_>) -> Result<Command, CliExit> {
    if args.get(1).is_some_and(|arg| arg == "workspace") {
        return evaluate_workspace(args);
    }
    let matches = RunArgs::command()
        .version(version())
        .try_get_matches_from(args)
        .map_err(|err| match err.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
                CliExit::Help(err.render().to_string())
            }
            ErrorKind::DisplayVersion => CliExit::Version(err.render().to_string()),
            _ => CliExit::Usage,
        })?;
    let parsed = RunArgs::from_arg_matches(&matches).map_err(|_| CliExit::Usage)?;

    if !parsed.ack {
        return Err(CliExit::Banner);
    }

    let logs_root = match parsed.logs_root {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(CliExit::Usage);
            }
            expand_path(trimmed, Some(&ctx.cwd))
        }
        None => match ctx.env_value(ENV_LOGS_ROOT) {
            Some(root) => expand_path(root, Some(&ctx.cwd)),
            None => ctx.cwd.clone(),
        },
    };

    let port = match parsed.port {
        Some(port) => Some(port),
        None => match (ctx.env)(ENV_PORT) {
            Some(raw) if !raw.trim().is_empty() => {
                Some(raw.trim().parse::<u16>().map_err(|_| CliExit::InvalidEnv {
                    name: ENV_PORT,
                    value: raw.clone(),
                    expected: "a port number between 0 and 65535",
                })?)
            }
            _ => None,
        },
    };

    let host = match parsed.host {
        Some(host) if host.trim().is_empty() => return Err(CliExit::Usage),
        Some(host) => Some(host.trim().to_owned()),
        None => ctx.env_value(ENV_HOST),
    };

    let db = match (parsed.no_db, parsed.db_path) {
        (true, _) => DbChoice::Disabled,
        (false, Some(path)) if path.trim().is_empty() => return Err(CliExit::Usage),
        (false, Some(path)) => DbChoice::Path(path.trim().to_owned()),
        (false, None) => DbChoice::FromEnv,
    };

    let workflow = parsed
        .workflow
        .or_else(|| ctx.env_value(ENV_WORKFLOW))
        .unwrap_or_else(|| "WORKFLOW.md".to_owned());
    let workflow = expand_path(&workflow, Some(&ctx.cwd));
    if !(ctx.file_regular)(&workflow) {
        return Err(CliExit::WorkflowNotFound(workflow));
    }

    Ok(Command::Run(Invocation {
        workflow,
        logs_root,
        port,
        host,
        db,
    }))
}

fn evaluate_workspace(args: &[OsString]) -> Result<Command, CliExit> {
    // `symphony workspace <sub> ...` -> program name `symphony workspace`, then `<sub> ...`.
    let argv =
        std::iter::once(OsString::from("symphony workspace")).chain(args.iter().skip(2).cloned());
    let parsed = WorkspaceArgs::try_parse_from(argv).map_err(|err| match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            CliExit::Help(err.render().to_string())
        }
        ErrorKind::DisplayVersion => CliExit::Version(err.render().to_string()),
        _ => CliExit::InvalidOption(invalid_option_message(&err)),
    })?;
    match parsed.command {
        WorkspaceCommand::BeforeRemove { branch, repo, .. } => {
            Ok(Command::BeforeRemove { branch, repo })
        }
    }
}

/// `Invalid option(s): [{"--wat", nil}]` (Elixir `inspect` of `OptionParser`'s invalid list).
fn invalid_option_message(err: &clap::Error) -> String {
    match err.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(arg)) => {
            let name = arg.split([' ', '=']).next().unwrap_or(arg);
            format!("Invalid option(s): [{{{name:?}, nil}}]")
        }
        _ => format!(
            "Invalid option(s): {}",
            err.render().to_string().lines().next().unwrap_or_default()
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    struct Fixture {
        env: HashMap<&'static str, &'static str>,
        regular: bool,
        checked: RefCell<Vec<PathBuf>>,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                env: HashMap::new(),
                regular: true,
                checked: RefCell::new(Vec::new()),
            }
        }

        fn eval(&self, args: &[&str]) -> Result<Command, CliExit> {
            let env = |name: &str| self.env.get(name).map(|v| (*v).to_owned());
            let file = |path: &Path| {
                self.checked.borrow_mut().push(path.to_path_buf());
                self.regular
            };
            let ctx = CliContext {
                cwd: PathBuf::from("/work"),
                env: &env,
                file_regular: &file,
            };
            let argv: Vec<OsString> = std::iter::once("symphony")
                .chain(args.iter().copied())
                .map(OsString::from)
                .collect();
            evaluate(&argv, &ctx)
        }

        fn run(&self, args: &[&str]) -> Invocation {
            match self.eval(args) {
                Ok(Command::Run(invocation)) => invocation,
                other => panic!("expected a run invocation, got {other:?}"),
            }
        }
    }

    const ACK: &str = "--i-understand-that-this-will-be-running-without-the-usual-guardrails";

    #[test]
    fn banner_is_returned_without_the_ack_flag_and_no_dependency_is_called() {
        let fx = Fixture::new();
        assert_eq!(fx.eval(&["WORKFLOW.md"]), Err(CliExit::Banner));
        assert_eq!(fx.eval(&[]), Err(CliExit::Banner));
        assert!(fx.checked.borrow().is_empty());
        let banner = CliExit::Banner.message();
        assert!(banner.contains("This Symphony implementation is a low key engineering preview."));
        assert!(banner.contains("Codex will run without any guardrails."));
        assert!(
            banner.contains("SymphonyElixir is not a supported product and is presented as-is.")
        );
        assert!(banner.contains(ACK));
    }

    #[test]
    fn banner_box_matches_the_elixir_layout() {
        let banner = acknowledgement_banner();
        assert!(banner.starts_with("\u{1b}[31m\u{1b}[1m╭"));
        assert!(banner.ends_with("╯\u{1b}[0m"));
        let body = banner
            .trim_start_matches("\u{1b}[31m\u{1b}[1m")
            .trim_end_matches("\u{1b}[0m");
        let lines: Vec<&str> = body.split('\n').collect();
        assert_eq!(lines.len(), 8);
        let width = BANNER_LINES[3].chars().count();
        for line in &lines {
            assert_eq!(line.chars().count(), width + 4, "line {line:?}");
        }
        assert_eq!(lines[1], format!("│ {} │", " ".repeat(width)));
        assert_eq!(
            lines[2],
            format!(
                "│ {}{} │",
                BANNER_LINES[0],
                " ".repeat(width - BANNER_LINES[0].len())
            )
        );
    }

    #[test]
    fn parsing_happens_before_the_ack_check() {
        let fx = Fixture::new();
        assert_eq!(fx.eval(&["--wat"]), Err(CliExit::Usage));
        assert_eq!(fx.eval(&["a.md", "b.md"]), Err(CliExit::Usage));
        assert_eq!(fx.eval(&["--port", "abc"]), Err(CliExit::Usage));
        assert_eq!(fx.eval(&["--port", "-1"]), Err(CliExit::Usage));
        assert_eq!(fx.eval(&["--port", "70000"]), Err(CliExit::Usage));
        assert_eq!(fx.eval(&["--port"]), Err(CliExit::Usage));
        assert_eq!(
            fx.eval(&[ACK, "--db-path", "x.db", "--no-db"]),
            Err(CliExit::Usage)
        );
        let usage = CliExit::Usage.message();
        assert!(usage.starts_with(
            "Usage: symphony [--logs-root <path>] [--port <port>] [path-to-WORKFLOW.md]\n"
        ));
    }

    #[test]
    fn defaults_to_workflow_md_in_the_cwd() {
        let fx = Fixture::new();
        let run = fx.run(&[ACK]);
        assert_eq!(run.workflow, PathBuf::from("/work/WORKFLOW.md"));
        assert_eq!(run.logs_root, PathBuf::from("/work"));
        assert_eq!(run.port, None);
        assert_eq!(run.host, None);
        assert_eq!(run.db, DbChoice::FromEnv);
        assert_eq!(
            *fx.checked.borrow(),
            vec![PathBuf::from("/work/WORKFLOW.md")]
        );
    }

    #[test]
    fn explicit_workflow_paths_are_expanded() {
        let fx = Fixture::new();
        let run = fx.run(&[ACK, "tmp/custom/WORKFLOW.md"]);
        assert_eq!(run.workflow, PathBuf::from("/work/tmp/custom/WORKFLOW.md"));
        let run = fx.run(&[ACK, "../other/./W.md"]);
        assert_eq!(run.workflow, PathBuf::from("/other/W.md"));
    }

    #[test]
    fn logs_root_is_trimmed_expanded_and_last_one_wins() {
        let fx = Fixture::new();
        let run = fx.run(&[ACK, "--logs-root", "tmp/custom-logs", "WORKFLOW.md"]);
        assert_eq!(run.logs_root, PathBuf::from("/work/tmp/custom-logs"));
        let run = fx.run(&[ACK, "--logs-root=a", "--logs-root", " b "]);
        assert_eq!(run.logs_root, PathBuf::from("/work/b"));
        assert_eq!(fx.eval(&[ACK, "--logs-root", "  "]), Err(CliExit::Usage));
    }

    #[test]
    fn port_last_one_wins_and_zero_is_allowed() {
        let fx = Fixture::new();
        assert_eq!(fx.run(&[ACK, "--port", "1", "--port", "0"]).port, Some(0));
        assert_eq!(fx.run(&[ACK, "--port=4000"]).port, Some(4000));
    }

    #[test]
    fn environment_fallbacks_apply_when_flags_are_absent() {
        let mut fx = Fixture::new();
        fx.env.insert(ENV_PORT, " 4100 ");
        fx.env.insert(ENV_HOST, "0.0.0.0");
        fx.env.insert(ENV_LOGS_ROOT, "/data/logs");
        fx.env.insert(ENV_WORKFLOW, "/config/WORKFLOW.md");
        let run = fx.run(&[ACK]);
        assert_eq!(run.port, Some(4100));
        assert_eq!(run.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(run.logs_root, PathBuf::from("/data/logs"));
        assert_eq!(run.workflow, PathBuf::from("/config/WORKFLOW.md"));

        let run = fx.run(&[
            ACK,
            "--port",
            "5",
            "--host",
            "::1",
            "--logs-root",
            "l",
            "w.md",
        ]);
        assert_eq!(run.port, Some(5));
        assert_eq!(run.host.as_deref(), Some("::1"));
        assert_eq!(run.logs_root, PathBuf::from("/work/l"));
        assert_eq!(run.workflow, PathBuf::from("/work/w.md"));
    }

    #[test]
    fn empty_environment_values_are_ignored_and_bad_ports_are_rejected() {
        let mut fx = Fixture::new();
        fx.env.insert(ENV_PORT, "");
        fx.env.insert(ENV_HOST, " ");
        assert_eq!(fx.run(&[ACK]).port, None);
        assert_eq!(fx.run(&[ACK]).host, None);
        fx.env.insert(ENV_PORT, "abc");
        let err = fx.eval(&[ACK]).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            err.message(),
            "Invalid SYMPHONY_PORT=\"abc\": expected a port number between 0 and 65535"
        );
        // The flag wins, so a bad env value is never read.
        assert_eq!(fx.run(&[ACK, "--port", "1"]).port, Some(1));
    }

    #[test]
    fn database_flags() {
        let fx = Fixture::new();
        assert_eq!(fx.run(&[ACK, "--no-db"]).db, DbChoice::Disabled);
        assert_eq!(
            fx.run(&[ACK, "--db-path", "/tmp/x.db"]).db,
            DbChoice::Path("/tmp/x.db".into())
        );
        assert_eq!(fx.eval(&[ACK, "--db-path", ""]), Err(CliExit::Usage));
    }

    #[test]
    fn missing_workflow_file_is_reported_with_the_expanded_path() {
        let mut fx = Fixture::new();
        fx.regular = false;
        let err = fx.eval(&[ACK, "missing.md"]).unwrap_err();
        assert_eq!(
            err,
            CliExit::WorkflowNotFound(PathBuf::from("/work/missing.md"))
        );
        assert_eq!(err.message(), "Workflow file not found: /work/missing.md");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn help_and_version_exit_zero() {
        let fx = Fixture::new();
        let help = fx.eval(&["--help"]).unwrap_err();
        assert_eq!(help.exit_code(), 0);
        assert!(help.is_stdout());
        assert!(help.message().contains("--logs-root"));
        assert!(help.message().contains("workspace before-remove"));
        let version = fx.eval(&["--version"]).unwrap_err();
        assert_eq!(version.exit_code(), 0);
        assert_eq!(
            version.message(),
            format!("symphony {}", crate::version::version())
        );
    }

    #[test]
    fn workspace_before_remove_parses_without_the_ack_flag() {
        let fx = Fixture::new();
        assert_eq!(
            fx.eval(&["workspace", "before-remove"]),
            Ok(Command::BeforeRemove {
                branch: None,
                repo: "openai/symphony".into()
            })
        );
        assert_eq!(
            fx.eval(&[
                "workspace",
                "before-remove",
                "lint",
                "--branch",
                "feature/x",
                "--repo",
                "o/r"
            ]),
            Ok(Command::BeforeRemove {
                branch: Some("feature/x".into()),
                repo: "o/r".into()
            })
        );
        let err = fx
            .eval(&["workspace", "before-remove", "--wat"])
            .unwrap_err();
        assert_eq!(err.message(), "Invalid option(s): [{\"--wat\", nil}]");
        assert_eq!(err.exit_code(), 1);
        let help = fx
            .eval(&["workspace", "before-remove", "--help"])
            .unwrap_err();
        assert_eq!(help.exit_code(), 0);
        assert!(help.message().contains("symphony workspace before-remove"));
    }
}
