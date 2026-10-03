//! Seeds the `memory` tracker from `tracker.provider.issues` (new in the Rust port).
//!
//! Elixir filled the memory tracker only from application env in tests, so `tracker.kind: memory`
//! was useless outside the test suite. Here a workflow can list issues inline, which makes demos,
//! local smoke tests and end-to-end tests possible without a real tracker:
//!
//! ```yaml
//! tracker:
//!   kind: memory
//!   provider:
//!     issues:
//!       - { id: "1", identifier: "DEMO-1", title: "Try Symphony", state: "Todo" }
//! ```
//!
//! Each entry deserializes into [`Issue`]; `dispatchable` defaults to `true` (the struct default is
//! `false`, which would make a hand-written issue never run). The list is applied at startup and
//! re-applied whenever the reloaded workflow changes it.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use symphony_core::{Issue, Settings, WorkflowStore};
use symphony_trackers::MemoryIssues;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// How often the seeder looks for a changed `provider.issues` list.
pub const SEED_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The raw `tracker.provider.issues` value of a memory workflow.
pub fn seed_value(settings: &Settings) -> Option<&Value> {
    if settings.tracker.kind.as_deref() != Some("memory") {
        return None;
    }
    settings.tracker.provider.get("issues")
}

/// Parses a `provider.issues` value.
pub fn parse_issues(value: &Value) -> Result<Vec<Issue>, String> {
    let Value::Array(items) = value else {
        return Err("tracker.provider.issues must be a list of issues".to_owned());
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let Value::Object(map) = item else {
                return Err(format!("tracker.provider.issues[{index}] must be a map"));
            };
            let mut map = map.clone();
            map.entry("dispatchable").or_insert(Value::Bool(true));
            serde_json::from_value::<Issue>(Value::Object(map))
                .map_err(|err| format!("tracker.provider.issues[{index}] is invalid: {err}"))
        })
        .collect()
}

/// Applies the workflow's issue list to `memory` now (synchronously) and whenever it changes,
/// until `cancel`.
pub fn spawn(
    workflow: Arc<WorkflowStore>,
    memory: MemoryIssues,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    // The first application is synchronous so the orchestrator's first poll already sees the issues.
    let mut applied = apply(&workflow, &memory, None);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SEED_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = interval.tick() => applied = apply(&workflow, &memory, applied),
            }
        }
    })
}

/// Applies the current list when it differs from `applied`; returns the list now applied.
fn apply(workflow: &WorkflowStore, memory: &MemoryIssues, applied: Option<Value>) -> Option<Value> {
    let settings = workflow.last_good().settings.clone();
    let Some(value) = seed_value(&settings) else {
        return applied;
    };
    if applied.as_ref() == Some(value) {
        return applied;
    }
    match parse_issues(value) {
        Ok(issues) => {
            tracing::info!(
                count = issues.len(),
                "Loaded memory tracker issues from WORKFLOW.md"
            );
            memory.set(issues);
        }
        Err(err) => tracing::warn!("Ignoring memory tracker issues: {err}"),
    }
    Some(value.clone())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_issues_and_defaults_dispatchable() {
        let issues = parse_issues(&json!([
            {"id": "1", "identifier": "DEMO-1", "title": "Try", "state": "Todo", "labels": ["a"]},
            {"id": "2", "identifier": "DEMO-2", "state": "Todo", "dispatchable": false,
             "created_at": "2026-02-26T18:06:48Z"}
        ]))
        .unwrap();
        assert_eq!(issues.len(), 2);
        assert!(issues[0].dispatchable);
        assert_eq!(issues[0].labels, vec!["a".to_owned()]);
        assert!(!issues[1].dispatchable);
        assert!(issues[1].created_at.is_some());
    }

    #[test]
    fn rejects_malformed_lists() {
        assert!(parse_issues(&json!({"id": "1"})).is_err());
        assert_eq!(
            parse_issues(&json!(["x"])).unwrap_err(),
            "tracker.provider.issues[0] must be a map"
        );
        assert!(parse_issues(&json!([{"labels": "not-a-list"}])).is_err());
    }

    #[tokio::test]
    async fn seeds_and_reseeds_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("WORKFLOW.md");
        let write = |title: &str| {
            std::fs::write(
                &path,
                format!(
                    "---\ntracker:\n  kind: memory\n  provider:\n    issues:\n      - {{id: \"1\", identifier: \"M-1\", title: \"{title}\", state: Todo}}\n---\nprompt\n"
                ),
            )
            .unwrap();
        };
        write("first");
        let workflow = WorkflowStore::start(Some(path.clone())).unwrap();
        let memory = MemoryIssues::new();
        let cancel = CancellationToken::new();
        let task = spawn(Arc::clone(&workflow), memory.clone(), cancel.clone());
        let wait_for = |title: &'static str| {
            let memory = memory.clone();
            async move {
                for _ in 0..100 {
                    if memory.get().first().and_then(|i| i.title.as_deref()) == Some(title) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                panic!("memory issues never became {title}");
            }
        };
        wait_for("first").await;
        // A different size guarantees a new stamp even within the same mtime second.
        write("second, longer");
        workflow.force_reload().unwrap();
        wait_for("second, longer").await;
        cancel.cancel();
        task.await.unwrap();
    }
}
