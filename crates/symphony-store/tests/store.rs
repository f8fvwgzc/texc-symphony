//! Integration tests for the public `Store` API.

use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use symphony_store::{
    INTERRUPTED_ERROR, INTERRUPTED_EVENT_KIND, NewRun, RetryRecord, RunId, RunQuery, RunStatus,
    SCHEMA_VERSION, Store, StoreError, TokenUsage,
};

fn memory() -> Store {
    Store::open_in_memory().expect("in-memory store")
}

fn new_run(issue: &str) -> NewRun {
    NewRun {
        issue_title: Some(format!("Title of {issue}")),
        ..NewRun::new(format!("id-{issue}"), issue)
    }
}

#[tokio::test]
async fn migrations_are_idempotent_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/dir/symphony.db");

    let store = Store::open(&path).unwrap();
    assert_eq!(store.path(), Some(path.as_path()));
    let id = store.start_run(new_run("MT-1")).await.unwrap();
    drop(store);

    for _ in 0..3 {
        let store = Store::open(&path).unwrap();
        assert!(store.get_run(id).await.unwrap().is_some());
        drop(store);
    }

    let raw = rusqlite::Connection::open(&path).unwrap();
    let version: u32 = raw
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let mode: String = raw
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
}

#[tokio::test]
async fn refuses_database_from_newer_build() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.db");
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 5))
        .unwrap();
    drop(raw);
    let err = Store::open(&path).unwrap_err();
    assert!(matches!(err, StoreError::SchemaTooNew { .. }), "{err}");
}

#[test]
fn open_failure_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    // A directory cannot be opened as a database file.
    let err = Store::open(dir.path()).unwrap_err();
    assert!(err.to_string().starts_with("store_"), "{err}");
}

#[tokio::test]
async fn run_crud_round_trip() {
    let store = memory();
    let started = Utc::now() - ChronoDuration::seconds(90);
    let id = store
        .start_run(NewRun {
            attempt: 2,
            worker_host: Some("dm-dev2".into()),
            started_at: Some(started),
            ..new_run("MT-7")
        })
        .await
        .unwrap();

    let run = store.get_run(id).await.unwrap().unwrap();
    assert_eq!(run.id, id);
    assert_eq!(run.issue_id, "id-MT-7");
    assert_eq!(run.issue_identifier, "MT-7");
    assert_eq!(run.issue_title.as_deref(), Some("Title of MT-7"));
    assert_eq!(run.attempt, 2);
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(
        run.started_at.timestamp_millis(),
        started.timestamp_millis()
    );
    assert_eq!(run.finished_at, None);
    assert_eq!(run.tokens, TokenUsage::default());

    store
        .update_runtime_info(id, None, Some("/workspaces/MT-7".into()))
        .await
        .unwrap();
    store
        .update_tokens(
            id,
            TokenUsage {
                input: 4,
                output: 8,
                total: 12,
            },
        )
        .await
        .unwrap();
    assert_eq!(store.increment_turns(id).await.unwrap(), 1);
    assert_eq!(store.increment_turns(id).await.unwrap(), 2);

    let finished = store
        .finish_run(id, RunStatus::Failed, Some("boom".into()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(finished.status, RunStatus::Failed);
    assert_eq!(finished.error.as_deref(), Some("boom"));
    assert_eq!(finished.turns, 2);
    assert_eq!(finished.worker_host.as_deref(), Some("dm-dev2"));
    assert_eq!(finished.workspace_path.as_deref(), Some("/workspaces/MT-7"));
    assert_eq!(finished.tokens.total, 12);
    let duration = finished.duration_ms.unwrap();
    assert!((90_000..120_000).contains(&duration), "{duration}");
    assert_eq!(store.get_run(id).await.unwrap(), Some(finished));

    // Finishing twice, finishing as running, and unknown ids are errors.
    assert_eq!(
        store.finish_run(id, RunStatus::Succeeded, None).await,
        Err(StoreError::RunAlreadyFinished(id))
    );
    assert!(matches!(
        store.finish_run(id, RunStatus::Running, None).await,
        Err(StoreError::InvalidArgument(_))
    ));
    let missing = RunId(9_999);
    assert_eq!(store.get_run(missing).await, Ok(None));
    assert_eq!(
        store.finish_run(missing, RunStatus::Failed, None).await,
        Err(StoreError::RunNotFound(missing))
    );
    assert_eq!(
        store.update_tokens(missing, TokenUsage::default()).await,
        Err(StoreError::RunNotFound(missing))
    );
    assert_eq!(
        store.increment_turns(missing).await,
        Err(StoreError::RunNotFound(missing))
    );
    assert_eq!(
        store.append_event(missing, "x", None, None).await,
        Err(StoreError::RunNotFound(missing))
    );
    assert!(matches!(
        store.start_run(NewRun::new("", "MT-x")).await,
        Err(StoreError::InvalidArgument(_))
    ));
}

#[tokio::test]
async fn identifier_falls_back_to_issue_id() {
    let store = memory();
    let id = store.start_run(NewRun::new("issue-9", "")).await.unwrap();
    let run = store.get_run(id).await.unwrap().unwrap();
    assert_eq!(run.issue_identifier, "issue-9");
}

#[tokio::test]
async fn events_have_per_run_sequences() {
    let store = memory();
    let a = store.start_run(new_run("MT-A")).await.unwrap();
    let b = store.start_run(new_run("MT-B")).await.unwrap();

    assert_eq!(
        store
            .append_event(a, "session_started", Some("hello".into()), None)
            .await,
        Ok(1)
    );
    assert_eq!(
        store.append_event(b, "session_started", None, None).await,
        Ok(1)
    );
    let payload = json!({"method": "turn/completed", "params": {"usage": {"input_tokens": 3}}});
    assert_eq!(
        store
            .append_event(a, "turn_completed", None, Some(payload.clone()))
            .await,
        Ok(2)
    );
    assert_eq!(
        store.append_event(a, "notification", None, None).await,
        Ok(3)
    );
    assert!(matches!(
        store.append_event(a, " ", None, None).await,
        Err(StoreError::InvalidArgument(_))
    ));

    let events = store.list_events(a, None, None).await.unwrap();
    let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![1, 2, 3]);
    assert_eq!(events[0].kind, "session_started");
    assert_eq!(events[0].message.as_deref(), Some("hello"));
    assert_eq!(events[1].payload, Some(payload));
    assert!(events.windows(2).all(|w| w[0].at <= w[1].at));

    let tail = store.list_events(a, Some(1), Some(1)).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].seq, 2);
    assert!(
        store
            .list_events(a, Some(3), None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.list_events(b, None, None).await.unwrap().len(), 1);

    let json = serde_json::to_value(&events[0]).unwrap();
    assert_eq!(json["run_id"], a.0);
    assert_eq!(json["seq"], 1);
    assert_eq!(json["kind"], "session_started");
    assert!(json["at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_appends_get_unique_gap_free_seqs() {
    let store = memory();
    let runs = [
        store.start_run(new_run("MT-C1")).await.unwrap(),
        store.start_run(new_run("MT-C2")).await.unwrap(),
    ];
    const TASKS: usize = 32;
    const PER_TASK: usize = 25;

    let mut handles = Vec::new();
    for task in 0..TASKS {
        let store = store.clone();
        let run = runs[task % 2];
        handles.push(tokio::spawn(async move {
            let mut seqs = Vec::new();
            for i in 0..PER_TASK {
                let seq = store
                    .append_event(run, "notification", Some(format!("{task}-{i}")), None)
                    .await
                    .unwrap();
                seqs.push(seq);
            }
            // Within one submitter, seqs follow submission order.
            assert!(seqs.windows(2).all(|w| w[0] < w[1]));
            seqs
        }));
    }
    // Fire-and-forget writes from another clone interleave safely.
    for _ in 0..10 {
        store.increment_turns(runs[0]).detach();
    }
    for handle in handles {
        handle.await.unwrap();
    }
    store.flush().await.unwrap();

    let expected_per_run = (TASKS / 2 * PER_TASK) as i64;
    for run in runs {
        let events = store.list_events(run, None, Some(1000)).await.unwrap();
        let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (1..=expected_per_run).collect::<Vec<_>>());
    }
    assert_eq!(store.get_run(runs[0]).await.unwrap().unwrap().turns, 10);
}

#[tokio::test]
async fn list_runs_paginates_with_before_id_cursor() {
    let store = memory();
    let mut ids = Vec::new();
    for i in 0..7 {
        ids.push(store.start_run(new_run(&format!("MT-{i}"))).await.unwrap());
    }

    let page1 = store
        .list_runs(RunQuery {
            limit: Some(3),
            ..RunQuery::default()
        })
        .await
        .unwrap();
    let got: Vec<RunId> = page1.runs.iter().map(|r| r.id).collect();
    assert_eq!(got, vec![ids[6], ids[5], ids[4]]);
    assert_eq!(page1.next_before_id, Some(ids[4].0));

    let page2 = store
        .list_runs(RunQuery {
            limit: Some(3),
            before_id: page1.next_before_id,
            ..RunQuery::default()
        })
        .await
        .unwrap();
    let got: Vec<RunId> = page2.runs.iter().map(|r| r.id).collect();
    assert_eq!(got, vec![ids[3], ids[2], ids[1]]);
    assert_eq!(page2.next_before_id, Some(ids[1].0));

    let page3 = store
        .list_runs(RunQuery {
            limit: Some(3),
            before_id: page2.next_before_id,
            ..RunQuery::default()
        })
        .await
        .unwrap();
    let got: Vec<RunId> = page3.runs.iter().map(|r| r.id).collect();
    assert_eq!(got, vec![ids[0]]);
    assert_eq!(page3.next_before_id, None);

    // Exactly-full last page reports no further cursor.
    let exact = store
        .list_runs(RunQuery {
            limit: Some(7),
            ..RunQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(exact.runs.len(), 7);
    assert_eq!(exact.next_before_id, None);

    // Default and maximum limits.
    let all = store.list_runs(RunQuery::default()).await.unwrap();
    assert_eq!(all.runs.len(), 7);
}

#[tokio::test]
async fn list_runs_respects_default_and_max_limit() {
    let store = memory();
    for i in 0..230 {
        store.start_run(new_run(&format!("MT-{i}"))).detach();
    }
    store.flush().await.unwrap();
    let default_page = store.list_runs(RunQuery::default()).await.unwrap();
    assert_eq!(default_page.runs.len(), 50);
    assert!(default_page.next_before_id.is_some());
    let capped = store
        .list_runs(RunQuery {
            limit: Some(10_000),
            ..RunQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(capped.runs.len(), 200);
}

#[tokio::test]
async fn list_runs_filters_by_identifier_and_status() {
    let store = memory();
    let a1 = store.start_run(new_run("MT-A")).await.unwrap();
    let b1 = store.start_run(new_run("MT-B")).await.unwrap();
    let a2 = store
        .start_run(NewRun {
            attempt: 1,
            ..new_run("MT-A")
        })
        .await
        .unwrap();
    store
        .finish_run(a1, RunStatus::Failed, Some("x".into()))
        .await
        .unwrap();
    store
        .finish_run(b1, RunStatus::Succeeded, None)
        .await
        .unwrap();

    let only_a = store
        .list_runs(RunQuery {
            issue_identifier: Some("MT-A".into()),
            ..RunQuery::default()
        })
        .await
        .unwrap();
    let got: Vec<RunId> = only_a.runs.iter().map(|r| r.id).collect();
    assert_eq!(got, vec![a2, a1]);

    let failed = store
        .list_runs(RunQuery {
            status: Some(RunStatus::Failed),
            ..RunQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(failed.runs.len(), 1);
    assert_eq!(failed.runs[0].id, a1);

    let running_a = store
        .list_runs(RunQuery {
            issue_identifier: Some("MT-A".into()),
            status: Some(RunStatus::Running),
            ..RunQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(running_a.runs.len(), 1);
    assert_eq!(running_a.runs[0].id, a2);

    let none = store
        .list_runs(RunQuery {
            issue_identifier: Some("MT-Z".into()),
            ..RunQuery::default()
        })
        .await
        .unwrap();
    assert!(none.runs.is_empty());
    assert_eq!(none.next_before_id, None);

    // Query-string style deserialization (what the HTTP layer will use).
    let query: RunQuery =
        serde_json::from_value(json!({"issue_identifier": "MT-A", "status": "running"})).unwrap();
    assert_eq!(store.list_runs(query).await.unwrap().runs.len(), 1);
}

#[tokio::test]
async fn totals_aggregate_all_runs() {
    let store = memory();
    let empty = store.totals().await.unwrap();
    assert_eq!(empty.runs_total, 0);
    assert_eq!(empty.runtime_ms, 0);

    let past = Utc::now() - ChronoDuration::seconds(10);
    let ok = store
        .start_run(NewRun {
            started_at: Some(past),
            ..new_run("MT-1")
        })
        .await
        .unwrap();
    let bad = store
        .start_run(NewRun {
            started_at: Some(past),
            ..new_run("MT-2")
        })
        .await
        .unwrap();
    let live = store.start_run(new_run("MT-3")).await.unwrap();
    for (run, n) in [(ok, 1), (bad, 10), (live, 100)] {
        store
            .update_tokens(
                run,
                TokenUsage {
                    input: n,
                    output: 2 * n,
                    total: 3 * n,
                },
            )
            .await
            .unwrap();
    }
    store
        .finish_run(ok, RunStatus::Succeeded, None)
        .await
        .unwrap();
    store
        .finish_run(bad, RunStatus::Failed, Some("e".into()))
        .await
        .unwrap();

    let totals = store.totals().await.unwrap();
    assert_eq!(totals.runs_total, 3);
    assert_eq!(totals.runs_succeeded, 1);
    assert_eq!(totals.runs_failed, 1);
    assert_eq!(
        totals.tokens,
        TokenUsage {
            input: 111,
            output: 222,
            total: 333
        }
    );
    assert!(totals.runtime_ms >= 20_000, "{}", totals.runtime_ms);

    let json = serde_json::to_value(totals).unwrap();
    for key in [
        "runs_total",
        "runs_succeeded",
        "runs_failed",
        "tokens",
        "runtime_ms",
    ] {
        assert!(json.get(key).is_some(), "missing {key}");
    }
}

#[tokio::test]
async fn interrupted_runs_are_recovered_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("symphony.db");

    let (running, done) = {
        let store = Store::open(&path).unwrap();
        let running = store.start_run(new_run("MT-R")).await.unwrap();
        store
            .append_event(running, "session_started", None, None)
            .await
            .unwrap();
        let done = store.start_run(new_run("MT-D")).await.unwrap();
        store
            .finish_run(done, RunStatus::Succeeded, None)
            .await
            .unwrap();
        store.flush().await.unwrap();
        (running, done)
        // Store dropped here without finishing `running`: simulates a crash.
    };

    let store = Store::open(&path).unwrap();
    assert_eq!(store.mark_interrupted_runs().await, Ok(1));
    assert_eq!(store.mark_interrupted_runs().await, Ok(0));

    let run = store.get_run(running).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(run.error.as_deref(), Some(INTERRUPTED_ERROR));
    assert_eq!(run.error.as_deref(), Some("interrupted by restart"));
    assert!(run.finished_at.is_some());
    assert!(run.duration_ms.unwrap() >= 0);

    let events = store.list_events(running, None, None).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].seq, 2);
    assert_eq!(events[1].kind, INTERRUPTED_EVENT_KIND);

    let untouched = store.get_run(done).await.unwrap().unwrap();
    assert_eq!(untouched.status, RunStatus::Succeeded);
    assert_eq!(untouched.error, None);
}

#[tokio::test]
async fn prune_deletes_old_finished_runs_and_their_events() {
    let store = memory();
    let old = Utc::now() - ChronoDuration::days(40);
    let mut old_ids = Vec::new();
    for i in 0..3 {
        let id = store
            .start_run(NewRun {
                started_at: Some(old),
                ..new_run(&format!("MT-OLD-{i}"))
            })
            .await
            .unwrap();
        store.append_event(id, "a", None, None).await.unwrap();
        store.append_event(id, "b", None, None).await.unwrap();
        old_ids.push(id);
    }
    // Finish the first two; the third is still running and must survive.
    store
        .finish_run(old_ids[0], RunStatus::Succeeded, None)
        .await
        .unwrap();
    store
        .finish_run(old_ids[1], RunStatus::Failed, None)
        .await
        .unwrap();
    let recent = store.start_run(new_run("MT-NEW")).await.unwrap();
    store
        .finish_run(recent, RunStatus::Succeeded, None)
        .await
        .unwrap();

    let thirty_days = Duration::from_secs(30 * 86_400);

    // keep_min_runs protects the newest runs regardless of age.
    let stats = store.prune(thirty_days, 3).await.unwrap();
    assert_eq!(stats.runs_deleted, 1);
    assert_eq!(stats.events_deleted, 2);
    assert_eq!(store.get_run(old_ids[0]).await.unwrap(), None);
    assert!(
        store
            .list_events(old_ids[0], None, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.get_run(old_ids[1]).await.unwrap().is_some());

    let stats = store.prune(thirty_days, 0).await.unwrap();
    assert_eq!(stats.runs_deleted, 1);
    assert_eq!(stats.events_deleted, 2);
    assert_eq!(store.get_run(old_ids[1]).await.unwrap(), None);

    // Running runs and recent runs are never pruned.
    assert!(store.get_run(old_ids[2]).await.unwrap().is_some());
    assert!(store.get_run(recent).await.unwrap().is_some());
    assert_eq!(store.prune(thirty_days, 0).await.unwrap().runs_deleted, 0);
    assert_eq!(store.totals().await.unwrap().runs_total, 2);

    // A zero window prunes every finished run beyond keep_min_runs.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let stats = store.prune(Duration::ZERO, 0).await.unwrap();
    assert_eq!(stats.runs_deleted, 1);
    assert!(store.get_run(old_ids[2]).await.unwrap().is_some());
}

fn retry(issue_id: &str, attempt: u32, due_in_ms: i64) -> RetryRecord {
    // Millisecond precision, as stored.
    let due_at = chrono::DateTime::from_timestamp_millis(
        (Utc::now() + ChronoDuration::milliseconds(due_in_ms)).timestamp_millis(),
    )
    .unwrap();
    RetryRecord {
        issue_id: issue_id.into(),
        attempt,
        due_at,
        identifier: format!("MT-{issue_id}"),
        issue_url: Some(format!("https://example.org/{issue_id}")),
        error: Some("agent exited: boom".into()),
        worker_host: None,
        workspace_path: Some(format!("/tmp/ws/MT-{issue_id}")),
    }
}

#[tokio::test]
async fn the_retry_queue_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("symphony.db");
    let (late, soon) = (retry("a", 3, 60_000), retry("b", 1, 1_000));
    {
        let store = Store::open(&path).unwrap();
        store.save_retry(retry("a", 2, 30_000)).detach();
        // One retry per issue: the newer one replaces the older.
        store.save_retry(late.clone()).detach();
        store.save_retry(soon.clone()).detach();
        store.save_retry(retry("gone", 1, 0)).detach();
        store.delete_retry("gone").detach();
        store.delete_retry("never-queued").await.unwrap();
        store.flush().await.unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.list_retries().await.unwrap(), vec![soon, late]);

    let disabled = Store::disabled();
    disabled.save_retry(retry("a", 1, 0)).await.unwrap();
    assert!(disabled.list_retries().await.unwrap().is_empty());
}

#[tokio::test]
async fn disabled_store_is_a_silent_no_op() {
    let store = Store::disabled();
    assert!(!store.is_enabled());
    assert_eq!(store.path(), None);
    assert_eq!(format!("{store:?}"), "Store(disabled)");

    let id = store.start_run(new_run("MT-1")).await.unwrap();
    assert_eq!(id, RunId::DISABLED);
    assert_eq!(store.append_event(id, "x", None, None).await, Ok(0));
    assert_eq!(store.update_tokens(id, TokenUsage::default()).await, Ok(()));
    assert_eq!(store.increment_turns(id).await, Ok(0));
    assert_eq!(store.update_runtime_info(id, None, None).await, Ok(()));
    assert_eq!(
        store.finish_run(id, RunStatus::Failed, None).await,
        Ok(None)
    );
    assert_eq!(store.get_run(id).await, Ok(None));
    assert!(
        store
            .list_runs(RunQuery::default())
            .await
            .unwrap()
            .runs
            .is_empty()
    );
    assert!(store.list_events(id, None, None).await.unwrap().is_empty());
    assert_eq!(store.totals().await.unwrap().runs_total, 0);
    assert_eq!(store.mark_interrupted_runs().await, Ok(0));
    assert_eq!(
        store.prune(Duration::ZERO, 0).await.unwrap().runs_deleted,
        0
    );
    store.flush().await.unwrap();
}

#[tokio::test]
async fn fire_and_forget_writes_land_and_failures_do_not_panic() {
    let store = memory();
    let id = store.start_run(new_run("MT-F")).await.unwrap();
    store.append_event(id, "one", None, None).detach();
    store
        .append_event(RunId(424_242), "orphan", None, None)
        .detach();
    drop(store.update_tokens(
        id,
        TokenUsage {
            input: 1,
            output: 2,
            total: 3,
        },
    ));
    store.flush().await.unwrap();
    assert_eq!(store.list_events(id, None, None).await.unwrap().len(), 1);
    assert_eq!(store.get_run(id).await.unwrap().unwrap().tokens.total, 3);
}

#[tokio::test]
async fn clones_share_one_database() {
    let store = memory();
    let other = store.clone();
    let id = store.start_run(new_run("MT-S")).await.unwrap();
    drop(store);
    assert!(other.get_run(id).await.unwrap().is_some());
}
