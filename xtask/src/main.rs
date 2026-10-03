//! Repository chores: `cargo run -p xtask -- <task> [args]`.
//!
//! Tasks:
//! - `pr-body-check [--file F] [--template T]`: validate a PR description against
//!   `.github/pull_request_template.md` (port of `mix pr_body.check`).

mod pr_body;

use std::io::Write;
use std::process::ExitCode;

const USAGE: &str = "Usage: cargo run -p xtask -- <task> [args]

Tasks:
  pr-body-check [--file <path>] [--template <path>]
      Validate a PR description against .github/pull_request_template.md
      (the body file defaults to $PR_BODY_FILE).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pr-body-check") => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
            let outcome =
                pr_body::run(&args[1..], &cwd, std::env::var(pr_body::ENV_BODY_FILE).ok());
            let _ = std::io::stdout().write_all(outcome.stdout.as_bytes());
            let _ = std::io::stderr().write_all(outcome.stderr.as_bytes());
            ExitCode::from(u8::try_from(outcome.code).unwrap_or(1))
        }
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("Unknown task: {other}\n\n{USAGE}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}
