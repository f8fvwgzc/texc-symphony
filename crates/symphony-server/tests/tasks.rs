//! The task API: create, list, get, cancel, unblock and the per-task SSE stream. Bodies are
//! validated against `docs/api/openapi.yaml`.

mod common;

use std::sync::LazyLock;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{Harness, SseReader, envelope, static_snapshot};
use serde_json::{Value, json};
use symphony_server::ServerConfig;
use symphony_server::testing::StaticControlPlane;
use symphony_store::{NewRun, RunStatus, Store, TaskStatus};

static OPENAPI: LazyLock<Value> = LazyLock::new(|| {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/api/openapi.yaml");
    serde_yaml_ng::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
});

#[track_caller]
fn assert_schema(name: &str, instance: &Value) {
    let mut document = OPENAPI.clone();
    document["$ref"] = json!(format!("#/components/schemas/{name}"));
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&document)
        .unwrap();
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(errors.is_empty(), "{name}: {errors:#?}\n{instance}");
}

fn harness() -> Harness {
    Harness::with(
        StaticControlPlane::new(Ok(static_snapshot())),
        Store::open_in_memory().unwrap(),
        ServerConfig::default(),
    )
}

async fn post(harness: &Harness, uri: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = harness.request(request).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn tasks_are_created_listed_and_read() {
    let harness = harness();
    let (status, task) = post(
        &harness,
        "/api/v1/tasks",
        json!({"title": "Summary", "prompt": "Summarize the README",
            "repo": "https://github.com/f8fvwgzc/texc-symphony", "branch": "main",
            "backend": "local", "limits": {"max_tokens": 50000, "timeout_ms": 600000},
            "metadata": {"ticket": "PROD-1"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    assert_schema("Task", &task);
    assert_eq!(task["status"], "queued");
    assert_eq!(task["identifier"], format!("task-{}", task["id"]));
    assert_eq!(
        task["limits"],
        json!({"max_tokens": 50000, "timeout_ms": 600000})
    );
    assert_eq!(task["metadata"], json!({"ticket": "PROD-1"}));
    assert_eq!(
        harness.control.refresh_calls(),
        1,
        "the orchestrator polls now"
    );

    let id = task["id"].as_i64().unwrap();
    let (status, read) = harness.get(&format!("/api/v1/tasks/{id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read, task);
    let (_, page) = harness.get("/api/v1/tasks?status=queued&limit=5").await;
    assert_schema("TaskPage", &page);
    assert_eq!(page["tasks"][0]["id"], id);
    assert_eq!(page["next_before_id"], Value::Null);
    let (status, missing) = harness.get("/api/v1/tasks/999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing, envelope("task_not_found", "Task not found"));
    assert_schema("ErrorEnvelope", &missing);
}

#[tokio::test]
async fn invalid_tasks_are_refused() {
    let harness = harness();
    for (body, message) in [
        (json!({"prompt": "  "}), "prompt must not be empty"),
        (
            json!({"prompt": "x", "repo": "file:///etc"}),
            "repo must be an https:// or ssh:// URL or user@host:path",
        ),
        (
            json!({"prompt": "x", "repo": "ext::sh -c id"}),
            "repo must be an https:// or ssh:// URL or user@host:path",
        ),
        (
            json!({"prompt": "x", "branch": "main"}),
            "branch needs repo",
        ),
        (
            json!({"prompt": "x", "backend": "gpt-9"}),
            "backend must be one of codex, local",
        ),
        (
            json!({"prompt": "x", "limits": {"max_tokens": 0}}),
            "limits must be greater than 0",
        ),
        (
            json!({"prompt": "x", "metadata": [1]}),
            "metadata must be a JSON object",
        ),
    ] {
        let (status, error) = post(&harness, "/api/v1/tasks", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(error, envelope("invalid_parameter", message), "{body}");
    }
    let (status, error) = post(
        &harness,
        "/api/v1/tasks",
        json!({"prompt": "x", "extra": 1}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("invalid body")
    );

    let disabled = Harness::new(StaticControlPlane::new(Ok(static_snapshot())));
    let (status, error) = post(&disabled, "/api/v1/tasks", json!({"prompt": "x"})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error["error"]["code"], "store_disabled");
}

#[tokio::test]
async fn cancel_and_unblock_follow_the_task_state() {
    let harness = harness();
    let (_, task) = post(&harness, "/api/v1/tasks", json!({"prompt": "x"})).await;
    let id = task["id"].as_i64().unwrap();
    let (status, cancelled) =
        post(&harness, &format!("/api/v1/tasks/{id}/cancel"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["error"], "cancelled by client");
    // Final tasks stay as they are.
    let (_, again) = post(&harness, &format!("/api/v1/tasks/{id}/cancel"), json!({})).await;
    assert_eq!(again["status"], "cancelled");
    let (status, conflict) =
        post(&harness, &format!("/api/v1/tasks/{id}/unblock"), json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_schema("ErrorEnvelope", &conflict);
    assert_eq!(conflict["error"]["code"], "task_conflict");

    // A blocked task is re-queued with the operator's reply.
    let (_, task) = post(&harness, "/api/v1/tasks", json!({"prompt": "Deploy it"})).await;
    let id = task["id"].as_i64().unwrap();
    harness.store.start_task(id).await.unwrap();
    harness
        .store
        .finish_task(id, TaskStatus::Blocked, None, Some("needs approval".into()))
        .await
        .unwrap();
    let (status, unblocked) = post(
        &harness,
        &format!("/api/v1/tasks/{id}/unblock"),
        json!({"message": "Approved: staging only."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(unblocked["status"], "queued");
    assert_schema("Task", &unblocked);
    assert!(
        unblocked["prompt"]
            .as_str()
            .unwrap()
            .ends_with("Operator reply:\nApproved: staging only.")
    );
    assert_eq!(harness.control.unblocked_tasks(), vec![id]);
    let (status, _) = post(&harness, "/api/v1/tasks/x/cancel", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// Real time: the store answers from its own thread, which a paused clock would race.
#[tokio::test]
async fn the_task_stream_sends_the_record_and_run_events_until_the_task_is_final() {
    let harness = harness();
    let (_, task) = post(&harness, "/api/v1/tasks", json!({"prompt": "x"})).await;
    let id = task["id"].as_i64().unwrap();
    let store = harness.store.clone();
    store.start_task(id).await.unwrap();
    let run = store
        .start_run(NewRun::new(format!("task:{id}"), format!("task-{id}")))
        .await
        .unwrap();
    store
        .append_event(run, "session_started", Some("session started".into()), None)
        .await
        .unwrap();
    // A tracker issue that happens to carry the same identifier must not leak into the stream.
    let foreign = store
        .start_run(NewRun::new("lin-1", format!("task-{id}")))
        .await
        .unwrap();
    store
        .append_event(foreign, "tracker_only", None, None)
        .await
        .unwrap();

    let response = harness
        .request(
            Request::builder()
                .uri(format!("/api/v1/tasks/{id}/events"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut reader = SseReader::new(response);
    assert_eq!(reader.next().await.unwrap().retry, Some(3000));
    let first = reader.next().await.unwrap();
    assert_eq!(first.event.as_deref(), Some("run_event"));
    assert_eq!(first.id, Some(format!("{run}-1")));
    assert_eq!(first.json()["kind"], "session_started");
    let running = reader.next().await.unwrap();
    assert_eq!(running.event.as_deref(), Some("task"));
    assert_eq!(running.json()["status"], "running");

    store
        .append_event(run, "turn_completed", None, None)
        .await
        .unwrap();
    store
        .finish_run(run, RunStatus::Succeeded, None)
        .await
        .unwrap();
    store
        .finish_task(id, TaskStatus::Succeeded, Some("done".into()), None)
        .await
        .unwrap();
    let mut kinds = Vec::new();
    while let Some(event) = reader.next().await {
        kinds.push((event.event.clone(), event.json()));
    }
    assert_eq!(kinds[0].1["kind"], "turn_completed");
    assert!(
        kinds.iter().all(|(_, json)| json["kind"] != "tracker_only"),
        "{kinds:?}"
    );
    let last = &kinds.last().unwrap().1;
    assert_schema("Task", last);
    assert_schema("RunEvent", &kinds[0].1);
    assert_eq!(last["status"], "succeeded");
    assert_eq!(last["result"], "done");

    let (status, _) = harness.get("/api/v1/tasks/999/events").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
