//! WorkflowStore tests ported from `extensions_test.exs` (store section) and the core_test reload cases.

mod support;

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use support::{Harness, TestEnv, WORKFLOW_PROMPT, write_workflow};
use symphony_core::error::TrackerConfigError;
use symphony_core::workflow_store::{WorkflowStoreOptions, load_snapshot};
use symphony_core::{ConfigError, MapEnv, WorkflowStore};
use tokio_util::sync::CancellationToken;

fn store_on(dir: &std::path::Path, file: &str) -> Result<Arc<WorkflowStore>, ConfigError> {
    WorkflowStore::start_with(WorkflowStoreOptions {
        path: Some(dir.join(file)),
        cwd: dir.to_path_buf(),
        env: Arc::new(MapEnv::new()),
    })
}

#[test]
fn workflow_store_reloads_changes_keeps_last_good_workflow_and_falls_back_when_stopped() {
    let h = Harness::new();
    assert_eq!(h.store.current().prompt, WORKFLOW_PROMPT);

    write_workflow(
        &h.path(),
        &[
            ("prompt", json!("Second prompt")),
            ("poll_interval_ms", json!(45_000)),
        ],
    );
    h.store.poll();
    assert_eq!(h.store.last_good().workflow.prompt, "Second prompt");
    assert_eq!(h.store.current().prompt, "Second prompt");
    let good_interval = h.store.settings().polling.interval_ms;
    assert_eq!(good_interval, 45_000);

    fs::write(h.path(), "---\ntracker: [\n---\nBroken prompt\n").unwrap();
    assert!(matches!(
        h.store.force_reload(),
        Err(ConfigError::WorkflowParseError(_))
    ));
    assert_eq!(h.store.current().prompt, "Second prompt");

    fs::write(
        h.path(),
        "---\npolling:\n  interval_ms: nope\n---\nTyped-invalid prompt\n",
    )
    .unwrap();
    match h.store.force_reload() {
        Err(ConfigError::InvalidWorkflowConfig(message)) => {
            assert!(message.contains("polling.interval_ms"))
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(h.store.current().prompt, "Second prompt");
    assert_eq!(h.store.settings().polling.interval_ms, good_interval);
    // The stamp is not advanced on failure, so the error is reported again.
    assert!(matches!(
        h.store.force_reload(),
        Err(ConfigError::InvalidWorkflowConfig(_))
    ));

    write_workflow(
        &h.path(),
        &[
            ("tracker_kind", json!("linear")),
            ("tracker_api_token", json!("token")),
            ("tracker_project_slug", serde_json::Value::Null),
            ("prompt", json!("Semantic-invalid prompt")),
        ],
    );
    assert_eq!(
        h.store.force_reload(),
        Err(TrackerConfigError::MissingLinearProjectSlug.into())
    );
    assert_eq!(h.store.current().prompt, "Second prompt");
    assert_eq!(h.store.settings().polling.interval_ms, good_interval);
    assert_eq!(
        h.store.force_reload(),
        Err(TrackerConfigError::MissingLinearProjectSlug.into())
    );

    let third = h.dir.path().join("THIRD_WORKFLOW.md");
    write_workflow(&third, &[("prompt", json!("Third prompt"))]);
    h.store.set_workflow_file_path(&third);
    assert_eq!(h.store.current().prompt, "Third prompt");
    assert_eq!(h.store.last_good().path, third);

    // "Store stopped": a direct load reads the file every time.
    let snapshot = load_snapshot(&third, &MapEnv::new()).unwrap();
    assert_eq!(snapshot.workflow.prompt, "Third prompt");
    assert_eq!(snapshot.settings.polling.interval_ms, 30_000);
    assert_eq!(h.store.force_reload(), Ok(()));
}

#[test]
fn workflow_store_init_stops_on_missing_workflow_file() {
    let dir = tempfile::tempdir().unwrap();
    match store_on(dir.path(), "MISSING_WORKFLOW.md") {
        Err(ConfigError::MissingWorkflowFile { path, reason }) => {
            assert_eq!(path, dir.path().join("MISSING_WORKFLOW.md"));
            assert_eq!(reason.name(), "enoent");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn workflow_store_start_and_poll_cover_missing_file_error_paths() {
    let dir = tempfile::tempdir().unwrap();
    let manual = dir.path().join("MANUAL_WORKFLOW.md");
    let missing = dir.path().join("MANUAL_MISSING_WORKFLOW.md");

    match load_snapshot(&missing, &MapEnv::new()) {
        Err(ConfigError::MissingWorkflowFile { path, reason }) => {
            assert_eq!(path, missing);
            assert_eq!(reason.name(), "enoent");
        }
        other => panic!("unexpected {other:?}"),
    }

    write_workflow(&manual, &[("prompt", json!("Manual workflow prompt"))]);
    let store = store_on(dir.path(), "MANUAL_WORKFLOW.md").unwrap();

    fs::write(&manual, "---\ntracker: [\n---\nBroken prompt\n").unwrap();
    store.poll();
    assert_eq!(store.last_good().workflow.prompt, "Manual workflow prompt");

    store.set_workflow_file_path(&missing);
    store.poll();
    assert_eq!(store.last_good().workflow.prompt, "Manual workflow prompt");
    assert!(matches!(
        store.force_reload(),
        Err(ConfigError::MissingWorkflowFile { ref path, .. }) if *path == missing
    ));

    store.set_workflow_file_path(&manual);
    fs::remove_file(&manual).unwrap();
    store.poll();
    assert_eq!(store.last_good().workflow.prompt, "Manual workflow prompt");
    // Same path as the last good snapshot: the stamp check reports the bare posix reason.
    match store.force_reload() {
        Err(ConfigError::WorkflowFileUnreadable { path, reason }) => {
            assert_eq!(path, manual);
            assert_eq!(reason.name(), "enoent");
        }
        other => panic!("unexpected {other:?}"),
    }

    write_workflow(&manual, &[("prompt", json!("Back again"))]);
    assert_eq!(store.force_reload(), Ok(()));
    assert_eq!(store.current().prompt, "Back again");
}

#[test]
fn orchestrator_fails_startup_when_semantic_preflight_fails() {
    let dir = tempfile::tempdir().unwrap();
    write_workflow(
        &dir.path().join("WORKFLOW.md"),
        &[
            ("tracker_api_token", json!("token")),
            ("tracker_project_slug", serde_json::Value::Null),
        ],
    );
    assert_eq!(
        store_on(dir.path(), "WORKFLOW.md").unwrap_err(),
        TrackerConfigError::MissingLinearProjectSlug.into()
    );
}

#[test]
fn runtime_keeps_last_good_settings_after_an_invalid_reload() {
    let h = Harness::new();
    h.write(&[("tracker_kind", json!("memory"))]);
    assert_eq!(h.store.settings().tracker.kind.as_deref(), Some("memory"));
    assert_eq!(
        h.write_and_validate(&[
            ("tracker_kind", json!("linear")),
            ("tracker_api_token", json!("token")),
            ("tracker_project_slug", serde_json::Value::Null),
        ]),
        Err(TrackerConfigError::MissingLinearProjectSlug.into())
    );
    assert_eq!(h.store.settings().tracker.kind.as_deref(), Some("memory"));
    assert_eq!(h.store.current().prompt, WORKFLOW_PROMPT);
}

#[test]
fn default_path_is_the_captured_cwd_and_switching_paths_reloads() {
    let dir = tempfile::tempdir().unwrap();
    write_workflow(
        &dir.path().join("WORKFLOW.md"),
        &[("prompt", json!("cwd prompt"))],
    );
    let store = WorkflowStore::start_with(WorkflowStoreOptions {
        path: None,
        cwd: dir.path().to_path_buf(),
        env: Arc::new(MapEnv::new()),
    })
    .unwrap();
    assert_eq!(store.workflow_file_path(), dir.path().join("WORKFLOW.md"));
    assert_eq!(store.current().prompt, "cwd prompt");

    let other = dir.path().join("OTHER.md");
    write_workflow(&other, &[("prompt", json!("other prompt"))]);
    store.set_workflow_file_path(&other);
    assert_eq!(store.last_good().workflow.prompt, "other prompt");

    store.clear_workflow_file_path();
    assert_eq!(store.last_good().workflow.prompt, "cwd prompt");
}

#[test]
fn same_size_edits_are_detected_through_the_content_hash() {
    let h = Harness::new();
    h.write(&[("prompt", json!("AAAA"))]);
    assert_eq!(h.store.current().prompt, "AAAA");
    write_workflow(&h.path(), &[("prompt", json!("BBBB"))]);
    assert_eq!(h.store.current().prompt, "BBBB");
}

#[test]
fn last_good_does_not_touch_the_file_system() {
    let h = Harness::new();
    write_workflow(&h.path(), &[("prompt", json!("changed"))]);
    assert_eq!(h.store.last_good().workflow.prompt, WORKFLOW_PROMPT);
    assert_eq!(h.store.snapshot().workflow.prompt, "changed");
}

#[test]
fn env_changes_apply_on_the_next_reload_of_a_changed_file() {
    let env = TestEnv::default();
    env.set("LINEAR_API_KEY", "first");
    let h = Harness::with_env(env);
    h.write(&[("tracker_api_token", serde_json::Value::Null)]);
    assert_eq!(h.store.settings().tracker.api_key.as_deref(), Some("first"));
    h.env.set("LINEAR_API_KEY", "second");
    h.write(&[
        ("tracker_api_token", serde_json::Value::Null),
        ("max_turns", json!(3)),
    ]);
    assert_eq!(
        h.store.settings().tracker.api_key.as_deref(),
        Some("second")
    );
}

#[tokio::test(start_paused = true)]
async fn poller_reloads_every_second_and_stops_on_cancel() {
    let h = Harness::new();
    let cancel = CancellationToken::new();
    let handle = h.store.spawn_poller(cancel.clone());

    write_workflow(&h.path(), &[("prompt", json!("Polled prompt"))]);
    assert_eq!(h.store.last_good().workflow.prompt, WORKFLOW_PROMPT);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(h.store.last_good().workflow.prompt, "Polled prompt");

    // A broken file is ignored by the poller; the last good snapshot stays.
    fs::write(h.path(), "---\ntracker: [\n---\n").unwrap();
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert_eq!(h.store.last_good().workflow.prompt, "Polled prompt");

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
}
