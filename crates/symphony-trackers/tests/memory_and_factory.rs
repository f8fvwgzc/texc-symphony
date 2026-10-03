//! Port of the tracker parts of `extensions_test.exs` and `core_test.exs`: memory adapter, kind
//! dispatch, session binding.

mod support;

use std::sync::Arc;

use serde_json::json;
use support::*;
use symphony_core::{Issue, MapEnv};
use symphony_trackers::{
    MemoryTracker, TRACKER_KINDS, ToolBinding, Tracker, TrackerError, bind_agent_tools,
    build_tracker, is_active_state, is_terminal_state, tracker_for_kind,
};

fn issue(id: &str, state: Option<&str>) -> Issue {
    Issue {
        id: Some(id.into()),
        identifier: Some(format!("MT-{id}")),
        title: Some("t".into()),
        state: state.map(str::to_owned),
        ..Issue::default()
    }
}

#[tokio::test]
async fn tracker_delegates_to_memory_and_linear_adapters() {
    let deps = offline_deps(MapEnv::new());
    deps.memory.set(vec![
        issue("issue-1", Some("In Progress")),
        issue("issue-2", Some("Done")),
    ]);
    let workflow = settings_with_env(json!({"kind": "memory"}), &MapEnv::new());
    let tracker = build_tracker(&workflow, &deps).unwrap();
    assert_eq!(tracker.kind(), "memory");

    let found = tracker
        .fetch_issues_by_states(&workflow.tracker, &strings(&[" in progress "]))
        .await
        .unwrap();
    assert_eq!(ids(&found), ["issue-1"]);
    let by_id = tracker
        .fetch_issues_by_ids(&workflow.tracker, &strings(&["issue-1"]))
        .await
        .unwrap();
    assert_eq!(ids(&by_id), ["issue-1"]);

    // Candidate/terminal helpers use the memory defaults (Linear's state lists).
    assert_eq!(
        ids(&tracker
            .fetch_candidate_issues(&workflow.tracker)
            .await
            .unwrap()),
        ["issue-1"]
    );
    assert_eq!(
        ids(&tracker
            .fetch_terminal_issues(&workflow.tracker)
            .await
            .unwrap()),
        ["issue-2"]
    );

    let binding = bind_agent_tools(&workflow, &deps).unwrap();
    assert_eq!(binding.kind(), "memory");
    assert!(binding.tool_specs().is_empty());
    assert!(binding.secret_environment_names().is_empty());
    let result = binding
        .execute(Some("linear_graphql"), &json!({}), None)
        .await;
    assert!(!result.success);
    assert_eq!(
        result.output,
        r#"{"error":{"message":"Unsupported dynamic tool: \"linear_graphql\".","supportedTools":[]}}"#
    );

    assert_eq!(
        tracker_for_kind("future-tracker", &deps).err(),
        Some(TrackerError::UnsupportedTrackerKind(
            "future-tracker".into()
        ))
    );

    let linear = settings_with_env(
        json!({"kind": "linear", "api_key": "token", "project_slug": "p"}),
        &MapEnv::new(),
    );
    let binding = bind_agent_tools(&linear, &deps).unwrap();
    assert_eq!(binding.kind(), "linear");
    assert_eq!(binding.secret_environment_names(), ["LINEAR_API_KEY"]);
    assert_eq!(binding.tool_names(), ["linear_graphql"]);
}

#[tokio::test]
async fn memory_tracker_quirks() {
    let tracker = MemoryTracker::default();
    tracker.issues().set(vec![
        issue("a", None),
        issue("b", Some("Todo")),
        issue("a", Some("Todo")),
    ]);
    let s = tracker_settings(json!({"kind": "memory"}));
    // A blank requested state matches issues without a state.
    assert_eq!(
        ids(&tracker
            .fetch_issues_by_states(&s, &strings(&[" "]))
            .await
            .unwrap()),
        ["a"]
    );
    // Configured order and duplicates are preserved; ids are matched exactly.
    assert_eq!(
        ids(&tracker
            .fetch_issues_by_ids(&s, &strings(&["b", "a", " a"]))
            .await
            .unwrap()),
        ["a", "b", "a"]
    );
    assert_eq!(tracker.fetch_issues_by_states(&s, &[]).await, Ok(vec![]));
    assert!(tracker.secret_environment_names(&s).is_empty());
    assert_eq!(tracker.validate_config(&s), Ok(()));
}

#[test]
fn every_supported_kind_builds_and_missing_kind_is_reported() {
    let deps = offline_deps(MapEnv::new());
    for kind in TRACKER_KINDS {
        assert_eq!(tracker_for_kind(kind, &deps).unwrap().kind(), kind);
    }
    assert_eq!(
        tracker_for_kind("Linear", &deps).err(),
        Some(TrackerError::UnsupportedTrackerKind("Linear".into()))
    );
    let workflow = settings_with_env(json!({}), &MapEnv::new());
    assert_eq!(
        build_tracker(&workflow, &deps).err(),
        Some(TrackerError::MissingTrackerKind)
    );
    assert_eq!(
        symphony_trackers::validate_config(&workflow.tracker, &deps),
        Err(TrackerError::MissingTrackerKind)
    );
    let unsupported = settings_with_env(json!({"kind": "123"}), &MapEnv::new());
    assert_eq!(
        symphony_trackers::validate_config(&unsupported.tracker, &deps),
        Err(TrackerError::UnsupportedTrackerKind("123".into()))
    );
}

#[test]
fn state_helpers_trim_and_lowercase() {
    let s = tracker_settings(json!({"kind": "memory"}));
    assert!(is_active_state(&s, " todo "));
    assert!(is_active_state(&s, "IN PROGRESS"));
    assert!(!is_active_state(&s, "Done"));
    assert!(is_terminal_state(&s, "canceled"));
    assert!(!is_terminal_state(&s, "Todo"));
}

#[tokio::test]
async fn binding_passes_the_bound_settings_even_after_reload() {
    let deps = offline_deps(MapEnv::new());
    let tracker: Arc<dyn Tracker> = Arc::new(MemoryTracker::new(deps.memory.clone()));
    let binding = ToolBinding::bind(tracker, tracker_settings(json!({"kind": "memory"})));
    assert_eq!(binding.settings().kind.as_deref(), Some("memory"));
    let result = binding.execute(None, &json!({}), None).await;
    assert!(result.output.contains("Unsupported dynamic tool: nil."));
    assert_eq!(result.to_value()["contentItems"][0]["text"], result.output);
}
