//! End-to-end: the supervised runtime with the real agent runner, a fake Codex app-server, the memory
//! tracker and an in-memory store — dispatch, continuation retry, terminal cleanup, history.

mod support;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use support::{TestWorkflow, codex_fixture, issue, sh_quote};
use symphony_runtime::{RunnerOptions, Runtime, RuntimeOptions};
use symphony_store::{RunQuery, RunStatus, Store};
use symphony_trackers::{MemoryIssues, MemoryTracker, TrackerDeps};

#[tokio::test]
async fn runtime_dispatches_records_and_cleans_up_an_issue() {
    let wf = TestWorkflow::new(json!({
        "polling": {"interval_ms": 100},
        "agent": {"max_turns": 1},
        "codex": {"command": format!("sh {} app-server", sh_quote(&codex_fixture("basic.sh").to_string_lossy()))},
    }));
    let home = wf.path("home");
    fs::create_dir_all(&home).unwrap();
    let memory = MemoryIssues::new();
    memory.set(vec![issue("i-1", "MT-E2E", "In Progress")]);
    let store = Store::open_in_memory().unwrap();
    let options = RuntimeOptions::new(Arc::clone(&wf.store), TrackerDeps::new().unwrap())
        .with_tracker(Arc::new(MemoryTracker::new(memory.clone())))
        .with_store(store.clone())
        .with_runner_options(RunnerOptions {
            codex_env: vec![("HOME".into(), home.to_string_lossy().into_owned())],
            codex_stop_grace: Duration::from_millis(300),
            ..RunnerOptions::default()
        });
    let runtime = Runtime::start(options);
    let handle = runtime.handle();
    let mut updates = handle.subscribe();

    // The run completes normally and a 1 s continuation re-check is queued.
    let snapshot = wait_for(&handle, |s| {
        s.retrying
            .iter()
            .any(|r| r.identifier == "MT-E2E" && r.attempt == 1)
    })
    .await;
    let retry = &snapshot.retrying[0];
    let workspace = PathBuf::from(retry.workspace_path.clone().expect("workspace recorded"));
    assert_eq!(workspace, wf.canonical_root().join("MT-E2E"));
    assert!(workspace.is_dir());
    assert!(updates.has_changed().unwrap());
    updates.borrow_and_update();

    // The issue is finished in the tracker: the re-check cleans the workspace and releases it.
    memory.set(vec![issue("i-1", "MT-E2E", "Done")]);
    wait_for(&handle, |s| {
        s.retrying.is_empty() && s.running.is_empty() && s.blocked.is_empty()
    })
    .await;
    support::eventually(Duration::from_secs(5), || !workspace.exists()).await;

    let json = serde_json::to_value(handle.snapshot().await.unwrap()).unwrap();
    for key in [
        "generated_at",
        "running",
        "retrying",
        "blocked",
        "codex_totals",
        "rate_limits",
        "polling",
    ] {
        assert!(json.get(key).is_some(), "snapshot JSON lacks {key}");
    }
    assert!(handle.issue("MT-E2E").await.unwrap().is_none());

    runtime.shutdown().await.unwrap();
    store.flush().await.unwrap();
    let runs = store.list_runs(RunQuery::default()).await.unwrap().runs;
    let run = runs
        .iter()
        .find(|r| r.issue_identifier == "MT-E2E")
        .unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.turns, 1);
    assert_eq!(
        run.workspace_path.as_deref(),
        Some(workspace.to_string_lossy().as_ref())
    );
}

async fn wait_for(
    handle: &symphony_runtime::RuntimeHandle,
    check: impl Fn(&symphony_runtime::Snapshot) -> bool,
) -> symphony_runtime::Snapshot {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let snapshot = handle.snapshot().await.unwrap();
        if check(&snapshot) {
            return snapshot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "condition not reached; last snapshot: {snapshot:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
