//! Req-default parity for the HTTP layer: retries, redirects, decoding, timeouts and credential
//! scrubbing (behaviour the Elixir suite got implicitly from Req).

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use support::*;
use symphony_core::MapEnv;
use symphony_trackers::github::GitHubTracker;
use symphony_trackers::transport::{RawRequest, RawResponse};
use symphony_trackers::{
    HttpClient, HttpRequest, Method, ReqwestTransport, RetryPolicy, Transport, TransportError,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGIN: &str = "https://api.test";

fn get(path: &str) -> HttpRequest {
    HttpRequest::new(Method::Get, format!("{ORIGIN}{path}"))
}

#[tokio::test]
async fn get_is_retried_on_transient_statuses_until_success() {
    let server = MockServer::start().await;
    Mock::given(path("/flaky"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/flaky"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .with_priority(2)
        .mount(&server)
        .await;
    let (logs, _guard) = capture_logs();
    let response = http_for(&[(ORIGIN, &server)])
        .send(get("/flaky"))
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, json!({"ok": true}));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert!(
        logs.contents()
            .contains("retry: got response with status 503, will retry in")
    );
}

#[tokio::test]
async fn retries_stop_after_three_and_return_the_final_status() {
    for status in [408, 429, 500, 502, 503, 504] {
        let server = MockServer::start().await;
        Mock::given(path("/down"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let response = http_for(&[(ORIGIN, &server)])
            .send(get("/down"))
            .await
            .unwrap();
        assert_eq!(response.status, status);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            4,
            "status {status}"
        );
    }
}

#[tokio::test]
async fn non_safe_methods_and_non_transient_statuses_are_not_retried() {
    let server = MockServer::start().await;
    Mock::given(path("/x"))
        .and(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(path("/missing"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let http = http_for(&[(ORIGIN, &server)]);
    let post = HttpRequest::new(Method::Post, format!("{ORIGIN}/x")).json(Some(json!({})));
    assert_eq!(http.send(post).await.unwrap().status, 503);
    assert_eq!(http.send(get("/missing")).await.unwrap().status, 404);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn retry_after_is_honored_but_capped() {
    let server = MockServer::start().await;
    Mock::given(path("/limited"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(path("/limited"))
        .respond_with(ResponseTemplate::new(200))
        .with_priority(2)
        .mount(&server)
        .await;
    let started = Instant::now();
    let http = http_for(&[(ORIGIN, &server)]).with_retry_policy(RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(200),
    });
    assert_eq!(http.send(get("/limited")).await.unwrap().status, 200);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(200),
        "waited for the capped Retry-After"
    );
    assert!(elapsed < Duration::from_secs(5));
}

#[tokio::test]
async fn transient_transport_errors_are_retried_for_get() {
    // A port with nothing listening: connection refused.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let transport = ReqwestTransport::new()
        .unwrap()
        .with_origin_override(ORIGIN, &format!("http://127.0.0.1:{port}"))
        .unwrap();
    let http = HttpClient::new(Arc::new(transport)).with_retry_policy(fast_retry());
    let (logs, _guard) = capture_logs();
    assert_eq!(
        http.send(get("/x")).await,
        Err(TransportError::ConnectionRefused)
    );
    assert_eq!(
        logs.contents()
            .matches("retry: got exception :econnrefused")
            .count(),
        3
    );
}

#[tokio::test]
async fn receive_timeouts_surface_as_timeout() {
    let server = MockServer::start().await;
    Mock::given(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(500)))
        .mount(&server)
        .await;
    let transport =
        ReqwestTransport::with_timeouts(Duration::from_secs(5), Duration::from_millis(100))
            .unwrap()
            .with_origin_override(ORIGIN, &server.uri())
            .unwrap();
    let http = HttpClient::new(Arc::new(transport)).with_retry_policy(RetryPolicy {
        max_retries: 0,
        ..fast_retry()
    });
    assert_eq!(http.send(get("/slow")).await, Err(TransportError::Timeout));
}

#[tokio::test]
async fn see_other_redirects_turn_post_into_get_and_temporary_redirects_keep_the_body() {
    let server = MockServer::start().await;
    Mock::given(path("/create"))
        .respond_with(ResponseTemplate::new(303).insert_header("location", "/result"))
        .mount(&server)
        .await;
    Mock::given(path("/temp"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/result"))
        .mount(&server)
        .await;
    Mock::given(path("/result"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"done": true})))
        .mount(&server)
        .await;
    let http = http_for(&[(ORIGIN, &server)]);
    let post = |p: &str| {
        HttpRequest::new(Method::Post, format!("{ORIGIN}{p}")).json(Some(json!({"a": 1})))
    };

    assert_eq!(
        http.send(post("/create")).await.unwrap().body,
        json!({"done": true})
    );
    assert_eq!(http.send(post("/temp")).await.unwrap().status, 200);
    let requests = server.received_requests().await.unwrap();
    let results: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path() == "/result")
        .collect();
    assert_eq!(results[0].method.as_str(), "GET");
    assert!(results[0].body.is_empty());
    assert!(header_of(results[0], "content-type").is_none());
    assert_eq!(results[1].method.as_str(), "POST");
    assert_eq!(json_body(results[1]), json!({"a": 1}));
}

#[tokio::test]
async fn redirect_loops_stop_after_ten_hops() {
    let server = MockServer::start().await;
    Mock::given(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;
    let http = http_for(&[(ORIGIN, &server)]);
    assert_eq!(
        http.send(get("/loop")).await,
        Err(TransportError::TooManyRedirects)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 11);
}

#[tokio::test]
async fn credentials_stay_stripped_after_leaving_the_origin() {
    let origin = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(path("/start"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://other.test/bounce"),
        )
        .mount(&origin)
        .await;
    Mock::given(path("/bounce"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "https://api.test/back"))
        .mount(&other)
        .await;
    Mock::given(path("/back"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin)
        .await;
    let http = http_for(&[(ORIGIN, &origin), ("https://other.test", &other)]);
    let request = get("/start").credential_header("Authorization", "Bearer tok-1234", "tok-1234");
    assert_eq!(http.send(request).await.unwrap().status, 200);
    let origin_requests = origin.received_requests().await.unwrap();
    assert_eq!(
        header_of(&origin_requests[0], "authorization").as_deref(),
        Some("Bearer tok-1234")
    );
    let back = origin_requests
        .iter()
        .find(|r| r.url.path() == "/back")
        .unwrap();
    assert!(header_of(back, "authorization").is_none());
    assert!(
        header_of(
            &other.received_requests().await.unwrap()[0],
            "authorization"
        )
        .is_none()
    );
}

#[tokio::test]
async fn bodies_decode_by_content_type() {
    let server = MockServer::start().await;
    Mock::given(path("/vendor"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(r#"{"a":1}"#, "application/vnd.api+json"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/empty"))
        .respond_with(ResponseTemplate::new(204).insert_header("content-type", "application/json"))
        .mount(&server)
        .await;
    Mock::given(path("/broken"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{not json", "application/json"))
        .mount(&server)
        .await;
    Mock::given(path("/text"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"looks":"json"}"#))
        .mount(&server)
        .await;
    let http = http_for(&[(ORIGIN, &server)]);
    assert_eq!(
        http.send(get("/vendor")).await.unwrap().body,
        json!({"a": 1})
    );
    assert_eq!(http.send(get("/empty")).await.unwrap().body, json!(""));
    assert!(matches!(
        http.send(get("/broken")).await,
        Err(TransportError::InvalidJson(_))
    ));
    assert_eq!(
        http.send(get("/text")).await.unwrap().body,
        Value::String(r#"{"looks":"json"}"#.into())
    );
}

#[tokio::test]
async fn query_params_are_form_encoded_and_appended_to_inline_queries() {
    let server = MockServer::start().await;
    Mock::given(path("/q"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let http = http_for(&[(ORIGIN, &server)]);
    let request = HttpRequest::new(Method::Get, format!("{ORIGIN}/q?expand=a"))
        .query(vec![("jql".into(), "status = \"To Do\"".into())]);
    http.send(request).await.unwrap();
    http.send(get("/q")).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        query_of(&requests[0]),
        vec![
            ("expand".to_string(), "a".to_string()),
            ("jql".into(), "status = \"To Do\"".into())
        ]
    );
    assert!(
        requests[0]
            .url
            .as_str()
            .contains("jql=status+%3D+%22To+Do%22")
    );
    assert_eq!(
        requests[1].url.query(),
        None,
        "no trailing ? without params"
    );
}

/// A transport whose failures echo the credential, as a careless library error might.
#[derive(Debug)]
struct LeakyTransport;

#[async_trait]
impl Transport for LeakyTransport {
    async fn send(&self, request: RawRequest) -> Result<RawResponse, TransportError> {
        let auth = request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        Err(TransportError::Other(format!(
            "handshake failed for header {auth} at https://user:pw@api.test"
        )))
    }
}

#[tokio::test]
async fn transport_errors_are_scrubbed_before_reaching_tool_output() {
    let http = HttpClient::new(Arc::new(LeakyTransport));
    let err = http
        .send(get("/x").credential_header(
            "Authorization",
            "Bearer ghp_secret_value",
            "ghp_secret_value",
        ))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(!text.contains("ghp_secret_value"), "{text}");
    assert!(!text.contains("user:pw"), "{text}");

    let tracker = GitHubTracker::new(http, Arc::new(MapEnv::new()));
    let settings = tracker_settings(json!({
        "kind": "github",
        "provider": {"repo": "octo/repo", "token": "ghp_secret_value", "api_url": ORIGIN},
        "active_states": ["open"],
        "terminal_states": ["closed"],
    }));
    let result = symphony_trackers::Tracker::execute_agent_tool(
        &tracker,
        Some("github_api"),
        &json!({"method": "POST", "path": "/user"}),
        &ctx(&settings),
    )
    .await;
    assert!(!result.success);
    assert!(
        !result.output.contains("ghp_secret_value"),
        "{}",
        result.output
    );
    assert!(result.output.contains("[REDACTED]"));
    let read =
        symphony_trackers::Tracker::fetch_issues_by_ids(&tracker, &settings, &strings(&["1"]))
            .await
            .unwrap_err();
    assert!(!format!("{read} {read:?}").contains("ghp_secret_value"));
}
