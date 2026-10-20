//! `symphony workflow audit`: a build-time token audit of a `WORKFLOW.md` prompt.
//!
//! The prompt and the tool specs sit in the agent's context for every model request of a run, so
//! their size multiplies by the number of requests; provider prompt caching bills those repeats at a
//! discount but they still fill the context window. Following SkillReducer (arXiv:2603.29919), the
//! audit separates what is always in context from what [`symphony_core::reference`] sections load on
//! demand and points at the usual bloat: big fenced templates and examples in the core, sections too
//! small to be worth a tool call, and a long prompt with no sections at all.
//!
//! Token counts are estimates (about four characters per token) of the template text: Symphony ships
//! no tokenizer, every Liquid branch is counted, and issue fields such as the description add their
//! own size per issue. The audit is for comparing sections and prompt versions, not for billing.

use std::path::Path;

use serde_json::Value;
use symphony_core::reference::{self, REFERENCE_TOOL, closes_fence, fence_opener};
use symphony_core::{ConfigError, LoadedWorkflow, prompt, workflow};
use symphony_trackers::{TrackerDeps, tracker_for_kind};

/// A fenced block in the core at or above this estimate is reported.
pub const FENCE_FINDING_TOKENS: usize = 40;
/// A reference section below this estimate is reported as not worth a tool call.
pub const SMALL_REFERENCE_TOKENS: usize = 30;
/// A prompt without reference sections at or above this estimate is reported.
pub const LARGE_CORE_TOKENS: usize = 1_500;
/// How many core sections the report lists.
const LARGEST_SECTIONS: usize = 5;

/// Estimated tokens of `text` (about four characters per token, rounded up).
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// `"1 token"`, `"1,200 tokens"`.
fn tokens(value: usize) -> String {
    if value == 1 {
        "1 token".to_owned()
    } else {
        format!("{} tokens", thousands(value))
    }
}

fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// A Markdown ATX heading: up to three spaces, one to six `#`, then a space or the end of the line.
fn is_heading(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    line.len() - trimmed.len() <= 3
        && (1..=6).contains(&hashes)
        && trimmed[hashes..].chars().next().is_none_or(|c| c == ' ')
}

/// The tracker's agent tool specs for the workflow's `tracker.kind` (`Err` explains why none).
pub fn tracker_tool_specs(workflow: &LoadedWorkflow) -> Result<Vec<Value>, String> {
    let kind = workflow
        .config
        .get("tracker")
        .and_then(|tracker| tracker.get("kind"))
        .and_then(Value::as_str)
        .ok_or("tracker.kind is not set")?;
    let deps = TrackerDeps::new().map_err(|e| e.to_string())?;
    tracker_for_kind(kind, &deps)
        .map(|tracker| tracker.agent_tool_specs())
        .map_err(|e| e.to_string())
}

/// Audits a loaded workflow. `tool_specs` are the tracker's agent tools (`Err`: why they could not be
/// counted). The error is the message startup would print for a malformed reference marker.
pub fn audit(
    workflow: &LoadedWorkflow,
    tool_specs: Result<&[Value], &str>,
) -> Result<String, String> {
    let default_prompt = workflow.prompt_template.trim().is_empty();
    let template = prompt::workflow_prompt(workflow);
    if !default_prompt {
        prompt::check_references(template, workflow.prompt_line)
            .map_err(|message| ConfigError::WorkflowParseError(message).user_message())?;
    }
    let sections = reference::split(template).map_err(|e| e.to_string())?;
    let placeholders = reference::Placeholders::new();
    let (core, bodies) = reference::cut(
        &reference::placeholder_template(template, &sections, placeholders),
        &sections,
        placeholders,
    )
    .ok_or("reference sections could not be located")?;
    let file_line = |template_line: usize| template_line + workflow.prompt_line.max(1) - 1;
    let in_section = |number: usize| {
        sections
            .iter()
            .any(|s| s.line <= number && number <= s.end_line)
    };

    // Core sections by heading, and fenced blocks in the core (an unclosed fence runs to the end).
    let mut headings: Vec<(usize, String, usize)> = Vec::new();
    let mut fences: Vec<(usize, usize)> = Vec::new();
    // (opening line, characters so far, opener) of the fenced block being read.
    let mut fence: Option<(usize, usize, (char, usize))> = None;
    for (index, line) in template.split('\n').enumerate() {
        let number = index + 1;
        if in_section(number) {
            continue;
        }
        let chars = line.chars().count() + 1;
        let mut in_fence = true;
        if let Some((start, size, opener)) = fence.as_mut() {
            *size += chars;
            if closes_fence(line, *opener) {
                fences.push((*start, *size));
                fence = None;
            }
        } else if let Some(opener) = fence_opener(line) {
            fence = Some((number, chars, opener));
        } else {
            in_fence = false;
        }
        if !in_fence && is_heading(line) {
            headings.push((number, line.trim().to_owned(), 0));
        } else if headings.is_empty() {
            headings.push((number, "(before the first heading)".to_owned(), 0));
        }
        if let Some(last) = headings.last_mut() {
            last.2 += chars;
        }
    }
    fences.extend(fence.map(|(start, size, _)| (start, size)));

    let names: Vec<&str> = sections.iter().map(|s| s.name.as_str()).collect();
    let specs = reference::with_reference_tool(
        tool_specs.map(<[Value]>::to_vec).unwrap_or_default(),
        &names,
    );
    let spec_tokens: usize = specs
        .iter()
        .map(|spec| estimate_tokens(&spec.to_string()))
        .sum();
    let spec_names: Vec<&str> = specs
        .iter()
        .filter_map(|spec| spec.get("name").and_then(Value::as_str))
        .collect();
    let core_tokens = estimate_tokens(&core);
    let on_demand: Vec<usize> = bodies.iter().map(|b| estimate_tokens(&b.text)).collect();
    let on_demand_total: usize = on_demand.iter().sum();
    let liquid_in_core = core.contains("{%");
    let width = names
        .iter()
        .map(|n| n.len())
        .chain([11])
        .max()
        .unwrap_or(11);

    let mut out = String::new();
    let mut line = |text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line("Workflow prompt audit".into());
    if default_prompt {
        line("The prompt body is blank, so the default prompt is audited.".into());
    }
    line("Token counts are estimates (about 4 characters per token) of the template text.".into());
    line(String::new());
    line("In context for every model request of a run:".into());
    line(format!(
        "  {:<width$} {:>7} tokens",
        "core prompt",
        thousands(core_tokens)
    ));
    match tool_specs {
        Ok(_) => line(format!(
            "  {:<width$} {:>7} tokens  {}",
            "tool specs",
            thousands(spec_tokens),
            if spec_names.is_empty() {
                "(none)".to_owned()
            } else {
                spec_names.join(", ")
            }
        )),
        Err(reason) => line(format!(
            "  {:<width$} {:>7} tokens  (tracker tools not counted: {reason})",
            "tool specs",
            thousands(spec_tokens)
        )),
    }
    line(format!(
        "  {:<width$} {:>7} tokens",
        "total",
        thousands(core_tokens + spec_tokens)
    ));
    if liquid_in_core {
        line(
            "  The core has Liquid tags; every branch is counted, so one run can get less.".into(),
        );
    }

    if !sections.is_empty() {
        line(String::new());
        line(format!("Loaded on demand with the {REFERENCE_TOOL} tool:"));
        for (section, estimate) in sections.iter().zip(&on_demand) {
            line(format!(
                "  {:<width$} {:>7} tokens  line {}  {}",
                section.name,
                thousands(*estimate),
                file_line(section.line),
                section.when
            ));
        }
        let share = on_demand_total * 100 / (on_demand_total + core_tokens).max(1);
        line(format!(
            "  {:<width$} {:>7} tokens  ({share}% of the prompt is out of the core)",
            "total",
            thousands(on_demand_total)
        ));
    }

    let mut largest = headings.clone();
    largest.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    largest.truncate(LARGEST_SECTIONS);
    if largest.len() > 1 {
        line(String::new());
        line("Largest core sections:".into());
        for (number, title, chars) in &largest {
            line(format!(
                "  {:>7} tokens  line {}  {title}",
                thousands(chars.div_ceil(4)),
                file_line(*number)
            ));
        }
    }

    let mut findings: Vec<String> = Vec::new();
    if sections.is_empty() && core_tokens >= LARGE_CORE_TOKENS {
        findings.push(format!(
            "The prompt has no reference sections, so up to {} of template text are re-sent with every model request. Move situational instructions (per-status flows, escape hatches, templates) into `<!-- reference NAME: WHEN -->` sections.",
            tokens(core_tokens)
        ));
    }
    for (number, chars) in &fences {
        let estimate = chars.div_ceil(4);
        if estimate >= FENCE_FINDING_TOKENS {
            findings.push(format!(
                "Line {}: a fenced block of about {} is in the core prompt. Templates and examples are usually needed at one step only, so a reference section keeps them out of every request.",
                file_line(*number),
                tokens(estimate)
            ));
        }
    }
    for (section, estimate) in sections.iter().zip(&on_demand) {
        if *estimate < SMALL_REFERENCE_TOKENS {
            findings.push(format!(
                "Reference `{}` (line {}) is about {}, about what its stub and the tool call cost. Inline it.",
                section.name,
                file_line(section.line),
                tokens(*estimate)
            ));
        }
    }

    line(String::new());
    if findings.is_empty() {
        line("No findings.".into());
    } else {
        line("Findings:".into());
        for finding in findings {
            line(format!("- {finding}"));
        }
    }
    Ok(out)
}

/// Loads and audits the workflow at `path` (tracker tool specs from its `tracker.kind`). Read and
/// parse failures carry the message startup would print.
pub fn audit_file(path: &Path) -> Result<String, String> {
    let workflow = workflow::load(path).map_err(|e| e.user_message())?;
    let specs = tracker_tool_specs(&workflow);
    let report = audit(&workflow, specs.as_deref().map_err(String::as_str))?;
    Ok(format!("{}\n{report}", path.display()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const FRONT: &str = "---\ntracker:\n  kind: memory\n---\n";

    fn audit_text(content: &str, specs: Result<&[Value], &str>) -> Result<String, String> {
        audit(&workflow::parse(content).unwrap(), specs)
    }

    fn report(body: &str) -> String {
        audit_text(&format!("{FRONT}{body}"), Ok(&[])).unwrap()
    }

    #[test]
    fn estimates_round_up_and_count_characters() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("ąąąą"), 1);
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(thousands(12), "12");
        assert_eq!(tokens(1), "1 token");
        assert_eq!(tokens(1_500), "1,500 tokens");
    }

    #[test]
    fn headings_follow_markdown() {
        for heading in ["# A", "###### A", "   ## A", "#"] {
            assert!(is_heading(heading), "{heading:?}");
        }
        for text in ["#1 rule: be brief", "####### A", "    # A", "#hashtag"] {
            assert!(!is_heading(text), "{text:?}");
        }
    }

    #[test]
    fn sections_are_listed_with_file_lines_and_the_tool_is_counted() {
        let body = format!(
            "# Intro\nShort core.\n\n<!-- reference rework: when the ticket is in Rework -->\n{}\n<!-- /reference -->\n",
            "Address each review comment, rerun the checks and update the workpad. ".repeat(4)
        );
        let text = report(&body);
        assert!(
            text.contains("Loaded on demand with the workflow_reference tool:"),
            "{text}"
        );
        // The opening marker is file line 8 (four front-matter lines, then prompt line 4).
        assert!(
            text.contains("line 8  when the ticket is in Rework"),
            "{text}"
        );
        assert!(text.contains("tokens  workflow_reference"), "{text}");
        assert!(
            text.contains("% of the prompt is out of the core)"),
            "{text}"
        );
        assert!(text.contains("No findings."), "{text}");
        assert!(!text.contains("Liquid tags"), "{text}");
    }

    #[test]
    fn bloat_is_reported() {
        let body = format!(
            "# Core\nAlways update the workpad.\n\n```md\n{}\n```\n\n<!-- reference tiny: when needed -->\nOk.\n<!-- /reference -->\n",
            "template line with plenty of placeholder text\n".repeat(6)
        );
        let text = report(&body);
        assert!(text.contains("Line 8: a fenced block of about"), "{text}");
        assert!(
            text.contains("Reference `tiny` (line 18) is about 1 token,"),
            "{text}"
        );
    }

    #[test]
    fn the_preamble_starts_at_the_first_core_line_and_text_is_not_a_heading() {
        let body = format!(
            "<!-- reference a: when needed -->\n{}\n<!-- /reference -->\n{}#1 rule: be brief\n# Heading\nx\n",
            "Section text that is long enough to keep. ".repeat(3),
            "Core text before any heading. ".repeat(20)
        );
        let text = report(&body);
        assert!(
            text.contains("line 8  (before the first heading)"),
            "{text}"
        );
        assert!(text.contains("line 9  # Heading"), "{text}");
        assert!(!text.contains("#1 rule"), "{text}");
    }

    #[test]
    fn an_unclosed_fence_is_reported_to_the_end_of_the_prompt() {
        let body = format!(
            "# Core\n```text\n{}",
            "unterminated template line\n".repeat(12)
        );
        assert!(report(&body).contains("Line 6: a fenced block of about"));
    }

    #[test]
    fn nested_fences_count_as_one_block_from_the_outer_opener() {
        let body = format!(
            "# Template\n````md\n## Workpad\n```text\nhost\n```\n{}````\nAfter.\n",
            "- [ ] a checklist item with enough words to matter\n".repeat(4)
        );
        let text = report(&body);
        assert!(text.contains("Line 6: a fenced block of about"), "{text}");
        assert_eq!(text.matches("a fenced block").count(), 1, "{text}");
    }

    #[test]
    fn a_long_prompt_without_sections_is_reported_and_liquid_is_flagged() {
        let body = format!(
            "{{% if attempt %}}Resume.{{% endif %}}\n{}",
            "Do the work carefully and report blockers only.\n".repeat(150)
        );
        let text = report(&body);
        assert!(text.contains("so up to 1,"), "{text}");
        assert!(text.contains("every branch is counted"), "{text}");
        assert!(!text.contains("Loaded on demand"), "{text}");
    }

    #[test]
    fn tool_specs_and_unavailable_trackers_are_reported() {
        let specs = [json!({"name": "linear_graphql", "description": "x", "inputSchema": {}})];
        let text = audit_text(&format!("{FRONT}Body"), Ok(&specs)).unwrap();
        assert!(text.contains("tokens  linear_graphql"), "{text}");
        let text = audit_text(&format!("{FRONT}Body"), Err("tracker.kind is not set")).unwrap();
        assert!(
            text.contains("(tracker tools not counted: tracker.kind is not set)"),
            "{text}"
        );
        let blank = audit_text(FRONT, Ok(&[])).unwrap();
        assert!(blank.contains("the default prompt is audited"), "{blank}");
    }

    #[test]
    fn malformed_sections_fail_with_the_startup_message_and_file_line() {
        let error = audit_text(
            &format!("{FRONT}\n<!-- reference x: when -->\nbody\n"),
            Ok(&[]),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Failed to parse WORKFLOW.md: invalid prompt reference: line 6: reference `x` is not closed with `<!-- /reference -->`"
        );
        let crlf =
            "---\r\ntracker:\r\n  kind: memory\r\n---\r\n\r\nIntro\r\n<!-- /reference -->\r\n";
        assert!(audit_text(crlf, Ok(&[])).unwrap_err().contains("line 7:"));
    }

    #[test]
    fn unreadable_files_fail_like_startup() {
        let dir = std::env::temp_dir().join(format!("symphony-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.md");
        std::fs::write(&path, b"---\n---\nIntro \xff\xfe\n").unwrap();
        assert_eq!(
            audit_file(&path).unwrap_err(),
            "Failed to parse WORKFLOW.md: workflow file is not valid UTF-8"
        );
        assert!(
            audit_file(&dir.join("missing.md"))
                .unwrap_err()
                .starts_with("Missing WORKFLOW.md at")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tracker_specs_follow_the_kind() {
        let linear = workflow::parse("---\ntracker:\n  kind: linear\n---\nx").unwrap();
        assert_eq!(
            tracker_tool_specs(&linear).unwrap()[0]["name"],
            "linear_graphql"
        );
        assert!(tracker_tool_specs(&workflow::parse("---\n{}\n---\nx").unwrap()).is_err());
    }
}
