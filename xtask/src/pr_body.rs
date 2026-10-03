//! `pr-body-check` (Elixir `mix pr_body.check`, blueprint F.1.1): validates a PR description
//! against the structure of `.github/pull_request_template.md`.
//!
//! Byte-compatible messages; the same (sometimes surprising) rules: headings are found by plain
//! substring search, a section only counts when its heading is followed by exactly `"\n\n"`, and
//! the next section starts at the first `"\n" + <any other heading>`. Additions: `--template`, and
//! `$PR_BODY_FILE` when `--file` is absent.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// Template locations tried in order, relative to the working directory.
pub const TEMPLATE_PATHS: [&str; 2] = [
    ".github/pull_request_template.md",
    "../.github/pull_request_template.md",
];

/// Environment variable naming the body file when `--file` is not given.
pub const ENV_BODY_FILE: &str = "PR_BODY_FILE";

/// `--help` text.
pub const HELP: &str =
    "Validates a PR description markdown file against the structure and expectations
implied by the repository pull request template.

Usage:

    cargo run -p xtask -- pr-body-check --file /path/to/pr_body.md
    cargo run -p xtask -- pr-body-check --file body.md --template .github/pull_request_template.md
    PR_BODY_FILE=/path/to/pr_body.md cargo run -p xtask -- pr-body-check
";

static HEADING: LazyLock<Regex> = LazyLock::new(|| compile(r"(?m)^#{4,6}\s+.+$"));
static BULLET: LazyLock<Regex> = LazyLock::new(|| compile(r"(?m)^- "));
static CHECKBOX_TEMPLATE: LazyLock<Regex> = LazyLock::new(|| compile(r"(?m)^- \[ \] "));
static CHECKBOX_BODY: LazyLock<Regex> = LazyLock::new(|| compile(r"(?m)^- \[[ xX]\] "));

fn compile(pattern: &str) -> Regex {
    // The patterns are compile-time constants covered by the tests; failure is impossible.
    Regex::new(pattern).unwrap_or_else(|err| panic!("invalid built-in regex {pattern}: {err}"))
}

/// Result of a run: what to print and the exit code.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Text for stdout.
    pub stdout: String,
    /// Text for stderr.
    pub stderr: String,
    /// Process exit code.
    pub code: i32,
}

impl Outcome {
    fn fail(mut self, message: &str) -> Self {
        self.stderr.push_str(message);
        self.stderr.push('\n');
        self.code = 1;
        self
    }
}

#[derive(Debug, Default)]
struct Options {
    file: Option<String>,
    template: Option<String>,
    help: bool,
    invalid: Vec<String>,
}

fn parse(args: &[String]) -> Options {
    let mut opts = Options::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) if name.starts_with("--") => (name, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        match name {
            "--help" | "-h" => opts.help = true,
            "--file" | "--template" => {
                let value = inline.or_else(|| iter.next().cloned());
                match value {
                    Some(value) if name == "--file" => opts.file = Some(value),
                    Some(value) => opts.template = Some(value),
                    None => opts.invalid.push(format!("{{{name:?}, nil}}")),
                }
            }
            other if other.starts_with('-') => opts.invalid.push(format!("{{{other:?}, nil}}")),
            // Positional arguments are ignored, like the Mix task did.
            _ => {}
        }
    }
    opts
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    cwd.join(path)
}

/// Runs the check with `args` (after `pr-body-check`), relative paths against `cwd`, and
/// `env_body_file` as the `$PR_BODY_FILE` fallback.
pub fn run(args: &[String], cwd: &Path, env_body_file: Option<String>) -> Outcome {
    let opts = parse(args);
    let out = Outcome::default();
    if opts.help {
        return Outcome {
            stdout: HELP.to_owned(),
            ..out
        };
    }
    if !opts.invalid.is_empty() {
        return out.fail(&format!("Invalid option(s): [{}]", opts.invalid.join(", ")));
    }
    let Some(file) = opts
        .file
        .or(env_body_file.filter(|value| !value.trim().is_empty()))
    else {
        return out.fail("Missing required option --file");
    };

    let template = match &opts.template {
        Some(path) => std::fs::read_to_string(resolve(cwd, path))
            .map(|text| (path.clone(), text))
            .map_err(|err| format!("Unable to read PR template {path}: {err}")),
        None => TEMPLATE_PATHS
            .iter()
            .find_map(|path| {
                std::fs::read_to_string(resolve(cwd, path))
                    .ok()
                    .map(|text| ((*path).to_owned(), text))
            })
            .ok_or_else(|| {
                format!(
                    "Unable to read PR template from any of: {}",
                    TEMPLATE_PATHS.join(", ")
                )
            }),
    };
    let (template_path, template) = match template {
        Ok(found) => found,
        Err(message) => return out.fail(&message),
    };
    let body = match std::fs::read(resolve(cwd, &file)) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) => return out.fail(&format!("Unable to read {file}: {err}")),
    };
    let headings = extract_headings(&template);
    if headings.is_empty() {
        return out.fail(&format!("No markdown headings found in {template_path}"));
    }
    let errors = lint(&template, &body, &headings);
    if errors.is_empty() {
        return Outcome {
            stdout: "PR body format OK\n".to_owned(),
            ..out
        };
    }
    let mut out = out;
    for error in &errors {
        out.stderr.push_str(&format!("ERROR: {error}\n"));
    }
    out.fail(&format!(
        "PR body format invalid. Read `{template_path}` and follow it precisely."
    ))
}

/// Every `####`–`######` heading line of the template (whole matched line).
pub fn extract_headings(template: &str) -> Vec<String> {
    HEADING
        .find_iter(template)
        .map(|m| m.as_str().to_owned())
        .collect()
}

/// The ordered error list (`lint/3`).
pub fn lint(template: &str, body: &str, headings: &[String]) -> Vec<String> {
    let mut errors: Vec<String> = headings
        .iter()
        .filter(|heading| !body.contains(heading.as_str()))
        .map(|heading| format!("Missing required heading: {heading}"))
        .collect();

    let positions: Vec<usize> = headings
        .iter()
        .filter_map(|heading| body.find(heading.as_str()))
        .collect();
    if !positions.is_sorted() {
        errors.push("Required headings are out of order.".to_owned());
    }

    if body.contains("<!--") {
        errors.push(
            "PR description still contains template placeholder comments (<!-- ... -->)."
                .to_owned(),
        );
    }

    for heading in headings {
        let template_section = capture_heading_section(template, heading, headings);
        let Some(body_section) = capture_heading_section(body, heading, headings) else {
            continue;
        };
        if body_section.trim().is_empty() {
            errors.push(format!("Section cannot be empty: {heading}"));
            continue;
        }
        let template_section = template_section.unwrap_or_default();
        if BULLET.is_match(template_section) && !BULLET.is_match(body_section) {
            errors.push(format!(
                "Section must include at least one bullet item: {heading}"
            ));
        }
        if CHECKBOX_TEMPLATE.is_match(template_section) && !CHECKBOX_BODY.is_match(body_section) {
            errors.push(format!(
                "Section must include at least one checkbox item: {heading}"
            ));
        }
    }
    errors
}

/// The text between `heading` + `"\n\n"` and the next `"\n" + <other heading>`; `None` when the
/// heading is missing or not followed by `"\n\n"`, `Some("")` when it sits at the end of `doc`.
pub fn capture_heading_section<'a>(
    doc: &'a str,
    heading: &str,
    headings: &[String],
) -> Option<&'a str> {
    let start = doc.find(heading)? + heading.len();
    if start + 2 > doc.len() {
        return Some("");
    }
    if &doc.as_bytes()[start..start + 2] != b"\n\n" {
        return None;
    }
    // `start + 2` follows two ASCII bytes, so it is a char boundary.
    let content = &doc[start + 2..];
    let end = headings
        .iter()
        .filter(|other| other.as_str() != heading)
        .filter_map(|other| content.find(&format!("\n{other}")))
        .min()
        .unwrap_or(content.len());
    Some(&content[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = "#### Context\n\n<!-- Why? -->\n\n#### TL;DR\n\n*<!-- Short -->*\n\n#### Summary\n\n- <!-- Bullet -->\n\n#### Alternatives\n\n- <!-- Alternative -->\n\n#### Test Plan\n\n- [ ] <!-- Check -->\n";

    const VALID: &str = "#### Context\n\nContext text.\n\n#### TL;DR\n\nShort summary.\n\n#### Summary\n\n- First change.\n\n#### Alternatives\n\n- Considered nothing else.\n\n#### Test Plan\n\n- [x] Ran targeted checks.\n";

    struct Tmp {
        dir: tempfile::TempDir,
    }

    impl Tmp {
        fn new(template: Option<&str>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            if let Some(template) = template {
                std::fs::create_dir(dir.path().join(".github")).unwrap();
                std::fs::write(
                    dir.path().join(".github/pull_request_template.md"),
                    template,
                )
                .unwrap();
            }
            Self { dir }
        }

        fn body(&self, body: &str) -> &Self {
            std::fs::write(self.dir.path().join("pr_body.md"), body).unwrap();
            self
        }

        fn run(&self, args: &[&str]) -> Outcome {
            let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            run(&args, self.dir.path(), None)
        }

        fn check(&self) -> Outcome {
            self.run(&["lint", "--file", "pr_body.md"])
        }
    }

    #[test]
    fn prints_help() {
        let out = Tmp::new(None).run(&["--help", "--wat"]);
        assert_eq!(out.code, 0);
        assert!(
            out.stdout
                .contains("pr-body-check --file /path/to/pr_body.md")
        );
        assert_eq!(Tmp::new(None).run(&["-h"]).code, 0);
    }

    #[test]
    fn fails_on_invalid_options() {
        let out = Tmp::new(None).run(&["lint", "--wat"]);
        assert_eq!(out.code, 1);
        assert_eq!(out.stderr, "Invalid option(s): [{\"--wat\", nil}]\n");
        let out = Tmp::new(None).run(&["--file"]);
        assert!(
            out.stderr
                .contains("Invalid option(s): [{\"--file\", nil}]")
        );
    }

    #[test]
    fn fails_when_file_option_is_missing() {
        let out = Tmp::new(Some(TEMPLATE)).run(&["lint"]);
        assert_eq!(out.code, 1);
        assert!(out.stderr.contains("Missing required option --file"));
    }

    #[test]
    fn reads_the_body_from_pr_body_file() {
        let tmp = Tmp::new(Some(TEMPLATE));
        tmp.body(VALID);
        let out = run(&[], tmp.dir.path(), Some("pr_body.md".into()));
        assert_eq!(out.code, 0, "{out:?}");
        let out = run(&[], tmp.dir.path(), Some(" ".into()));
        assert!(out.stderr.contains("Missing required option --file"));
    }

    #[test]
    fn fails_when_template_is_missing() {
        let out = Tmp::new(None).run(&["--file", "pr_body.md"]);
        assert_eq!(out.code, 1);
        assert!(out.stderr.contains(
            "Unable to read PR template from any of: .github/pull_request_template.md, ../.github/pull_request_template.md"
        ));
    }

    #[test]
    fn finds_the_template_in_the_parent_directory_or_explicitly() {
        let tmp = Tmp::new(Some(TEMPLATE));
        let sub = tmp.dir.path().join("crate");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("pr_body.md"), VALID).unwrap();
        let out = run(&["--file=pr_body.md".into()], &sub, None);
        assert_eq!(out.code, 0, "{out:?}");
        std::fs::write(sub.join("t.md"), "#### Only\n\n- <!-- x -->\n").unwrap();
        std::fs::write(sub.join("b.md"), "#### Only\n\n- done\n").unwrap();
        let out = run(
            &[
                "--file".into(),
                "b.md".into(),
                "--template".into(),
                "t.md".into(),
            ],
            &sub,
            None,
        );
        assert_eq!(out.code, 0, "{out:?}");
        let out = run(
            &[
                "--file".into(),
                "b.md".into(),
                "--template".into(),
                "missing.md".into(),
            ],
            &sub,
            None,
        );
        assert!(
            out.stderr
                .starts_with("Unable to read PR template missing.md")
        );
    }

    #[test]
    fn fails_when_template_has_no_headings() {
        let tmp = Tmp::new(Some("no headings here"));
        tmp.body(VALID);
        let out = tmp.check();
        assert_eq!(out.code, 1);
        assert!(
            out.stderr
                .contains("No markdown headings found in .github/pull_request_template.md")
        );
    }

    #[test]
    fn fails_when_body_file_is_missing() {
        let out = Tmp::new(Some(TEMPLATE)).run(&["--file", "missing.md"]);
        assert_eq!(out.code, 1);
        assert!(out.stderr.starts_with("Unable to read missing.md"));
    }

    fn invalid(body: &str) -> String {
        let tmp = Tmp::new(Some(TEMPLATE));
        tmp.body(body);
        let out = tmp.check();
        assert_eq!(out.code, 1, "{out:?}");
        assert!(out.stderr.contains(
            "PR body format invalid. Read `.github/pull_request_template.md` and follow it precisely."
        ));
        out.stderr
    }

    #[test]
    fn fails_when_body_still_has_placeholders() {
        assert!(invalid(TEMPLATE).contains(
            "ERROR: PR description still contains template placeholder comments (<!-- ... -->)."
        ));
    }

    #[test]
    fn fails_when_heading_is_missing() {
        let body = VALID.replace("#### Alternatives\n\n- Considered nothing else.\n\n", "");
        assert!(invalid(&body).contains("ERROR: Missing required heading: #### Alternatives"));
    }

    #[test]
    fn fails_when_headings_are_out_of_order() {
        let body = "#### TL;DR\n\nShort summary.\n\n#### Context\n\nContext text.\n\n#### Summary\n\n- First change.\n\n#### Alternatives\n\n- Considered nothing else.\n\n#### Test Plan\n\n- [x] Ran targeted checks.\n";
        assert!(invalid(body).contains("ERROR: Required headings are out of order."));
    }

    #[test]
    fn fails_on_empty_section() {
        let body = VALID.replace("Context text.", "");
        assert!(invalid(&body).contains("ERROR: Section cannot be empty: #### Context"));
    }

    #[test]
    fn fails_when_a_middle_section_is_blank_before_the_next_heading() {
        let body = VALID.replace("- Considered nothing else.\n", "\n");
        assert!(invalid(&body).contains("ERROR: Section cannot be empty: #### Alternatives"));
    }

    #[test]
    fn fails_when_bullet_and_checkbox_expectations_are_not_met() {
        let body = VALID
            .replace("- First change.", "First change.")
            .replace("- Considered nothing else.", "Considered nothing else.")
            .replace("- [x] Ran targeted checks.", "Ran targeted checks.");
        let err = invalid(&body);
        for expected in [
            "Section must include at least one bullet item: #### Summary",
            "Section must include at least one bullet item: #### Alternatives",
            "Section must include at least one bullet item: #### Test Plan",
            "Section must include at least one checkbox item: #### Test Plan",
        ] {
            assert!(err.contains(expected), "{expected} missing from {err}");
        }
    }

    #[test]
    fn fails_when_heading_has_no_content_delimiter() {
        let err = invalid("#### Context\nContext text.");
        assert!(!err.contains("Section cannot be empty: #### Context"));
        assert!(err.contains("Missing required heading: #### TL;DR"));
    }

    #[test]
    fn fails_when_heading_appears_at_end_of_file() {
        assert!(invalid("#### Context").contains("Section cannot be empty: #### Context"));
    }

    #[test]
    fn passes_for_valid_body() {
        let tmp = Tmp::new(Some(TEMPLATE));
        tmp.body(VALID);
        let out = tmp.check();
        assert_eq!(
            out,
            Outcome {
                stdout: "PR body format OK\n".into(),
                stderr: String::new(),
                code: 0,
            }
        );
    }

    #[test]
    fn section_capture_edge_cases() {
        let headings = vec!["#### A".to_owned(), "#### B".to_owned()];
        assert_eq!(capture_heading_section("x", "#### A", &headings), None);
        assert_eq!(
            capture_heading_section("#### A", "#### A", &headings),
            Some("")
        );
        assert_eq!(
            capture_heading_section("#### A\n", "#### A", &headings),
            Some("")
        );
        assert_eq!(
            capture_heading_section("#### A\r\n\r\nx", "#### A", &headings),
            None
        );
        assert_eq!(
            capture_heading_section("#### A\n\n\n#### B\n\nb", "#### A", &headings),
            Some("")
        );
        // Any other heading ends the section, even one that comes first in the template.
        assert_eq!(
            capture_heading_section("#### B\n\nb\n#### A\n\na", "#### B", &headings),
            Some("b")
        );
        // `\s+` may span a newline, and CRLF headings keep their `\r` (Elixir parity).
        assert_eq!(
            extract_headings("####\nText\n"),
            vec!["####\nText".to_owned()]
        );
        assert_eq!(extract_headings("#### X\r\n"), vec!["#### X\r".to_owned()]);
        assert!(extract_headings("### Three\n####### Seven").is_empty());
    }

    #[test]
    fn the_repository_template_accepts_a_filled_in_body() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let template =
            std::fs::read_to_string(root.join(".github/pull_request_template.md")).unwrap();
        let headings = extract_headings(&template);
        assert_eq!(
            headings,
            [
                "#### Context",
                "#### TL;DR",
                "#### Summary",
                "#### Alternatives",
                "#### Test Plan"
            ]
        );
        let body = "#### Context\n\nWhy.\n\n#### TL;DR\n\n*What.*\n\n#### Summary\n\n- One.\n\n#### Alternatives\n\n- None.\n\n#### Test Plan\n\n- [x] `make all`\n";
        assert!(lint(&template, body, &headings).is_empty());
        assert!(!lint(&template, &template, &headings).is_empty());
    }
}
