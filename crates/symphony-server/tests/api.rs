//! Elixir-compatible routes and envelopes (ports of `extensions_test.exs` "phoenix observability
//! api ..." tests) plus the new meta endpoints and middleware behavior.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{Harness, envelope, static_snapshot, timestamp};
use serde_json::{Value, json};
use symphony_server::testing::StaticControlPlane;
use symphony_server::{ServerConfig, StateError, Unavailable};
use symphony_store::Store;

#[tokio::test]
async fn state_issue_and_refresh_match_the_elixir_payloads() {
    let harness = Harness::snapshot();

    let (status, state) = harness.get("/api/v1/state").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state,
        json!({
            "generated_at": state["generated_at"],
            "counts": {"running": 1, "retrying": 1, "blocked": 1},
            "running": [{
                "issue_id": "issue-http",
                "issue_identifier": "MT-HTTP",
                "issue_url": "https://example.org/issues/MT-HTTP",
                "state": "In Progress",
                "worker_host": null,
                "workspace_path": null,
                "session_id": "thread-http",
                "turn_count": 7,
                "last_event": "notification",
                "last_message": "rendered",
                "started_at": state["running"][0]["started_at"],
                "last_event_at": null,
                "tokens": {"input_tokens": 4, "output_tokens": 8, "total_tokens": 12}
            }],
            "retrying": [{
                "issue_id": "issue-retry",
                "issue_identifier": "MT-RETRY",
                "issue_url": "https://example.org/issues/MT-RETRY",
                "attempt": 2,
                "due_at": state["retrying"][0]["due_at"],
                "error": "boom",
                "worker_host": null,
                "workspace_path": null
            }],
            "blocked": [{
                "issue_id": "issue-blocked",
                "issue_identifier": "MT-BLOCKED",
                "issue_url": "https://example.org/issues/MT-BLOCKED",
                "state": "In Progress",
                "error": "codex turn requires operator input",
                "worker_host": "dm-dev2",
                "workspace_path": "/workspaces/MT-BLOCKED",
                "session_id": "thread-blocked",
                "blocked_at": state["blocked"][0]["blocked_at"],
                "last_event": "turn_input_required",
                "last_message": "turn blocked: waiting for user input",
                "last_event_at": state["blocked"][0]["last_event_at"]
            }],
            "codex_totals": {"input_tokens": 4, "output_tokens": 8, "total_tokens": 12, "seconds_running": 42.5},
            "rate_limits": {"primary": {"remaining": 11}}
        })
    );
    // Second-truncated `Z` timestamps.
    for pointer in [
        "/generated_at",
        "/running/0/started_at",
        "/retrying/0/due_at",
        "/blocked/0/blocked_at",
    ] {
        let text = state.pointer(pointer).and_then(Value::as_str).unwrap();
        assert!(
            text.ends_with('Z') && !text.contains('.'),
            "{pointer}: {text}"
        );
    }
    let due = timestamp(&state["retrying"][0]["due_at"]);
    let generated = timestamp(&state["generated_at"]);
    assert!((due - generated).num_seconds() >= 1 && (due - generated).num_seconds() <= 3);

    let (status, issue) = harness.get("/api/v1/MT-HTTP").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        issue,
        json!({
            "issue_identifier": "MT-HTTP",
            "issue_id": "issue-http",
            "status": "running",
            "workspace": {"path": "/tmp/symphony_workspaces/MT-HTTP", "host": null},
            "attempts": {"restart_count": 0, "current_retry_attempt": 0},
            "running": {
                "worker_host": null,
                "workspace_path": null,
                "session_id": "thread-http",
                "turn_count": 7,
                "state": "In Progress",
                "started_at": issue["running"]["started_at"],
                "last_event": "notification",
                "last_message": "rendered",
                "last_event_at": null,
                "tokens": {"input_tokens": 4, "output_tokens": 8, "total_tokens": 12}
            },
            "retry": null,
            "blocked": null,
            "logs": {"codex_session_logs": []},
            "recent_events": [],
            "last_error": null,
            "tracked": {}
        })
    );

    let (status, retry) = harness.get("/api/v1/MT-RETRY").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(retry["status"], "retrying");
    assert_eq!(retry["retry"]["attempt"], 2);
    assert_eq!(retry["retry"]["error"], "boom");
    assert_eq!(
        retry["attempts"],
        json!({"restart_count": 1, "current_retry_attempt": 2})
    );

    let (status, blocked) = harness.get("/api/v1/MT-BLOCKED").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["last_error"], "codex turn requires operator input");
    assert_eq!(blocked["blocked"]["session_id"], "thread-blocked");
    assert_eq!(blocked["blocked"]["state"], "In Progress");
    assert_eq!(
        blocked["blocked"]["error"],
        "codex turn requires operator input"
    );
    assert_eq!(
        blocked["workspace"],
        json!({"path": "/workspaces/MT-BLOCKED", "host": "dm-dev2"})
    );
    assert_eq!(blocked["recent_events"][0]["event"], "turn_input_required");

    let (status, missing) = harness.get("/api/v1/MT-MISSING").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing, envelope("issue_not_found", "Issue not found"));

    let (status, refresh) = harness.json(Method::POST, "/api/v1/refresh").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(refresh["queued"], true);
    assert_eq!(refresh["coalesced"], false);
    assert_eq!(refresh["operations"], json!(["poll", "reconcile"]));
    let requested_at = refresh["requested_at"].as_str().unwrap();
    assert_eq!(
        requested_at.len(),
        "2026-02-24T20:15:30.123456Z".len(),
        "microsecond precision: {requested_at}"
    );
}

#[tokio::test]
async fn method_not_allowed_not_found_and_unavailable() {
    let harness = Harness::new(StaticControlPlane::unavailable());
    let not_allowed = envelope("method_not_allowed", "Method not allowed");

    for (method, uri) in [
        (Method::POST, "/api/v1/state"),
        (Method::GET, "/api/v1/refresh"),
        (Method::POST, "/"),
        (Method::POST, "/api/v1/MT-1"),
        (Method::DELETE, "/api/v1/state"),
        (Method::PUT, "/api/v1/MT-1"),
        (Method::POST, "/api/v1/health"),
        (Method::POST, "/api/v1/events"),
        (Method::POST, "/api/v1/runs"),
        (Method::DELETE, "/api/v1/runs/1"),
        (Method::POST, "/api/v1/runs/1/events"),
        (Method::POST, "/api/v1/totals"),
        (Method::POST, "/api/openapi.json"),
        (Method::OPTIONS, "/api/v1/state"),
    ] {
        let (status, body) = harness.json(method.clone(), uri).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {uri}");
        assert_eq!(body, not_allowed, "{method} {uri}");
    }

    let not_found = envelope("not_found", "Route not found");
    for (method, uri) in [
        (Method::GET, "/unknown"),
        (Method::GET, "/api/v1/a/b"),
        (Method::GET, "/api/v1/a/b/c"),
        (Method::GET, "/api"),
        (Method::GET, "/api/v1"),
        (Method::GET, "/favicon.ico"),
        (Method::POST, "/favicon.png"),
        (Method::DELETE, "/assets/whatever.js"),
        (Method::GET, "/dashboard.css"),
    ] {
        let (status, body) = harness.json(method.clone(), uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(body, not_found, "{method} {uri}");
    }

    let (status, state) = harness.get("/api/v1/state").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state,
        json!({
            "generated_at": state["generated_at"],
            "error": {"code": "snapshot_unavailable", "message": "Snapshot unavailable"}
        })
    );

    let (status, body) = harness.json(Method::POST, "/api/v1/refresh").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        envelope("orchestrator_unavailable", "Orchestrator is unavailable")
    );

    // Any snapshot failure makes issue lookups 404.
    let (status, body) = harness.get("/api/v1/MT-HTTP").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, envelope("issue_not_found", "Issue not found"));
}

#[tokio::test(start_paused = true)]
async fn snapshot_timeout_is_reported_in_band() {
    let control = StaticControlPlane::new(Ok(static_snapshot()));
    control.set_delay(Some(Duration::from_millis(25)));
    let config = ServerConfig {
        snapshot_timeout: Duration::from_millis(1),
        refresh_timeout: Duration::from_millis(1),
        ..ServerConfig::default()
    };
    let harness = Harness::with(control, Store::disabled(), config);

    let (status, state) = harness.get("/api/v1/state").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state,
        json!({
            "generated_at": state["generated_at"],
            "error": {"code": "snapshot_timeout", "message": "Snapshot timed out"}
        })
    );
    let (status, _) = harness.get("/api/v1/MT-HTTP").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Elixir crashed into a 500 on a refresh timeout; the port answers 503.
    let (status, body) = harness.json(Method::POST, "/api/v1/refresh").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        envelope("orchestrator_unavailable", "Orchestrator is unavailable")
    );
}

#[tokio::test]
async fn state_error_values_render_in_band() {
    let harness = Harness::new(StaticControlPlane::new(Err(StateError::Timeout)));
    let (_, state) = harness.get("/api/v1/state").await;
    assert_eq!(state["error"]["code"], "snapshot_timeout");
}

#[tokio::test]
async fn reserved_identifiers_never_reach_the_issue_lookup() {
    let harness = Harness::snapshot();
    for path in ["state", "health", "events", "runs", "totals"] {
        let request = Request::builder()
            .method(Method::HEAD)
            .uri(format!("/api/v1/{path}"))
            .body(Body::empty())
            .unwrap();
        let response = harness.request(request).await;
        assert_ne!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let (status, _) = harness.json(Method::GET, "/api/v1/refresh").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let (status, health) = harness.get("/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["status"], "ok");
    let (status, _) = harness.get("/api/v1/totals").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "store disabled");
    assert_eq!(harness.control.issue_calls(), 0);
}

#[tokio::test]
async fn identifiers_are_url_decoded_and_case_sensitive() {
    let mut state = static_snapshot();
    state.running[0].issue_identifier = "MT/42 x".into();
    let harness = Harness::new(StaticControlPlane::new(Ok(state)));
    let (status, body) = harness.get("/api/v1/MT%2F42%20x").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["issue_identifier"], "MT/42 x");
    // Workspace fallback uses the hashed workspace key for unsafe identifiers.
    let path = body["workspace"]["path"].as_str().unwrap();
    assert!(
        path.starts_with("/tmp/symphony_workspaces/MT_42_x--"),
        "{path}"
    );

    let (status, _) = harness.get("/api/v1/mt%2F42%20x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Invalid UTF-8 cannot name an issue.
    let (status, body) = harness.get("/api/v1/%FF").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "issue_not_found");
}

#[tokio::test]
async fn trailing_slash_and_head_are_accepted() {
    let harness = Harness::snapshot();
    let (status, body) = harness.get("/api/v1/state/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["counts"]["running"], 1);
    let (status, body) = harness.get("/api/v1/MT-HTTP/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "running");

    let (status, headers, body) = harness.send(Method::HEAD, "/api/v1/state").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(headers["content-type"], "application/json");
}

#[tokio::test]
async fn refresh_ignores_the_request_body() {
    let harness = Harness::snapshot();
    harness.control.set_refresh(Ok(true));
    for (content_type, body) in [
        ("application/x-www-form-urlencoded", ""),
        ("application/json", "{}"),
        ("application/json", "not json"),
        ("text/plain", "anything"),
    ] {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/refresh")
            .header("content-type", content_type)
            .body(Body::from(body))
            .unwrap();
        let response = harness.request(request).await;
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{content_type}");
    }
    let (_, body) = harness.json(Method::POST, "/api/v1/refresh").await;
    assert_eq!(body["coalesced"], true);
    assert_eq!(harness.control.refresh_calls(), 5);

    // A form POST to a GET route is still a 405 (no `_method` override).
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/state?_method=GET")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("_method=GET"))
        .unwrap();
    assert_eq!(
        harness.request(request).await.status(),
        StatusCode::METHOD_NOT_ALLOWED
    );

    harness.control.set_refresh(Err(Unavailable));
    let (status, _) = harness.json(Method::POST, "/api/v1/refresh").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn health_reports_version_uptime_and_store_mode() {
    let harness = Harness::snapshot();
    let (status, health) = harness.get("/api/v1/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        health,
        json!({"status": "ok", "version": env!("CARGO_PKG_VERSION"), "uptime_seconds": 0, "store": "disabled"})
    );
    assert_eq!(
        harness.control.state_calls(),
        0,
        "health never touches the orchestrator"
    );

    let store = Store::open_in_memory().unwrap();
    let harness = Harness::with(
        StaticControlPlane::unavailable(),
        store,
        ServerConfig {
            version: "9.9.9".into(),
            ..ServerConfig::default()
        },
    );
    let (_, health) = harness.get("/api/v1/health").await;
    assert_eq!(health["store"], "sqlite");
    assert_eq!(health["version"], "9.9.9");
}

#[tokio::test]
async fn openapi_document_is_served_as_json() {
    let harness = Harness::snapshot();
    let (status, doc) = harness.get("/api/openapi.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["openapi"], "3.1.0");
    let yaml: Value = serde_yaml_ng::from_str(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/api/openapi.yaml"
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(doc, yaml, "served document equals docs/api/openapi.yaml");
    let paths: Vec<&str> = doc["paths"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for path in [
        "/api/v1/state",
        "/api/v1/refresh",
        "/api/v1/health",
        "/api/v1/events",
        "/api/v1/runs",
        "/api/v1/runs/{id}",
        "/api/v1/runs/{id}/events",
        "/api/v1/totals",
        "/api/v1/{issue_identifier}",
        "/api/openapi.json",
    ] {
        assert!(paths.contains(&path), "{path}");
    }
}

#[tokio::test]
async fn security_headers_and_request_ids_are_set() {
    let harness = Harness::snapshot();
    for uri in ["/api/v1/state", "/", "/unknown"] {
        let (_, headers, _) = harness.send(Method::GET, uri).await;
        assert_eq!(headers["x-content-type-options"], "nosniff", "{uri}");
        assert_eq!(headers["x-frame-options"], "SAMEORIGIN", "{uri}");
        assert_eq!(
            headers["referrer-policy"], "strict-origin-when-cross-origin",
            "{uri}"
        );
        assert_eq!(headers["x-permitted-cross-domain-policies"], "none");
        let csp = headers["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'self'") && csp.contains("script-src 'self'"));
        assert!(headers.contains_key("x-request-id"), "{uri}");
        assert!(!headers.contains_key("access-control-allow-origin"));
    }
}

#[tokio::test]
async fn cors_is_off_by_default_and_follows_the_allow_list() {
    let config = ServerConfig {
        cors_allowed_origins: vec!["http://localhost:5173".into()],
        ..ServerConfig::default()
    };
    let harness = Harness::with(
        StaticControlPlane::new(Ok(static_snapshot())),
        Store::disabled(),
        config,
    );
    let allowed = Request::builder()
        .uri("/api/v1/state")
        .header("origin", "http://localhost:5173")
        .body(Body::empty())
        .unwrap();
    let response = harness.request(allowed).await;
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "http://localhost:5173"
    );
    let other = Request::builder()
        .uri("/api/v1/state")
        .header("origin", "http://evil.example")
        .body(Body::empty())
        .unwrap();
    let response = harness.request(other).await;
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
    let preflight = Request::builder()
        .method(Method::OPTIONS)
        .uri("/api/v1/refresh")
        .header("origin", "http://localhost:5173")
        .header("access-control-request-method", "POST")
        .body(Body::empty())
        .unwrap();
    let response = harness.request(preflight).await;
    assert!(response.status().is_success());
    assert!(
        response.headers()["access-control-allow-methods"]
            .to_str()
            .unwrap()
            .contains("POST")
    );
}

#[tokio::test]
async fn handler_panics_become_request_failed() {
    let harness = Harness::snapshot();
    harness.control.set_panic_on_state(true);
    let (status, body) = harness.get("/api/v1/state").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, envelope("request_failed", "Internal Server Error"));
}

#[tokio::test(start_paused = true)]
async fn slow_requests_hit_the_request_timeout() {
    let control = StaticControlPlane::new(Ok(static_snapshot()));
    control.set_delay(Some(Duration::from_secs(60)));
    let config = ServerConfig {
        snapshot_timeout: Duration::from_secs(120),
        request_timeout: Duration::from_secs(2),
        ..ServerConfig::default()
    };
    let harness = Harness::with(control, Store::disabled(), config);
    let started = tokio::time::Instant::now();
    let (status, body) = harness.get("/api/v1/state").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, envelope("request_failed", "Service Unavailable"));
    assert_eq!(started.elapsed(), Duration::from_secs(2));
}

#[tokio::test]
async fn json_responses_are_compressed_when_asked() {
    let mut state = static_snapshot();
    // Make the body comfortably larger than the compression threshold.
    state.rate_limits = Some(json!({"padding": "x".repeat(4096)}));
    let harness = Harness::new(StaticControlPlane::new(Ok(state)));
    let request = Request::builder()
        .uri("/api/v1/state")
        .header("accept-encoding", "gzip")
        .body(Body::empty())
        .unwrap();
    let response = harness.request(request).await;
    assert_eq!(response.headers()["content-encoding"], "gzip");
}
