//! Prompt builder tests ported from `core_test.exs`, plus Solid <-> liquid parity checks.

mod support;

use std::path::Path;

use chrono::{TimeZone, Utc};
use serde_json::json;
use support::Harness;
use symphony_core::issue::{BlockerRef, Issue};
use symphony_core::prompt::{
    DEFAULT_PROMPT_TEMPLATE, PromptError, build_prompt, build_turn_prompt, continuation_prompt,
    render_prompt,
};
use symphony_core::{ConfigError, LoadedWorkflow, MapEnv, workflow};

fn issue(identifier: &str, title: &str, description: Option<&str>, labels: &[&str]) -> Issue {
    Issue {
        identifier: Some(identifier.into()),
        title: Some(title.into()),
        description: description.map(str::to_owned),
        state: Some("Todo".into()),
        url: Some(format!("https://example.org/issues/{identifier}")),
        labels: labels.iter().map(|s| s.to_string()).collect(),
        ..Issue::default()
    }
}

fn build_with(
    h: &Harness,
    prompt: &str,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    h.write(&[("prompt", json!(prompt))]);
    let workflow = h.store.current();
    build_prompt(Ok(&workflow), issue, attempt)
}

#[test]
fn prompt_builder_renders_issue_and_attempt_values_from_workflow_template() {
    let h = Harness::new();
    let issue = Issue {
        state: Some("Todo".into()),
        ..issue(
            "S-1",
            "Refactor backend request path",
            Some("Replace transport layer"),
            &["backend"],
        )
    };
    let prompt = build_with(
        &h,
        "Ticket {{ issue.identifier }} {{ issue.title }} labels={{ issue.labels }} attempt={{ attempt }}",
        &issue,
        Some(3),
    )
    .unwrap();
    assert!(prompt.contains("Ticket S-1 Refactor backend request path"));
    assert!(prompt.contains("labels=backend"));
    assert!(prompt.contains("attempt=3"));
}

#[test]
fn prompt_builder_renders_issue_datetime_fields_without_crashing() {
    let h = Harness::new();
    let issue = Issue {
        created_at: Some(Utc.with_ymd_and_hms(2026, 2, 26, 18, 6, 48).unwrap()),
        updated_at: Some(Utc.with_ymd_and_hms(2026, 2, 26, 18, 7, 3).unwrap()),
        ..issue(
            "MT-697",
            "Live smoke",
            Some("Prompt should serialize datetimes"),
            &[],
        )
    };
    let prompt = build_with(
        &h,
        "Ticket {{ issue.identifier }} created={{ issue.created_at }} updated={{ issue.updated_at }}",
        &issue,
        None,
    )
    .unwrap();
    assert!(prompt.contains("Ticket MT-697"));
    assert!(prompt.contains("created=2026-02-26T18:06:48Z"));
    assert!(prompt.contains("updated=2026-02-26T18:07:03Z"));
}

#[test]
fn prompt_builder_normalizes_nested_maps_in_issue_fields() {
    let h = Harness::new();
    let issue = Issue {
        native_ref: json!({"phase": "test", "nested": {"date": "2026-02-28", "n": 1, "f": 1.5, "list": [true, null]}})
            .as_object()
            .cloned(),
        blocked_by: vec![BlockerRef {
            id: Some("b1".into()),
            identifier: Some("MT-1".into()),
            state: None,
        }],
        ..issue("MT-701", "Serialize nested values", Some("normalize nested terms"), &[])
    };
    assert_eq!(
        build_with(&h, "Ticket {{ issue.identifier }}", &issue, None).unwrap(),
        "Ticket MT-701"
    );
    assert_eq!(
        render_prompt(
            "{{ issue.native_ref.nested.n }}|{{ issue.blocked_by[0].identifier }}|{{ issue.blocked_by[0].state }}",
            &issue,
            None
        )
        .unwrap(),
        "1|MT-1|"
    );
}

#[test]
fn prompt_builder_uses_strict_variable_rendering() {
    let h = Harness::new();
    let issue = issue(
        "MT-123",
        "Investigate broken sync",
        Some("Reproduce and fix"),
        &["bug"],
    );
    let err = build_with(
        &h,
        "Work on ticket {{ missing.ticket_id }} and follow these steps.",
        &issue,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, PromptError::TemplateRender(_)), "{err:?}");
}

#[test]
fn prompt_builder_surfaces_invalid_template_content_with_prompt_context() {
    let h = Harness::new();
    let issue = issue(
        "MT-999",
        "Broken prompt",
        Some("Invalid template syntax"),
        &[],
    );
    let err = build_with(&h, "{% if issue.identifier %}", &issue, None).unwrap_err();
    match &err {
        PromptError::TemplateParse { template, .. } => {
            assert_eq!(template, "{% if issue.identifier %}")
        }
        other => panic!("unexpected {other:?}"),
    }
    let message = err.to_string();
    assert!(message.starts_with("template_parse_error: "), "{message}");
    assert!(
        message.contains("template=\"{% if issue.identifier %}\""),
        "{message}"
    );
}

#[test]
fn prompt_builder_uses_a_sensible_default_template_when_workflow_prompt_is_blank() {
    let h = Harness::new();
    let issue = Issue {
        state: Some("In Progress".into()),
        ..issue(
            "MT-777",
            "Make fallback prompt useful",
            Some("Include enough issue context to start working."),
            &["prompt"],
        )
    };
    let prompt = build_with(&h, "   \n", &issue, None).unwrap();
    assert!(prompt.contains("You are working on an issue from the configured tracker."));
    assert!(prompt.contains("Identifier: MT-777"));
    assert!(prompt.contains("Title: Make fallback prompt useful"));
    assert!(prompt.contains("Body:"));
    assert!(prompt.contains("Include enough issue context to start working."));
    let template = h.store.workflow_prompt();
    assert!(template.contains("{{ issue.identifier }}"));
    assert!(template.contains("{{ issue.title }}"));
    assert!(template.contains("{{ issue.description }}"));
}

#[test]
fn prompt_builder_default_template_handles_missing_issue_body() {
    let h = Harness::new();
    let issue = issue("MT-778", "Handle empty body", None, &[]);
    let prompt = build_with(&h, "", &issue, None).unwrap();
    assert!(prompt.contains("Identifier: MT-778"));
    assert!(prompt.contains("Title: Handle empty body"));
    assert!(prompt.contains("No description provided."));
}

#[test]
fn prompt_builder_reports_workflow_load_failures_separately_from_template_parse_errors() {
    let dir = tempfile::tempdir().unwrap();
    let err = workflow::load(&dir.path().join("missing-workflow.md")).unwrap_err();
    let issue = issue(
        "MT-780",
        "Workflow unavailable",
        Some("Missing workflow file"),
        &[],
    );
    let result = build_prompt(Err(&err), &issue, None).unwrap_err();
    assert!(matches!(result, PromptError::WorkflowUnavailable(_)));
    assert!(
        result
            .to_string()
            .starts_with("workflow_unavailable: missing_workflow_file")
    );
    let _: &ConfigError = &err;
}

#[test]
fn in_repo_workflow_md_renders_correctly() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/WORKFLOW.md");
    let env = MapEnv::new().with("LINEAR_API_KEY", "test-linear-api-key");
    let snapshot = symphony_core::workflow_store::load_snapshot(&path, &env).unwrap();
    let issue = Issue {
        state: Some("In Progress".into()),
        url: Some("https://example.org/issues/MT-616/use-rich-templates-for-workflowmd".into()),
        ..issue(
            "MT-616",
            "Use rich templates for WORKFLOW.md",
            Some("Render with rich template variables"),
            &["templating", "workflow"],
        )
    };
    let prompt = build_prompt(Ok(&snapshot.workflow), &issue, Some(2)).unwrap();
    for fragment in [
        "You are working on a Linear ticket `MT-616`",
        "Issue context:",
        "Identifier: MT-616",
        "Title: Use rich templates for WORKFLOW.md",
        "Current status: In Progress",
        "Labels: templatingworkflow",
        "https://example.org/issues/MT-616/use-rich-templates-for-workflowmd",
        "This is an unattended orchestration session.",
        "Only stop early for a true external blocker",
        "Do not include \"next steps for user\"",
        "open and follow `.codex/skills/land/SKILL.md`",
        "Do not call `gh pr merge` directly",
        "Follow-up context:",
        "follow-up attempt #2",
    ] {
        assert!(prompt.contains(fragment), "missing {fragment:?}");
    }
    let first_run = build_prompt(Ok(&snapshot.workflow), &issue, None).unwrap();
    assert!(!first_run.contains("Follow-up context:"));
}

#[test]
fn prompt_builder_adds_continuation_guidance_for_retries() {
    let h = Harness::new();
    let issue = issue(
        "MT-201",
        "Continue autonomous ticket",
        Some("Retry flow"),
        &[],
    );
    let prompt = build_with(
        &h,
        "{% if attempt %}Retry #{{ attempt }}{% endif %}",
        &issue,
        Some(2),
    )
    .unwrap();
    assert_eq!(prompt, "Retry #2");
}

#[test]
fn continuation_turns_use_fixed_guidance() {
    let expected = "Continuation guidance:\n\n\
- The previous Codex turn completed normally, but the tracker work item is still in an active state.\n\
- This is continuation turn #2 of 20 for the current agent run.\n\
- Resume from the current workspace and workpad state instead of restarting from scratch.\n\
- The original task instructions and prior turn context are already present in this thread, so do not restate them before acting.\n\
- Focus on the remaining ticket work and do not end the turn while the issue stays active unless you are truly blocked.\n";
    assert_eq!(continuation_prompt(2, 20), expected);

    let workflow = LoadedWorkflow {
        prompt: "First {{ issue.identifier }}".into(),
        prompt_template: "First {{ issue.identifier }}".into(),
        ..LoadedWorkflow::default()
    };
    let issue = issue("MT-9", "t", None, &[]);
    assert_eq!(
        build_turn_prompt(Ok(&workflow), &issue, None, 1, 20).unwrap(),
        "First MT-9"
    );
    assert_eq!(
        build_turn_prompt(Ok(&workflow), &issue, None, 2, 20).unwrap(),
        expected
    );
    // Later turns never touch the workflow, so an unavailable workflow does not matter there.
    let err = ConfigError::MissingTrackerKind;
    assert!(build_turn_prompt(Err(&err), &issue, None, 3, 20).is_ok());
}

#[test]
fn liquid_parity_with_solid_strict_mode() {
    let mut issue = issue("MT-1", "T", None, &["a", "b"]);
    issue.priority = Some(2);
    let render = |template: &str, attempt: Option<u32>| render_prompt(template, &issue, attempt);

    // Arrays concatenate without a separator; nil renders empty.
    assert_eq!(render("{{ issue.labels }}", None).unwrap(), "ab");
    assert_eq!(
        render("[{{ issue.description }}][{{ attempt }}]", None).unwrap(),
        "[][]"
    );
    assert_eq!(
        render("{{ issue.dispatchable }}|{{ issue.priority }}", None).unwrap(),
        "false|2"
    );
    assert_eq!(
        render(
            "{{ issue.labels.size }}|{{ issue.labels.first }}|{{ issue.labels[1] }}",
            None
        )
        .unwrap(),
        "2|a|b"
    );
    assert_eq!(
        render("{% for l in issue.labels %}<{{ l }}>{% endfor %}", None).unwrap(),
        "<a><b>"
    );
    assert_eq!(
        render("{{ issue.labels | join: \", \" }}", None).unwrap(),
        "a, b"
    );
    // Truthiness checks on missing variables do not raise (Solid behaves the same).
    assert_eq!(
        render("{% if missing %}y{% else %}n{% endif %}", None).unwrap(),
        "n"
    );
    assert_eq!(
        render("{% if issue.foo %}y{% else %}n{% endif %}", None).unwrap(),
        "n"
    );
    assert_eq!(
        render("{% if issue.description %}d{% else %}nd{% endif %}", None).unwrap(),
        "nd"
    );
    assert_eq!(render("{% if attempt %}A{% endif %}", None).unwrap(), "");
    assert_eq!(
        render("{% if attempt %}A{{ attempt }}{% endif %}", Some(1)).unwrap(),
        "A1"
    );
    // Output of unknown variables/fields fails at render time.
    assert!(matches!(
        render("{{ issue.foo }}", None),
        Err(PromptError::TemplateRender(_))
    ));
    assert!(matches!(
        render("{{ missing }}", None),
        Err(PromptError::TemplateRender(_))
    ));
    assert!(matches!(
        render("{{ issue.title.foo }}", None),
        Err(PromptError::TemplateRender(_))
    ));
    // Unknown filters are a render-class error, like Solid's strict_filters.
    assert!(matches!(
        render("{{ issue.title | nope }}", None),
        Err(PromptError::TemplateRender(_))
    ));
    // Unknown tags are parse errors in both engines.
    assert!(matches!(
        render("{% unknown_tag %}", None),
        Err(PromptError::TemplateParse { .. })
    ));
}

#[test]
fn default_prompt_template_matches_elixir() {
    assert_eq!(
        DEFAULT_PROMPT_TEMPLATE,
        "You are working on an issue from the configured tracker.\n\nIdentifier: {{ issue.identifier }}\nTitle: {{ issue.title }}\n\nBody:\n{% if issue.description %}\n{{ issue.description }}\n{% else %}\nNo description provided.\n{% endif %}\n"
    );
    let rendered = render_prompt("", &issue("MT-1", "T", Some("desc"), &[]), None).unwrap();
    assert_eq!(
        rendered,
        "You are working on an issue from the configured tracker.\n\nIdentifier: MT-1\nTitle: T\n\nBody:\n\ndesc\n\n"
    );
}
