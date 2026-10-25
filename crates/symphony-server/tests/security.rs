//! API token authentication, the Host allow-list, the Origin check and SSE tickets.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{Harness, SseReader, envelope, static_snapshot};
use serde_json::Value;
use symphony_server::testing::StaticControlPlane;
use symphony_server::{Auth, ServerConfig, ServerError, serve};
use symphony_store::Store;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-token-0123456789";

fn secured() -> Harness {
    Harness::with(
        StaticControlPlane::new(Ok(static_snapshot())),
        Store::open_in_memory().unwrap(),
        ServerConfig {
            auth: Auth::token(TOKEN),
            ..ServerConfig::default()
        },
    )
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).unwrap()
}

async fn status_and_json(
    harness: &Harness,
    request: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = harness.request(request).await;
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (status, headers, json)
}

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

#[tokio::test]
async fn api_routes_require_the_token_but_health_contract_and_assets_do_not() {
    let harness = secured();
    for uri in [
        "/api/v1/state",
        "/api/v1/runs",
        "/api/v1/runs/1/events",
        "/api/v1/totals",
        "/api/v1/MT-HTTP",
    ] {
        let (status, headers, body) =
            status_and_json(&harness, request(Method::GET, uri, &[])).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            body,
            envelope("unauthorized", "Missing or invalid API token"),
            "{uri}"
        );
        assert_eq!(headers["www-authenticate"], "Bearer realm=\"symphony\"");

        let (status, _, _) = status_and_json(
            &harness,
            request(Method::GET, uri, &[("authorization", "Bearer wrong")]),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let (status, _, _) =
        status_and_json(&harness, request(Method::POST, "/api/v1/refresh", &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, state) = status_and_json(
        &harness,
        request(
            Method::GET,
            "/api/v1/state",
            &[("authorization", &bearer())],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["counts"]["running"], 1);

    let (status, _, health) =
        status_and_json(&harness, request(Method::GET, "/api/v1/health", &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["auth_required"], true);
    let (status, _, _) =
        status_and_json(&harness, request(Method::GET, "/api/openapi.json", &[])).await;
    assert_eq!(status, StatusCode::OK);
    let response = harness.request(request(Method::GET, "/", &[])).await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn foreign_host_headers_are_refused_even_with_a_token() {
    let harness = secured();
    for host in ["evil.example", "evil.example:4000", "192.168.1.9:4000"] {
        let (status, _, body) = status_and_json(
            &harness,
            request(
                Method::GET,
                "/api/v1/state",
                &[("host", host), ("authorization", &bearer())],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{host}");
        assert_eq!(
            body,
            envelope("host_not_allowed", "Host header is not allowed")
        );
    }
    // A rebinding page also cannot fetch the dashboard shell.
    let response = harness
        .request(request(Method::GET, "/", &[("host", "evil.example")]))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    for host in ["127.0.0.1:4000", "localhost:4000", "[::1]:4000"] {
        let (status, _, _) = status_and_json(
            &harness,
            request(
                Method::GET,
                "/api/v1/state",
                &[("host", host), ("authorization", &bearer())],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{host}");
    }

    let named = Harness::with(
        StaticControlPlane::new(Ok(static_snapshot())),
        Store::disabled(),
        ServerConfig {
            allowed_hosts: vec!["symphony.internal".into()],
            ..ServerConfig::default()
        },
    );
    let (status, _, _) = status_and_json(
        &named,
        request(
            Method::GET,
            "/api/v1/state",
            &[("host", "symphony.internal:8080")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn cross_origin_writes_are_refused() {
    let harness = Harness::snapshot();
    let refresh = |headers: &[(&str, &str)]| request(Method::POST, "/api/v1/refresh", headers);
    let (status, _, body) = status_and_json(
        &harness,
        refresh(&[
            ("host", "127.0.0.1:4000"),
            ("origin", "https://evil.example"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        envelope("origin_not_allowed", "Cross-origin request refused")
    );
    let (status, _, _) = status_and_json(
        &harness,
        refresh(&[("host", "127.0.0.1:4000"), ("sec-fetch-site", "cross-site")]),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(harness.control.refresh_calls(), 0);

    for headers in [
        vec![
            ("host", "127.0.0.1:4000"),
            ("origin", "http://127.0.0.1:4000"),
        ],
        vec![("host", "127.0.0.1:4000")],
        vec![],
    ] {
        let (status, _, _) = status_and_json(&harness, refresh(&headers)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{headers:?}");
    }
    // Reads are not origin-checked (CORS keeps their responses from other origins).
    let (status, _, _) = status_and_json(
        &harness,
        request(
            Method::GET,
            "/api/v1/state",
            &[("origin", "https://evil.example")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let allowed = Harness::with(
        StaticControlPlane::new(Ok(static_snapshot())),
        Store::disabled(),
        ServerConfig {
            cors_allowed_origins: vec!["https://dash.example".into()],
            ..ServerConfig::default()
        },
    );
    let (status, _, _) = status_and_json(
        &allowed,
        refresh(&[
            ("host", "127.0.0.1:4000"),
            ("origin", "https://dash.example"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn sse_streams_open_with_a_single_use_ticket() {
    let harness = secured();
    let (status, _, _) =
        status_and_json(&harness, request(Method::GET, "/api/v1/events", &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = status_and_json(
        &harness,
        request(Method::POST, "/api/v1/events/tickets", &[]),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "tickets need the token");

    let (status, _, ticket) = status_and_json(
        &harness,
        request(
            Method::POST,
            "/api/v1/events/tickets",
            &[("authorization", &bearer())],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(ticket["expires_in_ms"], 60_000);
    let ticket = ticket["ticket"].as_str().unwrap().to_owned();
    let uri = format!("/api/v1/events?ticket={ticket}");

    let response = harness.request(request(Method::GET, &uri, &[])).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut reader = SseReader::new(response);
    let first = reader.next().await.unwrap();
    assert_eq!(first.retry, Some(3000));
    let snapshot = reader.next().await.unwrap();
    assert_eq!(snapshot.event.as_deref(), Some("snapshot"));
    drop(reader);

    let (status, _, _) = status_and_json(&harness, request(Method::GET, &uri, &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a ticket works once");
    // A ticket is no key to the rest of the API.
    let (status, _, _) = status_and_json(
        &harness,
        request(Method::GET, &format!("/api/v1/state?ticket={ticket}"), &[]),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The bearer token works for the stream too (non-browser clients).
    let response = harness
        .request(request(
            Method::GET,
            "/api/v1/events",
            &[("authorization", &bearer())],
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    harness.shutdown.cancel();
}

#[tokio::test]
async fn non_loopback_binds_need_auth() {
    let control = Arc::new(StaticControlPlane::new(Ok(static_snapshot())));
    let err = serve(
        ServerConfig::new("0.0.0.0", 0),
        control.clone(),
        Store::disabled(),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ServerError::AuthRequired { .. }), "{err}");
    assert!(err.to_string().starts_with("auth_required:"), "{err}");

    let shutdown = CancellationToken::new();
    let server = serve(
        ServerConfig {
            auth: Auth::token(TOKEN),
            ..ServerConfig::new("0.0.0.0", 0)
        },
        control,
        Store::disabled(),
        shutdown.clone(),
    )
    .await
    .unwrap();
    let base = format!("http://127.0.0.1:{}", server.port());
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let denied = http
        .get(format!("{base}/api/v1/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let allowed = http
        .get(format!("{base}/api/v1/state"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), 200);
    shutdown.cancel();
    server.wait().await.unwrap();
}
