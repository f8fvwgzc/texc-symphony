//! Prompt rendering (`SymphonyElixir.PromptBuilder`) with strict Liquid semantics.
//!
//! The template sees exactly two top-level variables: `attempt` (integer or nil) and `issue` (every
//! [`Issue`] field, nil when unset; datetimes as ISO-8601 `Z` strings). Unknown variables and filters fail.
//!
//! Reference sections ([`crate::reference`]) are cut out of the rendered turn-1 prompt and served on
//! demand; [`render_run_prompt`] returns both.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use liquid::model::{Object, Value as LiquidValue};
use liquid_core::parser::{
    BlockReflection, ParseBlock, ParseTag, TagBlock, TagReflection, TagTokenIter,
};
use liquid_core::runtime::{Renderable, Runtime};
use liquid_core::{Language, Result as LiquidResult};
use serde_json::Value;

use crate::error::ConfigError;
use crate::issue::{Issue, format_datetime};
use crate::reference::{self, Reference};
use crate::workflow::LoadedWorkflow;

/// Template used when the workflow body is blank (`Config @default_prompt_template`).
pub const DEFAULT_PROMPT_TEMPLATE: &str = "You are working on an issue from the configured tracker.

Identifier: {{ issue.identifier }}
Title: {{ issue.title }}

Body:
{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}
";

/// Prompt building failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// The workflow could not be loaded at all.
    #[error("workflow_unavailable: {0}")]
    WorkflowUnavailable(String),
    /// The template did not parse.
    #[error("template_parse_error: {message} template={template:?}")]
    TemplateParse {
        /// Parser message.
        message: String,
        /// The offending template.
        template: String,
    },
    /// Rendering failed (unknown variable, unknown filter, ...).
    #[error("template_render_error: {0}")]
    TemplateRender(String),
}

/// `Config.workflow_prompt/0`: the workflow body, or [`DEFAULT_PROMPT_TEMPLATE`] when it is blank.
pub fn workflow_prompt(workflow: &LoadedWorkflow) -> &str {
    if workflow.prompt_template.trim().is_empty() {
        DEFAULT_PROMPT_TEMPLATE
    } else {
        &workflow.prompt_template
    }
}

fn json_to_liquid(value: &Value) -> LiquidValue {
    match value {
        Value::Null => LiquidValue::Nil,
        Value::Bool(b) => LiquidValue::scalar(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => LiquidValue::scalar(i),
            None => LiquidValue::scalar(n.as_f64().unwrap_or_default()),
        },
        Value::String(s) => LiquidValue::scalar(s.clone()),
        Value::Array(items) => LiquidValue::Array(items.iter().map(json_to_liquid).collect()),
        Value::Object(map) => {
            let mut obj = Object::new();
            for (k, v) in map {
                obj.insert(k.clone().into(), json_to_liquid(v));
            }
            LiquidValue::Object(obj)
        }
    }
}

fn opt_str(value: &Option<String>) -> LiquidValue {
    value
        .as_ref()
        .map_or(LiquidValue::Nil, |s| LiquidValue::scalar(s.clone()))
}

/// The `issue` object exposed to templates (every field present; nil when unset).
pub fn issue_to_liquid(issue: &Issue) -> Object {
    let mut obj = Object::new();
    obj.insert("id".into(), opt_str(&issue.id));
    obj.insert(
        "native_ref".into(),
        issue.native_ref.as_ref().map_or(LiquidValue::Nil, |m| {
            json_to_liquid(&Value::Object(m.clone()))
        }),
    );
    obj.insert("identifier".into(), opt_str(&issue.identifier));
    obj.insert("title".into(), opt_str(&issue.title));
    obj.insert("description".into(), opt_str(&issue.description));
    obj.insert(
        "priority".into(),
        issue.priority.map_or(LiquidValue::Nil, LiquidValue::scalar),
    );
    obj.insert("state".into(), opt_str(&issue.state));
    obj.insert("branch_name".into(), opt_str(&issue.branch_name));
    obj.insert("url".into(), opt_str(&issue.url));
    obj.insert("assignee_id".into(), opt_str(&issue.assignee_id));
    obj.insert(
        "blocked_by".into(),
        LiquidValue::Array(
            issue
                .blocked_by
                .iter()
                .map(|b| {
                    let mut blocker = Object::new();
                    blocker.insert("id".into(), opt_str(&b.id));
                    blocker.insert("identifier".into(), opt_str(&b.identifier));
                    blocker.insert("state".into(), opt_str(&b.state));
                    LiquidValue::Object(blocker)
                })
                .collect(),
        ),
    );
    obj.insert(
        "labels".into(),
        LiquidValue::Array(
            issue
                .labels
                .iter()
                .map(|l| LiquidValue::scalar(l.clone()))
                .collect(),
        ),
    );
    obj.insert(
        "dispatchable".into(),
        LiquidValue::scalar(issue.dispatchable),
    );
    let datetime = |dt: &Option<chrono::DateTime<chrono::Utc>>| {
        dt.as_ref().map_or(LiquidValue::Nil, |d| {
            LiquidValue::scalar(format_datetime(d))
        })
    };
    obj.insert("created_at".into(), datetime(&issue.created_at));
    obj.insert("updated_at".into(), datetime(&issue.updated_at));
    obj
}

/// The turn-1 prompt of one agent run plus its rendered reference sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPrompt {
    /// The core prompt (each reference section replaced by its stub line).
    pub prompt: String,
    /// Rendered reference sections in file order (served by the `workflow_reference` tool).
    pub references: Vec<Reference>,
}

fn parser() -> Result<liquid::Parser, PromptError> {
    liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(|e| PromptError::TemplateParse {
            message: e.to_string(),
            template: String::new(),
        })
}

/// Renders `source` with Solid's error classes; errors name `template` (the operator's text, which
/// differs from `source` when reference placeholders were substituted). `prompt`, when given, is the
/// `prompt` variable of a backend's `prompt_template`.
fn render_template(
    source: &str,
    template: &str,
    issue: &Issue,
    attempt: Option<u32>,
    prompt: Option<&str>,
) -> Result<String, PromptError> {
    let parsed = parser()?.parse(source).map_err(|e| {
        let message = e.to_string();
        if message.contains("Unknown filter") {
            PromptError::TemplateRender(message)
        } else {
            PromptError::TemplateParse {
                message,
                template: template.to_owned(),
            }
        }
    })?;

    let mut globals = Object::new();
    globals.insert(
        "attempt".into(),
        attempt.map_or(LiquidValue::Nil, |a| LiquidValue::scalar(i64::from(a))),
    );
    globals.insert("issue".into(), LiquidValue::Object(issue_to_liquid(issue)));
    if let Some(prompt) = prompt {
        globals.insert("prompt".into(), LiquidValue::scalar(prompt.to_owned()));
    }
    parsed
        .render(&globals)
        .map_err(|e| PromptError::TemplateRender(e.to_string()))
}

/// Renders `template` (blank -> [`DEFAULT_PROMPT_TEMPLATE`]) for `issue` and `attempt`: the core prompt
/// and every reference section.
///
/// The template renders once, as if it had no markers; the sections are then cut out of the rendered
/// text (see [`crate::reference`]), so Liquid scope reaches across them.
///
/// Error classes follow Solid: syntax errors are [`PromptError::TemplateParse`]; unknown variables and
/// unknown filters are [`PromptError::TemplateRender`] (liquid-rust detects unknown filters while parsing,
/// they are reclassified so both engines report the same class). A malformed reference marker is a
/// [`PromptError::TemplateParse`] (workflow loading rejects it first, see [`check_references`]).
pub fn render_run_prompt(
    template: &str,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<RunPrompt, PromptError> {
    let template = if template.trim().is_empty() {
        DEFAULT_PROMPT_TEMPLATE
    } else {
        template
    };
    let sections = reference::split(template).map_err(|e| PromptError::TemplateParse {
        message: format!("invalid prompt reference: {e}"),
        template: template.to_owned(),
    })?;
    let placeholders = reference::Placeholders::new();
    let source = reference::placeholder_template(template, &sections, placeholders);
    let rendered = render_template(&source, template, issue, attempt, None)?;
    let (prompt, references) = reference::cut(&rendered, &sections, placeholders).ok_or_else(|| {
        PromptError::TemplateRender(
            "reference sections did not render in place: a reference marker is inside a Liquid block or tag".into(),
        )
    })?;
    Ok(RunPrompt { prompt, references })
}

/// Renders the turn-1 prompt of `template` (the core, without reference sections) for `issue` and
/// `attempt`; see [`render_run_prompt`].
pub fn render_prompt(
    template: &str,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    render_run_prompt(template, issue, attempt).map(|run| run.prompt)
}

/// Renders a backend's `prompt_template` around an already rendered `prompt`: the template sees
/// `prompt`, `issue` and `attempt` (strict, like [`render_prompt`]).
pub fn render_backend_prompt(
    template: &str,
    prompt: &str,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    render_template(template, template, issue, attempt, Some(prompt))
}

/// Load-time check of a prompt body's reference markers. `first_line` is the file line of the body's
/// first line ([`LoadedWorkflow::prompt_line`]); the error is an operator message naming the file line.
///
/// Besides the rules of [`reference::split`], every marker must sit outside Liquid blocks and tags so
/// that it renders exactly once. liquid-rust itself decides: each marker line becomes a probe tag, the
/// stdlib blocks are wrapped to count their nesting, and every probe must be parsed exactly once at
/// depth 0 (a probe inside `{% raw %}` or a string literal is never parsed as a tag). A template that
/// does not parse is left alone; its Liquid error fails each run as before.
pub fn check_references(template: &str, first_line: usize) -> Result<(), String> {
    let sections = reference::split(template)
        .map_err(|e| format!("invalid prompt reference: {}", e.offset(first_line)))?;
    if sections.is_empty() {
        return Ok(());
    }
    let mut lines: Vec<String> = template.split('\n').map(str::to_owned).collect();
    let markers: Vec<usize> = sections.iter().flat_map(|s| [s.line, s.end_line]).collect();
    for (index, line) in markers.iter().enumerate() {
        if let Some(text) = line.checked_sub(1).and_then(|i| lines.get_mut(i)) {
            *text = format!("{{% {PROBE_TAG} {index} %}}");
        }
    }
    let Some(seen) = probe_marker_depths(&lines.join("\n")) else {
        return Ok(());
    };
    for (index, line) in markers.iter().enumerate() {
        let depths: Vec<usize> = seen
            .iter()
            .filter(|(i, _)| *i == index)
            .map(|(_, d)| *d)
            .collect();
        if depths != [0] {
            return Err(format!(
                "invalid prompt reference: line {}: reference markers must be outside Liquid blocks and tags ({{% if %}}, {{% for %}}, {{% capture %}}, {{% raw %}}, {{% comment %}}, a multi-line {{{{ ... }}}})",
                line + first_line.max(1) - 1
            ));
        }
    }
    Ok(())
}

const PROBE_TAG: &str = "symphony_reference_probe";

/// Block nesting while a template parses, and `(probe index, depth)` for every probe tag parsed.
#[derive(Debug, Default)]
struct Probe {
    depth: AtomicUsize,
    seen: Mutex<Vec<(usize, usize)>>,
}

/// A stdlib block that counts its nesting in the shared [`Probe`].
#[derive(Clone)]
struct Tracked<B> {
    block: B,
    probe: Arc<Probe>,
}

impl<B: ParseBlock + Clone + 'static> ParseBlock for Tracked<B> {
    fn parse(
        &self,
        arguments: TagTokenIter<'_>,
        block: TagBlock<'_, '_>,
        options: &Language,
    ) -> LiquidResult<Box<dyn Renderable>> {
        self.probe.depth.fetch_add(1, Ordering::SeqCst);
        let parsed = self.block.parse(arguments, block, options);
        self.probe.depth.fetch_sub(1, Ordering::SeqCst);
        parsed
    }

    fn reflection(&self) -> &dyn BlockReflection {
        self.block.reflection()
    }
}

/// `{% symphony_reference_probe <index> %}`: records its index and the block depth it was parsed at.
#[derive(Clone)]
struct ProbeTag {
    probe: Arc<Probe>,
}

impl TagReflection for ProbeTag {
    fn tag(&self) -> &str {
        PROBE_TAG
    }

    fn description(&self) -> &str {
        "Marks where a reference marker line sits (load-time check only)."
    }
}

impl ParseTag for ProbeTag {
    fn parse(
        &self,
        mut arguments: TagTokenIter<'_>,
        _options: &Language,
    ) -> LiquidResult<Box<dyn Renderable>> {
        let index = arguments
            .expect_next("probe index")?
            .as_str()
            .parse::<usize>()
            .unwrap_or(usize::MAX);
        arguments.expect_nothing()?;
        let depth = self.probe.depth.load(Ordering::SeqCst);
        self.probe
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((index, depth));
        Ok(Box::new(Silent))
    }

    fn reflection(&self) -> &dyn TagReflection {
        self
    }
}

#[derive(Debug)]
struct Silent;

impl Renderable for Silent {
    fn render_to(&self, _writer: &mut dyn Write, _runtime: &dyn Runtime) -> LiquidResult<()> {
        Ok(())
    }
}

/// Parses `template` with the stdlib blocks wrapped in [`Tracked`] and [`ProbeTag`] registered;
/// `None` when it does not parse. liquid-rust 0.26 can panic on some malformed input (an unclosed
/// `{% raw %}` inside `{% comment %}`); that counts as "does not parse" so a reload never takes the
/// poller down; such a template fails its runs as before.
fn probe_marker_depths(template: &str) -> Option<Vec<(usize, usize)>> {
    use liquid_lib::stdlib;
    let probe = Arc::new(Probe::default());
    let parser = liquid::ParserBuilder::with_stdlib()
        .block(tracked_block(stdlib::RawBlock, &probe))
        .block(tracked_block(stdlib::IfBlock, &probe))
        .block(tracked_block(stdlib::UnlessBlock, &probe))
        .block(tracked_block(stdlib::IfChangedBlock, &probe))
        .block(tracked_block(stdlib::ForBlock, &probe))
        .block(tracked_block(stdlib::TableRowBlock, &probe))
        .block(tracked_block(stdlib::CommentBlock, &probe))
        .block(tracked_block(stdlib::CaptureBlock, &probe))
        .block(tracked_block(stdlib::CaseBlock, &probe))
        .tag(ProbeTag {
            probe: Arc::clone(&probe),
        })
        .build()
        .ok()?;
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| parser.parse(template)));
    match parsed {
        Ok(Ok(_)) => Some(
            probe
                .seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        ),
        Ok(Err(_)) | Err(_) => None,
    }
}

fn tracked_block<B: ParseBlock + Clone + 'static>(block: B, probe: &Arc<Probe>) -> Tracked<B> {
    Tracked {
        block,
        probe: Arc::clone(probe),
    }
}

/// `PromptBuilder.build_prompt/2`: renders the workflow prompt; an unavailable workflow is reported as
/// [`PromptError::WorkflowUnavailable`].
pub fn build_prompt(
    workflow: Result<&LoadedWorkflow, &ConfigError>,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    build_run_prompt(workflow, issue, attempt).map(|run| run.prompt)
}

/// [`build_prompt`] plus the rendered reference sections.
pub fn build_run_prompt(
    workflow: Result<&LoadedWorkflow, &ConfigError>,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<RunPrompt, PromptError> {
    let workflow = workflow.map_err(|e| PromptError::WorkflowUnavailable(e.to_string()))?;
    render_run_prompt(workflow_prompt(workflow), issue, attempt)
}

/// Fixed guidance sent on turns 2..=max_turns of one agent run (the rendered prompt is only sent on
/// turn 1).
pub fn continuation_prompt(turn_number: u32, max_turns: u32) -> String {
    format!(
        "Continuation guidance:

- The previous Codex turn completed normally, but the tracker work item is still in an active state.
- This is continuation turn #{turn_number} of {max_turns} for the current agent run.
- Resume from the current workspace and workpad state instead of restarting from scratch.
- The original task instructions and prior turn context are already present in this thread, so do not restate them before acting.
- Focus on the remaining ticket work and do not end the turn while the issue stays active unless you are truly blocked.
"
    )
}
