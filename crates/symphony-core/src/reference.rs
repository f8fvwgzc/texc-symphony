//! Reference sections: progressive disclosure for `WORKFLOW.md` prompts.
//!
//! The design follows SkillReducer (Gao et al., "Optimizing LLM Agent Skills for Token Efficiency",
//! arXiv:2603.29919): most of a long agent instruction body is situational guidance that a given run
//! never needs, yet the turn-1 prompt stays in the thread and is re-sent with every model request of
//! the run. A prompt section wrapped in
//!
//! ```text
//! <!-- reference rework: when the ticket is in Rework -->
//! ...situational instructions...
//! <!-- /reference -->
//! ```
//!
//! is cut out of the turn-1 prompt and replaced by a one-line stub that names the section and when to
//! load it ([`stub`]). The agent loads the section on demand with the [`REFERENCE_TOOL`] dynamic tool.
//!
//! The whole template renders once: [`placeholder_template`] stands an unguessable placeholder line in
//! for each marker, and [`cut`] splits the rendered text at the placeholders. Liquid therefore behaves
//! as without markers (`assign` and `capture` reach across sections), the sections are moved rather
//! than rewritten, and issue text cannot fake a placeholder. A section that renders blank is dropped
//! with its stub.
//!
//! [`split`] checks the marker rules that need no Liquid: a marker is a whole line outside fenced code
//! blocks; `NAME` is 1-64 characters of `a-z`, `0-9`, `-` and `_` starting with a letter or digit; the
//! load condition after `: ` is required and must not contain Liquid; sections do not nest, names are
//! unique, and a body is not blank. A comment without the `reference NAME: ` shape
//! (`<!-- reference material below -->`, `<!-- reference https://... -->`) is ordinary prompt text;
//! `<!-- /reference -->` is reserved. A marker must also sit outside every Liquid block and tag
//! (`{% if %}`, `{% for %}`, `{% capture %}`, `{% raw %}`, `{% comment %}`, a multi-line `{{ ... }}`),
//! so that it renders exactly once; `crate::prompt::check_references` checks that with liquid-rust's
//! own parser when the workflow loads. [`cut`] still refuses a render whose placeholders did not come
//! out exactly once and in order, so nothing can split wrongly.

use std::hash::{BuildHasher, RandomState};

use serde_json::{Value, json};

/// Name of the dynamic tool that returns a reference section.
pub const REFERENCE_TOOL: &str = "workflow_reference";

/// Longest accepted section name.
pub const MAX_NAME_LEN: usize = 64;

const CLOSE_MARKER: &str = "/reference";

/// One reference section of a prompt template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Section name (the tool argument).
    pub name: String,
    /// When the agent should load it (shown in the stub).
    pub when: String,
    /// 1-based template line of the opening marker.
    pub line: usize,
    /// 1-based template line of the closing marker.
    pub end_line: usize,
}

/// A rendered reference section, served by the [`REFERENCE_TOOL`] tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// Section name.
    pub name: String,
    /// Rendered body.
    pub text: String,
}

/// A malformed reference marker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("line {line}: {message}")]
pub struct ReferenceSyntaxError {
    /// 1-based line of the offending marker (template line from [`split`]; see [`Self::offset`]).
    pub line: usize,
    /// What is wrong.
    pub message: String,
}

impl ReferenceSyntaxError {
    /// The same error with `line` moved to a file line, given the file line of template line 1.
    pub fn offset(mut self, first_line: usize) -> Self {
        self.line += first_line.max(1) - 1;
        self
    }
}

enum Marker<'a> {
    Open(&'a str, &'a str),
    Close,
}

/// A marker line, or `None` for any other line (including comments of another shape).
fn marker(line: &str) -> Option<Marker<'_>> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim();
    if inner == CLOSE_MARKER {
        return Some(Marker::Close);
    }
    let rest = inner.strip_prefix("reference")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    // The name ends at the first `:` followed by whitespace or the end (so `https://` is not one).
    let colon = rest
        .match_indices(':')
        .map(|(index, _)| index)
        .find(|index| {
            rest[index + 1..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
        })?;
    let name = rest[..colon].trim();
    (!name.is_empty() && !name.contains(char::is_whitespace))
        .then(|| Marker::Open(name, rest[colon + 1..].trim()))
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name.len() <= MAX_NAME_LEN
}

fn check_open(name: &str, when: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!(
            "invalid reference name {name:?}: use 1-{MAX_NAME_LEN} of a-z, 0-9, '-' and '_', starting with a letter or digit"
        ));
    }
    if when.is_empty() {
        return Err(format!("reference `{name}` has an empty load condition"));
    }
    if when.contains("{{") || when.contains("{%") {
        return Err(format!(
            "the load condition of reference `{name}` must not contain Liquid"
        ));
    }
    Ok(())
}

/// The fence that opens a fenced code block on `line` (`(char, run length)`): three or more
/// backticks or tildes after any indentation (a fence nested in a list item is indented), and, as in
/// CommonMark, no backtick in the info string of a backtick fence.
pub fn fence_opener(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let marker = trimmed.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = trimmed.chars().take_while(|c| *c == marker).count();
    let info = &trimmed[run..];
    (run >= 3 && !(marker == '`' && info.contains('`'))).then_some((marker, run))
}

/// Whether `line` closes a fenced block opened by `opener` (same character, at least as long, nothing
/// else on the line).
pub fn closes_fence(line: &str, (marker, run): (char, usize)) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= run && trimmed.chars().all(|c| c == marker)
}

/// Drops leading and trailing whitespace-only lines.
fn trim_blank_lines(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let blank = |line: &&str| line.trim().is_empty();
    let start = lines.iter().position(|l| !blank(l)).unwrap_or(lines.len());
    let end = lines
        .iter()
        .rposition(|l| !blank(l))
        .map_or(start, |i| i + 1);
    lines[start..end].join("\n")
}

/// The core-prompt line that stands in for a section.
pub fn stub(name: &str, when: &str) -> String {
    let stop = if when.ends_with(['.', '!', '?']) {
        ""
    } else {
        "."
    };
    format!("Reference `{name}`: call the `{REFERENCE_TOOL}` tool with name `{name}` {when}{stop}")
}

/// Finds the reference sections of `template` (an LF-joined prompt body), checking the marker rules
/// from the module docs. Line numbers are template lines.
pub fn split(template: &str) -> Result<Vec<Section>, ReferenceSyntaxError> {
    let error = |line: usize, message: String| ReferenceSyntaxError { line, message };
    let mut sections: Vec<Section> = Vec::new();
    // (name, when, opening line, whether a non-blank body line was seen) of the open section.
    let mut open: Option<(String, String, usize, bool)> = None;
    let mut fence: Option<(char, usize)> = None;
    for (index, line) in template.split('\n').enumerate() {
        let number = index + 1;
        let found = match fence {
            Some(opener) => {
                if closes_fence(line, opener) {
                    fence = None;
                }
                None
            }
            None => {
                fence = fence_opener(line);
                marker(line)
            }
        };
        match found {
            Some(Marker::Open(name, when)) => {
                if let Some((open_name, ..)) = &open {
                    return Err(error(
                        number,
                        format!("reference sections do not nest (`{open_name}` is still open)"),
                    ));
                }
                check_open(name, when).map_err(|message| error(number, message))?;
                if sections.iter().any(|s| s.name == name) {
                    return Err(error(number, format!("duplicate reference name `{name}`")));
                }
                open = Some((name.to_owned(), when.to_owned(), number, false));
            }
            Some(Marker::Close) => {
                let Some((name, when, line, filled)) = open.take() else {
                    return Err(error(
                        number,
                        "`<!-- /reference -->` without an open reference (open one with `<!-- reference NAME: WHEN -->`)".into(),
                    ));
                };
                if !filled {
                    return Err(error(line, format!("reference `{name}` is empty")));
                }
                sections.push(Section {
                    name,
                    when,
                    line,
                    end_line: number,
                });
            }
            None => {
                if let Some((.., filled)) = open.as_mut() {
                    *filled |= !line.trim().is_empty();
                }
            }
        }
    }
    if let Some((name, _, line, _)) = open {
        return Err(error(
            line,
            format!("reference `{name}` is not closed with `<!-- /reference -->`"),
        ));
    }
    Ok(sections)
}

/// Placeholders standing in for the markers of one render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placeholders {
    nonce: u64,
}

impl Placeholders {
    /// Fresh, unguessable placeholders (random per call).
    pub fn new() -> Self {
        Self {
            nonce: RandomState::new().hash_one(0u8),
        }
    }

    fn open(self, index: usize) -> String {
        format!("<<symphony-reference:{:016x}:{index}:open>>", self.nonce)
    }

    fn close(self, index: usize) -> String {
        format!("<<symphony-reference:{:016x}:{index}:close>>", self.nonce)
    }
}

impl Default for Placeholders {
    fn default() -> Self {
        Self::new()
    }
}

/// `template` with the marker lines of `sections` (from [`split`] of the same template) replaced by
/// `placeholders`, ready to render as one Liquid template.
pub fn placeholder_template(
    template: &str,
    sections: &[Section],
    placeholders: Placeholders,
) -> String {
    let mut lines: Vec<String> = template.split('\n').map(str::to_owned).collect();
    for (index, section) in sections.iter().enumerate() {
        for (number, text) in [
            (section.line, placeholders.open(index)),
            (section.end_line, placeholders.close(index)),
        ] {
            if let Some(line) = number.checked_sub(1).and_then(|i| lines.get_mut(i)) {
                *line = text;
            }
        }
    }
    lines.join("\n")
}

/// Splits `rendered` (the render of [`placeholder_template`]) into the core prompt, with each section
/// replaced by its [`stub`], and the sections. A section that renders blank is dropped with its stub.
/// `None` unless every placeholder occurs exactly once and in order, which holds when the markers sit
/// outside Liquid blocks and tags.
pub fn cut(
    rendered: &str,
    sections: &[Section],
    placeholders: Placeholders,
) -> Option<(String, Vec<Reference>)> {
    let mut core = String::with_capacity(rendered.len());
    let mut references = Vec::with_capacity(sections.len());
    let mut rest = rendered;
    for (index, section) in sections.iter().enumerate() {
        let (open, close) = (placeholders.open(index), placeholders.close(index));
        if rendered.matches(&open).count() != 1 || rendered.matches(&close).count() != 1 {
            return None;
        }
        let (before, after_open) = rest.split_once(open.as_str())?;
        let (body, after_close) = after_open.split_once(close.as_str())?;
        let text = trim_blank_lines(body);
        core.push_str(before);
        if text.is_empty() {
            // Keep one line break where the stub would have been.
            rest = match (before.ends_with('\n'), after_close.strip_prefix('\n')) {
                (true, Some(tail)) => tail,
                _ => after_close,
            };
            continue;
        }
        core.push_str(&stub(&section.name, &section.when));
        references.push(Reference {
            name: section.name.clone(),
            text,
        });
        rest = after_close;
    }
    core.push_str(rest);
    Some((core, references))
}

/// The dynamic tools of a run: `tracker_specs` plus the [`REFERENCE_TOOL`] spec when there are
/// sections (`names`).
pub fn with_reference_tool(mut tracker_specs: Vec<Value>, names: &[&str]) -> Vec<Value> {
    if !names.is_empty() {
        tracker_specs.push(json!({
            "name": REFERENCE_TOOL,
            "description": "Returns a reference section of the workflow instructions. The prompt names each section and when to load it.",
            "inputSchema": {
                "type": "object",
                "properties": {"name": {"type": "string", "enum": names}},
                "required": ["name"],
                "additionalProperties": false,
            },
        }));
    }
    tracker_specs
}

/// Finds the section named by tool `arguments` (`{"name": "..."}`). The error is the failure payload
/// to return to the agent (it lists the available names).
pub fn lookup<'a>(references: &'a [Reference], arguments: &Value) -> Result<&'a Reference, Value> {
    let names: Vec<&str> = references.iter().map(|r| r.name.as_str()).collect();
    let message = match arguments.get("name").and_then(Value::as_str) {
        Some(name) => match references.iter().find(|r| r.name == name) {
            Some(found) => return Ok(found),
            None => format!("Unknown workflow reference: {}.", Value::from(name)),
        },
        None => "Missing `name` argument.".to_owned(),
    };
    Err(json!({"error": {"message": message, "references": names}}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(template: &str) -> ReferenceSyntaxError {
        split(template).unwrap_err()
    }

    fn names(template: &str) -> Vec<String> {
        split(template)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// `cut` of the unrendered placeholder template: the core and the raw section bodies.
    fn layout(template: &str) -> (String, Vec<Reference>) {
        let sections = split(template).unwrap();
        let placeholders = Placeholders::new();
        cut(
            &placeholder_template(template, &sections, placeholders),
            &sections,
            placeholders,
        )
        .unwrap()
    }

    #[test]
    fn comments_of_another_shape_are_prompt_text() {
        let template = "Intro\n\n<!-- a plain comment -->\n<!-- reference material below -->\n<!-- reference: https://example.org -->\n<!-- reference https://example.org/runbook -->\n<!-- references: x -->\n<!-- reference x:y -->\nEnd";
        assert!(names(template).is_empty());
        assert_eq!(layout(template).0, template);
    }

    #[test]
    fn sections_move_out_of_the_core_and_leave_a_stub() {
        let template = "Intro\n<!-- reference rework: when the ticket is in Rework -->\n\n  - step one\n- step two\n\n<!-- /reference -->\nOutro";
        assert_eq!(
            split(template).unwrap(),
            vec![Section {
                name: "rework".into(),
                when: "when the ticket is in Rework".into(),
                line: 2,
                end_line: 7,
            }]
        );
        let (core, references) = layout(template);
        assert_eq!(
            core,
            "Intro\nReference `rework`: call the `workflow_reference` tool with name `rework` when the ticket is in Rework.\nOutro"
        );
        assert_eq!(references[0].text, "  - step one\n- step two");
    }

    #[test]
    fn markers_tolerate_spacing_and_keep_terminal_punctuation() {
        let template = "  <!--reference land-1:   before merging!   -->\nx\n<!--/reference-->";
        assert_eq!(names(template), vec!["land-1"]);
        assert_eq!(
            layout(template).0,
            "Reference `land-1`: call the `workflow_reference` tool with name `land-1` before merging!"
        );
    }

    #[test]
    fn markers_inside_fenced_code_are_prompt_text() {
        let template = "```md\n<!-- reference demo: when shown -->\nexample\n<!-- /reference -->\n```\n<!-- reference real: when needed -->\n````md\n<!-- /reference -->\n````\n<!-- /reference -->";
        assert_eq!(names(template), vec!["real"]);
        assert_eq!(
            layout(template).1[0].text,
            "````md\n<!-- /reference -->\n````"
        );
        // An unclosed fence runs to the end, so the close marker is swallowed.
        assert!(
            err("<!-- reference a: w -->\n```\nx\n<!-- /reference -->")
                .message
                .contains("not closed")
        );
        // A fence nested in a list item is indented.
        let listed = "1.  Show the syntax:\n    ```md\n    <!-- reference demo: when shown -->\n    x\n    <!-- /reference -->\n    ```\n2.  Done.";
        assert!(names(listed).is_empty());
        // Not fences: inline code at the start of a line, too few backticks.
        for line in ["```make all``` before pushing.", "``x"] {
            assert!(fence_opener(line).is_none(), "{line:?}");
            let template = format!("{line}\n<!-- reference a: w -->\nx\n<!-- /reference -->");
            assert_eq!(names(&template), vec!["a"], "{line:?}");
        }
        assert_eq!(fence_opener("   ~~~~ rust"), Some(('~', 4)));
        assert!(closes_fence("`````", ('`', 4)));
        assert!(!closes_fence("```", ('`', 4)));
        assert!(!closes_fence("```text", ('`', 3)));
    }

    #[test]
    fn malformed_markers_are_rejected_with_their_line() {
        let cases = [
            (
                "<!-- reference Big: when -->\nx\n<!-- /reference -->",
                1,
                "invalid reference name",
            ),
            (
                "<!-- reference ą: when -->\nx\n<!-- /reference -->",
                1,
                "invalid reference name",
            ),
            (
                "<!-- reference a: -->\nx\n<!-- /reference -->",
                1,
                "empty load condition",
            ),
            (
                "<!-- reference a: if {{ attempt }} -->\nx\n<!-- /reference -->",
                1,
                "must not contain Liquid",
            ),
            ("x\n<!-- /reference -->", 2, "without an open reference"),
            (
                "<!-- reference rework -->\nx\n<!-- /reference -->",
                3,
                "without an open reference",
            ),
            (
                "<!-- reference a: w -->\n<!-- reference b: w -->",
                2,
                "do not nest",
            ),
            ("<!-- reference a: w -->\nbody", 1, "not closed"),
            (
                "<!-- reference a: w -->\n  \n<!-- /reference -->",
                1,
                "is empty",
            ),
            (
                "<!-- reference a: w -->\nx\n<!-- /reference -->\n<!-- reference a: w -->\ny\n<!-- /reference -->",
                4,
                "duplicate reference name",
            ),
        ];
        for (template, line, message) in cases {
            let error = err(template);
            assert_eq!(error.line, line, "{template:?}");
            assert!(error.message.contains(message), "{template:?}: {error}");
        }
        let long = format!(
            "<!-- reference {}: w -->\nx\n<!-- /reference -->",
            "a".repeat(65)
        );
        assert!(err(&long).message.contains("invalid reference name"));
        assert_eq!(err("x\n<!-- /reference -->").offset(10).line, 11);
        assert_eq!(err("x\n<!-- /reference -->").offset(0).line, 2);
    }

    #[test]
    fn rendered_sections_are_cut_at_the_placeholders() {
        let template = "Core\n<!-- reference a: when A -->\nbody a\n<!-- /reference -->\nMiddle\n<!-- reference b: when B -->\nbody b\n<!-- /reference -->\nEnd";
        let sections = split(template).unwrap();
        let placeholders = Placeholders::new();
        let prepared = placeholder_template(template, &sections, placeholders);
        assert!(!prepared.contains("<!-- reference"));
        // Stand-in for a render that joined lines (`{%-` whitespace control) around a placeholder.
        let rendered = prepared.replace("\nMiddle\n", "Middle");
        let (core, references) = cut(&rendered, &sections, placeholders).unwrap();
        assert_eq!(
            core,
            format!(
                "Core\n{}Middle{}\nEnd",
                stub("a", "when A"),
                stub("b", "when B")
            )
        );
        assert_eq!(
            references,
            vec![
                Reference {
                    name: "a".into(),
                    text: "body a".into()
                },
                Reference {
                    name: "b".into(),
                    text: "body b".into()
                },
            ]
        );
        // A missing, repeated or reordered placeholder is refused.
        let open = placeholders.open(1);
        assert!(cut(&rendered.replace(&open, ""), &sections, placeholders).is_none());
        assert!(cut(&format!("{rendered}{open}"), &sections, placeholders).is_none());
        let reordered = format!("{open}{}", rendered.replace(&open, ""));
        assert!(cut(&reordered, &sections, placeholders).is_none());
        assert_ne!(
            Placeholders::new(),
            placeholders,
            "fresh placeholders per render"
        );
    }

    #[test]
    fn a_section_that_renders_blank_is_dropped_with_its_stub() {
        let template = "Core\n<!-- reference a: when A -->\nbody\n<!-- /reference -->\nEnd";
        let sections = split(template).unwrap();
        let placeholders = Placeholders::new();
        let rendered = placeholder_template(template, &sections, placeholders).replace("body", " ");
        let (core, references) = cut(&rendered, &sections, placeholders).unwrap();
        assert_eq!(core, "Core\nEnd");
        assert!(references.is_empty());
    }

    #[test]
    fn lookup_serves_known_names_and_lists_them_otherwise() {
        let references = vec![Reference {
            name: "land".into(),
            text: "Land it.".into(),
        }];
        assert_eq!(
            lookup(&references, &json!({"name": "land"})).unwrap().text,
            "Land it."
        );
        assert_eq!(
            lookup(&references, &json!({"name": "nope"})).unwrap_err(),
            json!({"error": {"message": "Unknown workflow reference: \"nope\".", "references": ["land"]}})
        );
        for arguments in [json!({}), json!("land"), json!({"name": 3})] {
            assert_eq!(
                lookup(&references, &arguments).unwrap_err()["error"]["message"],
                "Missing `name` argument."
            );
        }
    }

    #[test]
    fn the_reference_tool_is_added_only_with_sections() {
        let tracker = vec![json!({"name": "linear_graphql"})];
        assert_eq!(with_reference_tool(tracker.clone(), &[]), tracker);
        let specs = with_reference_tool(tracker, &["a", "b"]);
        assert_eq!(specs[1]["name"], REFERENCE_TOOL);
        assert_eq!(
            specs[1]["inputSchema"]["properties"]["name"]["enum"],
            json!(["a", "b"])
        );
        assert_eq!(specs[1]["inputSchema"]["required"], json!(["name"]));
    }
}
