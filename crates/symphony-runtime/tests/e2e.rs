//! End-to-end: the supervised runtime with the real agent runner, a fake Codex app-server, the memory
//! tracker and an in-memory store — dispatch, continuation retry, terminal cleanup, history.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
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

/// A Claude Code run that a provider limit ends (the result Claude Code 2.1.302 prints for an HTTP
/// 429) waits instead of counting toward `agent.max_retry_attempts`: the issue is retried again
/// and again at the same attempt and never blocked. Genuine failures after it still count.
#[tokio::test]
async fn a_rate_limited_claude_run_waits_instead_of_blocking_the_issue() {
    let bin = tempfile::tempdir().unwrap();
    let claude = bin.path().join("claude");
    fs::write(
        &claude,
        r#"#!/bin/sh
dir=$(dirname "$0")
cat > /dev/null
echo x >> "$dir/runs"
if [ -f "$dir/fail" ]; then
  echo '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}'
else
  echo '{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"result":"API Error: Request rejected (429) · Rate limit exceeded","terminal_reason":"api_error","session_id":"s-1"}'
fi
exit 1
"#,
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let runs = || {
        fs::read_to_string(bin.path().join("runs"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    let wf = TestWorkflow::new(json!({
        "polling": {"interval_ms": 100},
        "agent": {
            "max_turns": 1,
            "max_retry_attempts": 2,
            "max_retry_backoff_ms": 50,
            "backend": "claude",
        },
        "backends": {"claude": {"kind": "claude_code", "command": claude.to_string_lossy()}},
    }));
    let memory = MemoryIssues::new();
    memory.set(vec![issue("i-1", "MT-LIM", "In Progress")]);
    let options = RuntimeOptions::new(Arc::clone(&wf.store), TrackerDeps::new().unwrap())
        .with_tracker(Arc::new(MemoryTracker::new(memory.clone())))
        .with_runner_options(RunnerOptions {
            symphony_executable: None,
            ..RunnerOptions::default()
        });
    let runtime = Runtime::start(options);
    let handle = runtime.handle();

    // Five limited runs, more than the retry limit allows failures: still waiting at attempt 1.
    let snapshot = wait_for(&handle, |s| {
        runs() >= 5 && s.retrying.iter().any(|r| r.identifier == "MT-LIM")
    })
    .await;
    assert!(snapshot.blocked.is_empty(), "{snapshot:#?}");
    let retry = &snapshot.retrying[0];
    assert_eq!(retry.attempt, 1);
    assert!(
        retry
            .error
            .as_deref()
            .is_some_and(|e| e.contains("agent_rate_limited") && e.contains("(429)")),
        "{retry:#?}"
    );

    // Genuine failures count: retry 2, then the issue is blocked.
    fs::write(bin.path().join("fail"), "").unwrap();
    let snapshot = wait_for(&handle, |s| !s.blocked.is_empty()).await;
    assert_eq!(
        snapshot.blocked[0].error,
        "retry limit reached: 2 retries failed (last error: agent exited: agent_backend_failed: \
         claude reported error_during_execution: boom)"
    );
    runtime.shutdown().await.unwrap();
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
