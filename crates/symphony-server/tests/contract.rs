//! Every response body validates against its schema in `docs/api/openapi.yaml`.

mod common;

use std::sync::LazyLock;

use axum::http::{Method, StatusCode};
use common::{Harness, SseReader, static_snapshot};
use serde_json::{Value, json};
use symphony_server::testing::StaticControlPlane;
use symphony_server::{ServerConfig, StateError};
use symphony_store::{NewRun, RunStatus, Store, TokenUsage};

static OPENAPI: LazyLock<Value> = LazyLock::new(|| {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/api/openapi.yaml");
    serde_yaml_ng::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
});

/// Validate `instance` against the schema at JSON pointer `pointer` of the OpenAPI document
/// (resolving `$ref`s inside it).
#[track_caller]
fn assert_valid(pointer: &str, instance: &Value) {
    let mut document = OPENAPI.clone();
    document["$ref"] = json!(format!("#{pointer}"));
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&document)
        .unwrap_or_else(|err| panic!("schema {pointer} does not compile: {err}"));
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|err| format!("{} at {}", err, err.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{pointer}: {errors:#?}\n{}",
        serde_json::to_string_pretty(instance).unwrap()
    );
}

fn schema(name: &str) -> String {
    format!("/components/schemas/{name}")
}

#[test]
fn the_validator_rejects_wrong_shapes() {
    let document = {
        let mut document = OPENAPI.clone();
        document["$ref"] = json!("#/components/schemas/ErrorEnvelope");
        document
    };
    let validator = jsonschema::draft202012::options().build(&document).unwrap();
    assert!(validator.is_valid(&json!({"error": {"code": "x", "message": "y"}})));
    assert!(!validator.is_valid(&json!({"error": {"code": "x"}})));
    assert!(!validator.is_valid(&json!({"error": {"code": "x", "message": "y"}, "extra": 1})));
}

#[tokio::test]
async fn live_state_responses_match_the_contract() {
    let harness = Harness::snapshot();
    let (_, state) = harness.get("/api/v1/state").await;
    assert_valid(&schema("StatePayload"), &state);
    assert_valid(&schema("StateSnapshot"), &state);

    for identifier in ["MT-HTTP", "MT-RETRY", "MT-BLOCKED"] {
        let (status, issue) = harness.get(&format!("/api/v1/{identifier}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_valid(&schema("IssueDetail"), &issue);
    }

    let (status, refresh) = harness.json(Method::POST, "/api/v1/refresh").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_valid(&schema("RefreshAccepted"), &refresh);

    let (_, health) = harness.get("/api/v1/health").await;
    assert_valid(&schema("Health"), &health);

    let (_, openapi) = harness.get("/api/openapi.json").await;
    assert!(openapi.is_object());

    for (error, code) in [
        (StateError::Timeout, "snapshot_timeout"),
        (StateError::Unavailable, "snapshot_unavailable"),
    ] {
        let harness = Harness::new(StaticControlPlane::new(Err(error)));
        let (_, failure) = harness.get("/api/v1/state").await;
        assert_eq!(failure["error"]["code"], code);
        assert_valid(&schema("StatePayload"), &failure);
        assert_valid(&schema("StateError"), &failure);
    }
}

#[tokio::test]
async fn sse_payloads_match_the_contract() {
    let harness = Harness::snapshot();
    let response = harness
        .request(
            axum::http::Request::builder()
                .uri("/api/v1/events")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    let mut reader = SseReader::new(response);
    let _retry = reader.next().await.unwrap();
    let snapshot = reader.next().await.unwrap();
    assert_valid(&schema("StatePayload"), &snapshot.json());

    let heartbeat = json!({"at": "2026-02-24T20:15:45Z", "generation": 42});
    assert_valid(
        "/paths/~1api~1v1~1events/get/responses/200/content/text~1event-stream/x-sse-events/heartbeat/schema",
        &heartbeat,
    );
}

#[tokio::test]
async fn every_error_envelope_matches_the_contract() {
    let harness = Harness::new(StaticControlPlane::unavailable());
    for (method, uri, status) in [
        (Method::GET, "/unknown", StatusCode::NOT_FOUND),
        (
            Method::GET,
            "/api/v1/refresh",
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        (Method::GET, "/api/v1/MT-1", StatusCode::NOT_FOUND),
        (
            Method::POST,
            "/api/v1/refresh",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (Method::GET, "/api/v1/runs", StatusCode::SERVICE_UNAVAILABLE),
        (Method::GET, "/api/v1/runs?limit=0", StatusCode::BAD_REQUEST),
    ] {
        let (actual, body) = harness.json(method.clone(), uri).await;
        assert_eq!(actual, status, "{method} {uri}");
        assert_valid(&schema("ErrorEnvelope"), &body);
    }

    let store = Store::open_in_memory().unwrap();
    let harness = Harness::with(
        StaticControlPlane::unavailable(),
        store,
        ServerConfig::default(),
    );
    let (status, body) = harness.get("/api/v1/runs/77").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_valid(&schema("ErrorEnvelope"), &body);
}

#[tokio::test]
async fn history_responses_match_the_contract() {
    let store = Store::open_in_memory().unwrap();
    let finished = store
        .start_run(NewRun {
            issue_title: Some("Render the HTTP dashboard".into()),
            workspace_path: Some("/tmp/symphony_workspaces/MT-HTTP".into()),
            ..NewRun::new("issue-http", "MT-HTTP")
        })
        .await
        .unwrap();
    store
        .append_event(
            finished,
            "session_started",
            Some("session started (thread-http)".into()),
            Some(json!({"session_id": "thread-http"})),
        )
        .await
        .unwrap();
    store
        .append_event(finished, "notification", None, Some(json!(["raw", 1])))
        .await
        .unwrap();
    store
        .update_tokens(
            finished,
            TokenUsage {
                input: 18230,
                output: 2207,
                total: 20437,
            },
        )
        .await
        .unwrap();
    store
        .finish_run(finished, RunStatus::Succeeded, None)
        .await
        .unwrap();
    let running = store
        .start_run(NewRun {
            attempt: 2,
            worker_host: Some("dm-dev2".into()),
            ..NewRun::new("issue-retry", "MT-RETRY")
        })
        .await
        .unwrap();

    let harness = Harness::with(
        StaticControlPlane::new(Ok(static_snapshot())),
        store,
        ServerConfig::default(),
    );
    let (_, page) = harness.get("/api/v1/runs").await;
    assert_eq!(page["runs"].as_array().unwrap().len(), 2);
    assert_valid(&schema("RunList"), &page);
    let (_, page) = harness.get("/api/v1/runs?limit=1").await;
    assert!(page["next_before_id"].is_i64());
    assert_valid(&schema("RunList"), &page);

    for id in [finished, running] {
        let (status, run) = harness.get(&format!("/api/v1/runs/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_valid(&schema("RunRecord"), &run);
        let (_, events) = harness.get(&format!("/api/v1/runs/{id}/events")).await;
        assert_valid(&schema("RunEventList"), &events);
    }

    let (_, totals) = harness.get("/api/v1/totals").await;
    assert_valid(&schema("Totals"), &totals);
    let (_, health) = harness.get("/api/v1/health").await;
    assert_eq!(health["store"], "sqlite");
    assert_valid(&schema("Health"), &health);
}
