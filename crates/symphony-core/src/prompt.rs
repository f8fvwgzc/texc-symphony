//! Prompt rendering (`SymphonyElixir.PromptBuilder`) with strict Liquid semantics.
//!
//! The template sees exactly two top-level variables: `attempt` (integer or nil) and `issue` (every
//! [`Issue`] field, nil when unset; datetimes as ISO-8601 `Z` strings). Unknown variables and filters fail.

use liquid::model::{Object, Value as LiquidValue};
use serde_json::Value;

use crate::error::ConfigError;
use crate::issue::{Issue, format_datetime};
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

/// Renders `template` (blank -> [`DEFAULT_PROMPT_TEMPLATE`]) for `issue` and `attempt`.
///
/// Error classes follow Solid: syntax errors are [`PromptError::TemplateParse`]; unknown variables and
/// unknown filters are [`PromptError::TemplateRender`] (liquid-rust detects unknown filters while parsing,
/// they are reclassified so both engines report the same class).
pub fn render_prompt(
    template: &str,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    let template = if template.trim().is_empty() {
        DEFAULT_PROMPT_TEMPLATE
    } else {
        template
    };
    let parser =
        liquid::ParserBuilder::with_stdlib()
            .build()
            .map_err(|e| PromptError::TemplateParse {
                message: e.to_string(),
                template: template.to_owned(),
            })?;
    let parsed = parser.parse(template).map_err(|e| {
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
    parsed
        .render(&globals)
        .map_err(|e| PromptError::TemplateRender(e.to_string()))
}

/// `PromptBuilder.build_prompt/2`: renders the workflow prompt; an unavailable workflow is reported as
/// [`PromptError::WorkflowUnavailable`].
pub fn build_prompt(
    workflow: Result<&LoadedWorkflow, &ConfigError>,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    let workflow = workflow.map_err(|e| PromptError::WorkflowUnavailable(e.to_string()))?;
    render_prompt(workflow_prompt(workflow), issue, attempt)
}

/// Fixed guidance sent on turns 2..=max_turns of one agent run (the full template is only sent on turn 1).
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

/// `AgentRunner.build_turn_prompt/4`: turn 1 renders the workflow prompt, later turns get
/// [`continuation_prompt`].
pub fn build_turn_prompt(
    workflow: Result<&LoadedWorkflow, &ConfigError>,
    issue: &Issue,
    attempt: Option<u32>,
    turn_number: u32,
    max_turns: u32,
) -> Result<String, PromptError> {
    if turn_number <= 1 {
        build_prompt(workflow, issue, attempt)
    } else {
        Ok(continuation_prompt(turn_number, max_turns))
    }
}
