//! Run history endpoints backed by an in-memory `symphony-store`.

mod common;

use axum::http::{Method, StatusCode};
use common::{Harness, envelope};
use serde_json::json;
use symphony_server::ServerConfig;
use symphony_server::testing::StaticControlPlane;
use symphony_store::{NewRun, RunId, RunStatus, Store, TokenUsage};

/// Five runs: ids 1..=5. MT-1 has runs 1 (failed) and 3 (succeeded), run 5 is still running.
async fn seeded_store() -> Store {
    let store = Store::open_in_memory().unwrap();
    let plan = [
        ("issue-1", "MT-1", Some(RunStatus::Failed)),
        ("issue-2", "MT-2", Some(RunStatus::Succeeded)),
        ("issue-1", "MT-1", Some(RunStatus::Succeeded)),
        ("issue-4", "MT-4", Some(RunStatus::Blocked)),
        ("issue-5", "MT-5", None),
    ];
    for (attempt, (issue_id, identifier, status)) in plan.into_iter().enumerate() {
        let run = store
            .start_run(NewRun {
                issue_title: Some(format!("Title of {identifier}")),
                attempt: u32::try_from(attempt).unwrap(),
                ..NewRun::new(issue_id, identifier)
            })
            .await
            .unwrap();
        store
            .append_event(
                run,
                "session_started",
                Some("session started (thread-1)".into()),
                Some(json!({"session_id": "thread-1"})),
            )
            .await
            .unwrap();
        store
            .append_event(run, "notification", Some("rendered".into()), None)
            .await
            .unwrap();
        store
            .update_tokens(
                run,
                TokenUsage {
                    input: 10,
                    output: 5,
                    total: 15,
                },
            )
            .await
            .unwrap();
        if let Some(status) = status {
            let error = (status != RunStatus::Succeeded).then(|| "boom".to_owned());
            store.finish_run(run, status, error).await.unwrap();
        }
    }
    store
}

async fn harness() -> Harness {
    Harness::with(
        StaticControlPlane::unavailable(),
        seeded_store().await,
        ServerConfig::default(),
    )
}

fn ids(page: &serde_json::Value) -> Vec<i64> {
    page["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["id"].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn runs_are_listed_newest_first_with_keyset_pagination() {
    let harness = harness().await;
    let (status, page) = harness.get("/api/v1/runs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&page), vec![5, 4, 3, 2, 1]);
    assert_eq!(page["next_before_id"], serde_json::Value::Null);

    let run = &page["runs"][4];
    assert_eq!(run["issue_identifier"], "MT-1");
    assert_eq!(run["issue_title"], "Title of MT-1");
    assert_eq!(run["status"], "failed");
    assert_eq!(run["error"], "boom");
    assert_eq!(
        run["tokens"],
        json!({"input": 10, "output": 5, "total": 15})
    );
    assert!(run["duration_ms"].is_i64());
    assert!(run["finished_at"].is_string());
    let running = &page["runs"][0];
    assert_eq!(running["status"], "running");
    assert_eq!(running["finished_at"], serde_json::Value::Null);
    assert_eq!(running["duration_ms"], serde_json::Value::Null);

    let (_, first) = harness.get("/api/v1/runs?limit=2").await;
    assert_eq!(ids(&first), vec![5, 4]);
    let cursor = first["next_before_id"].as_i64().unwrap();
    let (_, second) = harness
        .get(&format!("/api/v1/runs?limit=2&before_id={cursor}"))
        .await;
    assert_eq!(ids(&second), vec![3, 2]);
    let cursor = second["next_before_id"].as_i64().unwrap();
    let (_, last) = harness
        .get(&format!("/api/v1/runs?limit=2&before_id={cursor}"))
        .await;
    assert_eq!(ids(&last), vec![1]);
    assert_eq!(last["next_before_id"], serde_json::Value::Null);
}

#[tokio::test]
async fn runs_can_be_filtered_by_issue_and_status() {
    let harness = harness().await;
    let (_, page) = harness.get("/api/v1/runs?issue=MT-1").await;
    assert_eq!(ids(&page), vec![3, 1]);
    let (_, page) = harness.get("/api/v1/runs?issue=MT-1&status=failed").await;
    assert_eq!(ids(&page), vec![1]);
    let (_, page) = harness.get("/api/v1/runs?status=blocked").await;
    assert_eq!(ids(&page), vec![4]);
    let (_, page) = harness.get("/api/v1/runs?issue=mt-1").await;
    assert_eq!(ids(&page), Vec::<i64>::new(), "exact, case-sensitive");
    let (_, page) = harness.get("/api/v1/runs?unknown=1&limit=1").await;
    assert_eq!(ids(&page), vec![5], "unknown parameters are ignored");
}

#[tokio::test]
async fn invalid_parameters_are_400() {
    let harness = harness().await;
    for (uri, message) in [
        (
            "/api/v1/runs?limit=0",
            "limit must be an integer between 1 and 200",
        ),
        (
            "/api/v1/runs?limit=201",
            "limit must be an integer between 1 and 200",
        ),
        (
            "/api/v1/runs?limit=ten",
            "limit must be an integer between 1 and 200",
        ),
        (
            "/api/v1/runs?before_id=0",
            "before_id must be a positive integer",
        ),
        (
            "/api/v1/runs?before_id=x",
            "before_id must be a positive integer",
        ),
        ("/api/v1/runs?issue=", "issue must not be empty"),
        (
            "/api/v1/runs?status=done",
            "status must be one of running, succeeded, failed, cancelled, blocked",
        ),
        ("/api/v1/runs/abc", "id must be a positive integer"),
        ("/api/v1/runs/0", "id must be a positive integer"),
        ("/api/v1/runs/-3/events", "id must be a positive integer"),
        (
            "/api/v1/runs/1/events?after_seq=-1",
            "after_seq must be a non-negative integer",
        ),
        (
            "/api/v1/runs/1/events?limit=1001",
            "limit must be an integer between 1 and 1000",
        ),
    ] {
        let (status, body) = harness.get(uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(body, envelope("invalid_parameter", message), "{uri}");
    }
}

#[tokio::test]
async fn one_run_and_its_events() {
    let harness = harness().await;
    let (status, run) = harness.get("/api/v1/runs/3").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(run["id"], 3);
    assert_eq!(run["status"], "succeeded");
    assert_eq!(run["error"], serde_json::Value::Null);

    let (status, body) = harness.get("/api/v1/runs/999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, envelope("run_not_found", "Run not found"));

    let (status, events) = harness.get("/api/v1/runs/3/events").await;
    assert_eq!(status, StatusCode::OK);
    let events = events["events"].as_array().unwrap().clone();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["run_id"], 3);
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["kind"], "session_started");
    assert_eq!(events[0]["payload"], json!({"session_id": "thread-1"}));
    assert_eq!(events[1]["payload"], serde_json::Value::Null);

    let (_, page) = harness
        .get("/api/v1/runs/3/events?after_seq=1&limit=1")
        .await;
    let page = page["events"].as_array().unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0]["seq"], 2);

    let (status, body) = harness.get("/api/v1/runs/999/events").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, envelope("run_not_found", "Run not found"));

    // A run without events (yet) is an empty page, not a 404.
    let fresh = harness
        .store
        .start_run(NewRun::new("issue-9", "MT-9"))
        .await
        .unwrap();
    assert_eq!(fresh, RunId(6));
    let (status, body) = harness.get("/api/v1/runs/6/events").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"events": []}));
}

#[tokio::test]
async fn totals_aggregate_all_runs() {
    let harness = harness().await;
    let (status, totals) = harness.get("/api/v1/totals").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(totals["runs_total"], 5);
    assert_eq!(totals["runs_succeeded"], 2);
    assert_eq!(totals["runs_failed"], 1);
    assert_eq!(
        totals["tokens"],
        json!({"input": 50, "output": 25, "total": 75})
    );
    assert!(totals["runtime_ms"].is_u64());
}

#[tokio::test]
async fn history_endpoints_report_store_disabled() {
    let harness = Harness::new(StaticControlPlane::unavailable());
    for uri in [
        "/api/v1/runs",
        "/api/v1/runs/1",
        "/api/v1/runs/1/events",
        "/api/v1/totals",
    ] {
        let (status, body) = harness.get(uri).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(
            body,
            envelope("store_disabled", "Run history store is disabled"),
            "{uri}"
        );
    }
    // Parameter validation still comes first.
    let (status, _) = harness.json(Method::GET, "/api/v1/runs?limit=0").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
