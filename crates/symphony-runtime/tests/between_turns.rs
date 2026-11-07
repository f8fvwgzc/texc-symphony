//! The between-turns issue read on real adapters: a brief retry must not outlast the stall timeout,
//! repeat a read that spent the transport's full backoff, or ask again inside a rate limit.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use support::{TestWorkflow, issue};
use symphony_core::config::TrackerSettings;
use symphony_core::{Issue, MapEnv};
use symphony_runtime::runner::continue_with_issue;
use symphony_runtime::{FetchError, LiveIssueFetcher, TrackerClient};
use symphony_trackers::transport::{RawRequest, RawResponse};
use symphony_trackers::{
    GitHubTracker, HttpClient, LinearTracker, Tracker, TrackerDeps, TrackerError, Transport,
    TransportError,
};
use tokio::time::Instant;

/// Accepts every read and never answers.
struct Hung;

#[async_trait]
impl Tracker for Hung {
    fn kind(&self) -> &'static str {
        "memory"
    }

    async fn fetch_issues_by_states(
        &self,
        _settings: &TrackerSettings,
        _states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        std::future::pending().await
    }

    async fn fetch_issues_by_ids(
        &self,
        _settings: &TrackerSettings,
        _ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        std::future::pending().await
    }

    fn secret_environment_names(&self, _settings: &TrackerSettings) -> Vec<String> {
        Vec::new()
    }
}

/// Gives every request the same empty response and notes when each one arrived.
#[derive(Debug)]
struct Canned {
    status: u16,
    headers: Vec<(String, String)>,
    began: Instant,
    arrivals: Mutex<Vec<Duration>>,
}

impl Canned {
    fn new(status: u16, headers: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            status,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            began: Instant::now(),
            arrivals: Mutex::new(Vec::new()),
        })
    }

    fn arrivals(&self) -> Vec<Duration> {
        self.arrivals.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for Canned {
    async fn send(&self, _request: RawRequest) -> Result<RawResponse, TransportError> {
        self.arrivals.lock().unwrap().push(self.began.elapsed());
        Ok(RawResponse {
            status: self.status,
            headers: self.headers.clone(),
            body: Vec::new(),
        })
    }
}

fn github(transport: Arc<dyn Transport>) -> (TestWorkflow, Arc<dyn Tracker>) {
    let wf = TestWorkflow::new(json!({"tracker": {
        "kind": "github",
        "provider": {"repo": "octo/repo", "token": "test-token", "api_url": "https://github.test"},
        "required_labels": ["symphony"],
        "active_states": ["open"],
        "terminal_states": ["closed"],
    }}));
    let tracker = GitHubTracker::new(HttpClient::new(transport), Arc::new(MapEnv::new()));
    (wf, Arc::new(tracker))
}

fn linear(transport: Arc<dyn Transport>) -> (TestWorkflow, Arc<dyn Tracker>) {
    let wf = TestWorkflow::new(json!({"tracker": {
        "kind": "linear",
        "api_key": "test-token",
        "project_slug": "proj",
    }}));
    (wf, Arc::new(LinearTracker::new(HttpClient::new(transport))))
}

async fn read_through(
    wf: &TestWorkflow,
    tracker: Arc<dyn Tracker>,
    state: &str,
) -> Result<(), FetchError> {
    let client = TrackerClient::new(TrackerDeps::new().unwrap()).with_tracker(tracker);
    let fetcher = LiveIssueFetcher::new(Arc::clone(&wf.store), client);
    let active = issue("1", "MT-1", state);
    continue_with_issue(&active, &fetcher, &wf.store.settings())
        .await
        .map(|_| ())
}

#[tokio::test(start_paused = true)]
async fn a_read_that_timed_out_is_not_repeated() {
    let wf = TestWorkflow::new(json!({}));
    let began = Instant::now();
    let result = read_through(&wf, Arc::new(Hung), "In Progress").await;
    assert_eq!(result, Err(FetchError::Timeout(120_000)));
    assert_eq!(began.elapsed(), Duration::from_secs(120));
}

#[tokio::test(start_paused = true)]
async fn a_rate_limit_is_not_retried_inside_its_retry_after() {
    let canned = Canned::new(429, &[("retry-after", "60")]);
    let (wf, tracker) = linear(canned.clone());
    assert!(read_through(&wf, tracker, "In Progress").await.is_err());
    assert_eq!(canned.arrivals().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_server_error_on_a_post_read_is_retried_here_because_the_transport_does_not() {
    let canned = Canned::new(503, &[]);
    let (wf, tracker) = linear(canned.clone());
    assert!(read_through(&wf, tracker, "In Progress").await.is_err());
    assert_eq!(
        canned.arrivals(),
        [0, 1, 3, 7].map(Duration::from_secs).to_vec()
    );
}

#[tokio::test(start_paused = true)]
async fn the_retries_of_the_transport_are_not_repeated() {
    let canned = Canned::new(503, &[]);
    let (wf, tracker) = github(canned.clone());
    assert!(read_through(&wf, tracker, "open").await.is_err());
    assert_eq!(
        canned.arrivals(),
        [0, 1, 3, 7].map(Duration::from_secs).to_vec()
    );
}
