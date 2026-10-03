//! Shared helpers for the integration tests.
#![allow(dead_code)]

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde_json::{Value, json};
use symphony_server::testing::StaticControlPlane;
use symphony_server::view::{BlockedEntry, CodexTotals, RetryEntry, RunningEntry, TokenCounts};
use symphony_server::{App, ServerConfig, StateView, presenter, router};
use symphony_store::Store;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// The Elixir `extensions_test.exs` `static_snapshot/0`, already projected.
pub fn static_snapshot() -> StateView {
    let now = Utc::now();
    StateView {
        running: vec![RunningEntry {
            issue_id: "issue-http".into(),
            issue_identifier: "MT-HTTP".into(),
            issue_url: Some("https://example.org/issues/MT-HTTP".into()),
            state: Some("In Progress".into()),
            worker_host: None,
            workspace_path: None,
            session_id: Some("thread-http".into()),
            turn_count: 7,
            last_event: Some("notification".into()),
            last_message: Some("rendered".into()),
            started_at: Some(now),
            last_event_at: None,
            tokens: TokenCounts {
                input_tokens: 4,
                output_tokens: 8,
                total_tokens: 12,
            },
        }],
        retrying: vec![RetryEntry {
            issue_id: "issue-retry".into(),
            issue_identifier: "MT-RETRY".into(),
            issue_url: Some("https://example.org/issues/MT-RETRY".into()),
            attempt: Some(2),
            due_at: presenter::due_at(now, Some(2_000)),
            error: Some("boom".into()),
            worker_host: None,
            workspace_path: None,
        }],
        blocked: vec![BlockedEntry {
            issue_id: "issue-blocked".into(),
            issue_identifier: "MT-BLOCKED".into(),
            issue_url: Some("https://example.org/issues/MT-BLOCKED".into()),
            state: Some("In Progress".into()),
            error: Some("codex turn requires operator input".into()),
            worker_host: Some("dm-dev2".into()),
            workspace_path: Some("/workspaces/MT-BLOCKED".into()),
            session_id: Some("thread-blocked".into()),
            blocked_at: Some(now),
            last_event: Some("turn_input_required".into()),
            last_message: Some("turn blocked: waiting for user input".into()),
            last_event_at: Some(now),
        }],
        codex_totals: CodexTotals {
            input_tokens: 4,
            output_tokens: 8,
            total_tokens: 12,
            seconds_running: 42.5,
        },
        rate_limits: Some(json!({"primary": {"remaining": 11}})),
    }
}

/// A state with `n` running issues (used to tell snapshots apart).
pub fn running_state(n: usize) -> StateView {
    let template = static_snapshot().running.remove(0);
    StateView {
        running: (0..n)
            .map(|i| RunningEntry {
                issue_id: format!("issue-{i}"),
                issue_identifier: format!("MT-{i}"),
                ..template.clone()
            })
            .collect(),
        ..StateView::default()
    }
}

pub fn timestamp(value: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value.as_str().expect("timestamp string"))
        .expect("RFC 3339 timestamp")
        .with_timezone(&Utc)
}

/// A test harness: the app plus the handles the tests poke.
pub struct Harness {
    pub app: App,
    pub control: Arc<StaticControlPlane>,
    pub shutdown: CancellationToken,
    pub store: Store,
}

impl Harness {
    pub fn new(control: StaticControlPlane) -> Self {
        Harness::with(control, Store::disabled(), ServerConfig::default())
    }

    pub fn with(control: StaticControlPlane, store: Store, config: ServerConfig) -> Self {
        let control = Arc::new(control);
        let shutdown = CancellationToken::new();
        let app = router(&config, control.clone(), store.clone(), shutdown.clone());
        Harness {
            app,
            control,
            shutdown,
            store,
        }
    }

    pub fn snapshot() -> Self {
        Harness::new(StaticControlPlane::new(Ok(static_snapshot())))
    }

    pub async fn request(&self, request: Request<Body>) -> axum::response::Response {
        self.app
            .clone()
            .oneshot(request)
            .await
            .expect("infallible service")
    }

    pub async fn send(&self, method: Method, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("request");
        let response = self.request(request).await;
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .expect("body");
        (status, headers, body)
    }

    pub async fn json(&self, method: Method, uri: &str) -> (StatusCode, Value) {
        let (status, headers, body) = self.send(method, uri).await;
        assert!(
            headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json")),
            "{uri}: expected JSON, got {headers:?}"
        );
        (status, serde_json::from_slice(&body).expect("JSON body"))
    }

    pub async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.json(Method::GET, uri).await
    }
}

pub fn envelope(code: &str, message: &str) -> Value {
    json!({"error": {"code": code, "message": message}})
}

/// One parsed SSE event.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub id: Option<String>,
    pub data: Option<String>,
    pub retry: Option<u64>,
}

impl SseEvent {
    pub fn json(&self) -> Value {
        serde_json::from_str(self.data.as_deref().expect("data")).expect("JSON data")
    }
}

/// Incremental SSE parser over a response body.
pub struct SseReader {
    stream: futures::stream::BoxStream<'static, Result<Bytes, axum::Error>>,
    buffer: String,
}

impl SseReader {
    pub fn new(response: axum::response::Response) -> Self {
        SseReader {
            stream: response.into_body().into_data_stream().boxed(),
            buffer: String::new(),
        }
    }

    /// The next event, `None` when the stream ended.
    pub async fn next(&mut self) -> Option<SseEvent> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                return Some(parse_block(&block));
            }
            match self.stream.next().await {
                Some(Ok(chunk)) => self
                    .buffer
                    .push_str(std::str::from_utf8(&chunk).expect("UTF-8 stream")),
                Some(Err(err)) => panic!("stream error: {err}"),
                None => return None,
            }
        }
    }
}

pub fn parse_block(block: &str) -> SseEvent {
    let mut event = SseEvent::default();
    for line in block.lines() {
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value).to_owned();
        match field {
            "event" => event.event = Some(value),
            "id" => event.id = Some(value),
            "data" => {
                event.data = Some(match event.data.take() {
                    Some(previous) => format!("{previous}\n{value}"),
                    None => value,
                })
            }
            "retry" => event.retry = value.parse().ok(),
            _ => {}
        }
    }
    event
}
