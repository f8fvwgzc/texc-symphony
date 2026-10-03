//! Shared integration-test helpers: settings from JSON front matter, mock-server wiring through
//! logical origins, and a log capture.

#![allow(dead_code)]

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use symphony_core::config::{self, Settings, TrackerSettings};
use symphony_core::{EnvSource, MapEnv};
use symphony_trackers::transport::{RawRequest, RawResponse};
use symphony_trackers::{
    HttpClient, ReqwestTransport, RetryPolicy, ToolContext, Tracker, TrackerDeps, Transport,
    TransportError,
};
use wiremock::MockServer;

/// Parses `{"tracker": tracker}` with `env`.
pub fn settings_with_env(tracker: Value, env: &MapEnv) -> Settings {
    let Value::Object(map) = json!({ "tracker": tracker }) else {
        unreachable!()
    };
    config::parse(&map, env).expect("front matter parses")
}

/// Parses `{"tracker": tracker}` with an empty environment.
pub fn tracker_settings(tracker: Value) -> TrackerSettings {
    settings_with_env(tracker, &MapEnv::new()).tracker
}

/// Fast retry policy so retry tests take milliseconds.
pub fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(50),
    }
}

/// An HTTP client that routes each logical origin (e.g. `https://github.test`) to a mock server.
pub fn http_for(routes: &[(&str, &MockServer)]) -> HttpClient {
    let mut transport = ReqwestTransport::new().expect("reqwest client");
    for (logical, server) in routes {
        transport = transport
            .with_origin_override(logical, &server.uri())
            .expect("valid override");
    }
    HttpClient::new(Arc::new(transport)).with_retry_policy(fast_retry())
}

/// Dependencies with `env` and mock routes.
pub fn deps_for(env: MapEnv, routes: &[(&str, &MockServer)]) -> TrackerDeps {
    TrackerDeps::with_http(Arc::new(env), http_for(routes))
}

/// A transport that refuses every request (proves "no request was made" without touching the network).
#[derive(Debug)]
pub struct NoNetwork;

#[async_trait::async_trait]
impl Transport for NoNetwork {
    async fn send(&self, _request: RawRequest) -> Result<RawResponse, TransportError> {
        panic!("unexpected network request in a test that must not send one");
    }
}

/// Dependencies whose transport panics on any request.
pub fn offline_deps(env: MapEnv) -> TrackerDeps {
    TrackerDeps::with_http(Arc::new(env), HttpClient::new(Arc::new(NoNetwork)))
}

/// Tool context with `settings`.
pub fn ctx(settings: &TrackerSettings) -> ToolContext {
    ToolContext {
        settings: Arc::new(settings.clone()),
        issue: None,
    }
}

/// Executes a tool and returns (success, decoded output).
pub async fn run_tool(
    tracker: &dyn Tracker,
    tool: &str,
    args: Value,
    settings: &TrackerSettings,
) -> (bool, Value) {
    let result = tracker
        .execute_agent_tool(Some(tool), &args, &ctx(settings))
        .await;
    assert_eq!(result.content_items.len(), 1);
    assert_eq!(result.content_items[0].kind, "inputText");
    assert_eq!(result.content_items[0].text, result.output);
    let decoded = serde_json::from_str(&result.output).unwrap_or(Value::String(result.output));
    (result.success, decoded)
}

/// Captured log output.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl LogCapture {
    /// Everything logged so far.
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs a thread-local subscriber capturing all events (use with current-thread runtimes).
pub fn capture_logs() -> (LogCapture, tracing::subscriber::DefaultGuard) {
    let capture = LogCapture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (capture, guard)
}

/// Env lookups for assertions.
pub fn env_has(env: &MapEnv, name: &str) -> bool {
    env.var(name).is_some()
}

/// `["a", "b"]` -> `Vec<String>`.
pub fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Ids of issues.
pub fn ids(issues: &[symphony_core::Issue]) -> Vec<String> {
    issues.iter().filter_map(|i| i.id.clone()).collect()
}

/// Query pairs of a received request, in order.
pub fn query_of(request: &wiremock::Request) -> Vec<(String, String)> {
    request
        .url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Header value of a received request.
pub fn header_of(request: &wiremock::Request, name: &str) -> Option<String> {
    request
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// JSON body of a received request.
pub fn json_body(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).expect("json body")
}
