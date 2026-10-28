//! symphony-tasks-client: a thin, dependency-light client for Symphony's task API.
//!
//! Products queue work for an AI agent without a tracker, then cancel, unblock or watch it:
//!
//! ```no_run
//! # async fn demo() -> Result<(), symphony_tasks_client::ClientError> {
//! use symphony_tasks_client::{NewTask, TasksClient};
//!
//! let client = TasksClient::new("http://127.0.0.1:4000")?.with_token("<SYMPHONY_API_TOKEN>");
//! let task = client
//!     .create(&NewTask {
//!         prompt: "Read README.md and write a five-line summary.".into(),
//!         repo: Some("https://github.com/f8fvwgzc/texc-symphony".into()),
//!         ..NewTask::default()
//!     })
//!     .await?;
//! let done = client.wait(task.id).await?;
//! println!("{:?}: {}", done.status, done.result.unwrap_or_default());
//! # Ok(())
//! # }
//! ```
//!
//! A task can wait for others ([`NewTask::depends_on`]: it runs once they have succeeded and fails
//! if one fails), and [`TasksClient::plan`] shows the order and the waiting set of a set of tasks
//! without creating any.
//!
//! Install it as a git dependency pinned to a release tag:
//!
//! ```toml
//! symphony-tasks-client = { git = "https://github.com/f8fvwgzc/texc-symphony", tag = "v0.2.0" }
//! ```
//!
//! The wire contract is `docs/api/openapi.yaml` (`/api/openapi.json` on a running server).

#![warn(missing_docs)]

mod sse;

use std::collections::VecDeque;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use futures::{Stream, StreamExt};
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

/// Lifecycle status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Waiting for a free agent slot.
    Queued,
    /// Queued, but a task in `depends_on` has not succeeded yet.
    Waiting,
    /// An agent is working on it.
    Running,
    /// Done; `result` holds the agent's final message.
    Succeeded,
    /// The run failed (`error`); [`TasksClient::unblock`] queues it again.
    Failed,
    /// Cancelled.
    Cancelled,
    /// The agent needs a human; [`TasksClient::unblock`] queues it again with a reply.
    Blocked,
}

impl TaskStatus {
    /// `true` for `succeeded`, `failed` and `cancelled`.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Limits of a task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLimits {
    /// Tokens (input + output) the task may use.
    pub max_tokens: Option<u64>,
    /// Wall-clock limit of one run in milliseconds.
    pub timeout_ms: Option<u64>,
}

/// A kind of failed run a retry policy can retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskFailure {
    /// The agent's run failed.
    Error,
    /// The run reached `timeout_ms` or stalled.
    Timeout,
    /// The replica running it stopped; another one takes the task over.
    Interrupted,
}

/// A task's retry policy. A failed run of a kind in `on` runs again while fewer than
/// `max_attempts` attempts have failed, after `backoff_ms * 2^(failed - 1)` (at most
/// `max_backoff_ms`; at once after an interruption). Hand-offs (kill switch, daily budget,
/// shutdown) do not count, and unblocking resets the count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRetry {
    /// Attempts that may fail before the task does (1-20; default 3).
    pub max_attempts: u32,
    /// Delay before the first retry in ms (default 10 000), doubled per failed attempt.
    pub backoff_ms: u64,
    /// Longest delay in ms (default 300 000).
    pub max_backoff_ms: u64,
    /// Retried failures (default: `interrupted` only).
    pub on: Vec<TaskFailure>,
}

impl Default for TaskRetry {
    /// The server's default policy.
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff_ms: 10_000,
            max_backoff_ms: 300_000,
            on: vec![TaskFailure::Interrupted],
        }
    }
}

/// A task to queue (`POST /api/v1/tasks`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewTask {
    /// Short label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// What the agent should do.
    pub prompt: String,
    /// Repository to shallow-clone into the workspace; `None` = empty workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Branch or tag to clone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// A `backends` entry of the server's WORKFLOW.md; `None` = its default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limits: Option<TaskLimits>,
    /// Free-form JSON object kept with the task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Existing tasks that must succeed first (at most 100). The task is `waiting` until then, and
    /// fails if one of them fails or is cancelled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<i64>,
    /// Retry policy; `None` = the server's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<TaskRetry>,
}

/// A task as the server reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    /// Task id.
    pub id: i64,
    /// `task-<id>`.
    pub identifier: String,
    /// Short label.
    pub title: Option<String>,
    /// Prompt (operator replies appended).
    pub prompt: String,
    /// Repository.
    pub repo: Option<String>,
    /// Branch or tag.
    pub branch: Option<String>,
    /// Backend name.
    pub backend: Option<String>,
    /// Limits.
    pub limits: TaskLimits,
    /// Submitter metadata.
    pub metadata: Option<Value>,
    /// Tasks that must succeed before this one runs (ascending ids).
    #[serde(default)]
    pub depends_on: Vec<i64>,
    /// Status.
    pub status: TaskStatus,
    /// Final agent message of a succeeded task.
    pub result: Option<String>,
    /// Why it failed, was cancelled or is blocked.
    pub error: Option<String>,
    /// Runs started so far.
    pub attempts: u32,
    /// Retry policy.
    #[serde(default)]
    pub retry: TaskRetry,
    /// Failed attempts counted against `retry.max_attempts` (since creation or the last unblock).
    #[serde(default)]
    pub failed_attempts: u32,
    /// When a retried task may run again (`None`: as soon as a slot is free).
    #[serde(default)]
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
    /// Start of the latest run.
    pub started_at: Option<DateTime<Utc>>,
    /// When it became final.
    pub finished_at: Option<DateTime<Utc>>,
}

/// One page of tasks, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskPage {
    /// Tasks.
    pub tasks: Vec<Task>,
    /// Cursor for the next page.
    pub next_before_id: Option<i64>,
}

/// Filters of [`TasksClient::list`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListTasks {
    /// Page size (1..=200).
    pub limit: Option<u32>,
    /// Only tasks with a smaller id.
    pub before_id: Option<i64>,
    /// Only tasks with this status.
    pub status: Option<TaskStatus>,
}

/// A dependency in a plan: another planned task's key, or an existing task's id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PlanDependency {
    /// A planned task, by its key.
    Planned(String),
    /// An existing task, by its id.
    Existing(i64),
}

/// One task of a plan request ([`TasksClient::plan`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTask {
    /// The client's name for it (1-128 characters, unique in the plan).
    pub key: String,
    /// What it waits for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<PlanDependency>,
}

/// What creating a planned task now would give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlannedStatus {
    /// Dispatched as soon as a slot is free.
    Queued,
    /// Waits for `waiting_for`.
    Waiting,
    /// `failed_dependency` already failed or was cancelled; creating it would be refused.
    Failed,
}

/// One task of a [`Plan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTask {
    /// Its key.
    pub key: String,
    /// Its wave (0: depends on no planned task).
    pub wave: usize,
    /// Its status if created now.
    pub status: PlannedStatus,
    /// Planned dependencies and existing ones that have not succeeded.
    pub waiting_for: Vec<PlanDependency>,
    /// The dependency that makes it fail.
    pub failed_dependency: Option<PlanDependency>,
}

/// A plan (`POST /api/v1/tasks/plan`): nothing was created or run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// Keys in dependency order (wave by wave, request order within a wave).
    pub order: Vec<String>,
    /// Keys by wave; each wave depends only on earlier ones.
    pub waves: Vec<Vec<String>>,
    /// The planned tasks, in request order.
    pub tasks: Vec<PlannedTask>,
}

/// One event recorded by a task's run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    /// Run id.
    pub run_id: i64,
    /// Sequence within the run.
    pub seq: i64,
    /// When it happened.
    pub at: DateTime<Utc>,
    /// Event kind (`session_started`, `notification`, `turn_completed`, ...).
    pub kind: String,
    /// Short summary.
    pub message: Option<String>,
    /// Redacted payload (dropped after the server's payload retention).
    pub payload: Option<Value>,
}

/// What [`TasksClient::watch`] yields.
#[derive(Debug, Clone, PartialEq)]
pub enum TaskEvent {
    /// The task record changed.
    Task(Box<Task>),
    /// A run of the task recorded an event.
    RunEvent(Box<RunEvent>),
}

/// Client failures.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The base URL is not an `http(s)://` URL.
    #[error("invalid_url: {0}")]
    InvalidUrl(String),
    /// The request did not complete.
    #[error("request_failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The server answered with an error envelope.
    #[error("api_error: {status} {code}: {message}")]
    Api {
        /// HTTP status.
        status: u16,
        /// Stable error code (`task_not_found`, `invalid_parameter`, ...).
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// A body did not decode.
    #[error("decode_failed: {0}")]
    Decode(String),
    /// The task stream ended before the task was final.
    #[error("stream_ended")]
    StreamEnded,
}

#[derive(Deserialize)]
struct Envelope {
    error: EnvelopeBody,
}

#[derive(Deserialize)]
struct EnvelopeBody {
    code: String,
    message: String,
}

#[derive(Deserialize)]
struct Ticket {
    ticket: String,
}

/// A stream of task events.
pub type TaskEvents = Pin<Box<dyn Stream<Item = Result<TaskEvent, ClientError>> + Send>>;

/// Client of one Symphony server.
#[derive(Clone)]
pub struct TasksClient {
    base: Url,
    token: Option<String>,
    http: reqwest::Client,
}

/// Never prints the token.
impl std::fmt::Debug for TasksClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TasksClient")
            .field("base", &self.base.as_str())
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .finish_non_exhaustive()
    }
}

impl TasksClient {
    /// A client for `base_url` (for example `http://127.0.0.1:4000`).
    pub fn new(base_url: &str) -> Result<Self, ClientError> {
        let mut base =
            Url::parse(base_url).map_err(|err| ClientError::InvalidUrl(err.to_string()))?;
        if !matches!(base.scheme(), "http" | "https") || base.host().is_none() {
            return Err(ClientError::InvalidUrl(base_url.to_owned()));
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        Ok(Self {
            base,
            token: None,
            http: reqwest::Client::new(),
        })
    }

    /// Sends `Authorization: Bearer <token>` (the server's `SYMPHONY_API_TOKEN`).
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Uses `http` (timeouts, proxies, TLS roots) instead of a default client.
    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    fn url(&self, path: &str) -> Result<Url, ClientError> {
        self.base
            .join(path.trim_start_matches('/'))
            .map_err(|err| ClientError::InvalidUrl(err.to_string()))
    }

    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        let builder = self.http.request(method, url);
        match &self.token {
            Some(token) => builder.bearer_auth(token),
            None => builder,
        }
    }

    async fn decode<T: for<'de> Deserialize<'de>>(response: Response) -> Result<T, ClientError> {
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(match serde_json::from_slice::<Envelope>(&bytes) {
                Ok(envelope) => ClientError::Api {
                    status: status.as_u16(),
                    code: envelope.error.code,
                    message: envelope.error.message,
                },
                Err(_) => ClientError::Api {
                    status: status.as_u16(),
                    code: "http_error".into(),
                    message: String::from_utf8_lossy(&bytes).chars().take(500).collect(),
                },
            });
        }
        serde_json::from_slice(&bytes).map_err(|err| ClientError::Decode(err.to_string()))
    }

    /// Queues a task.
    pub async fn create(&self, task: &NewTask) -> Result<Task, ClientError> {
        let response = self
            .request(Method::POST, self.url("api/v1/tasks")?)
            .json(task)
            .send()
            .await?;
        Self::decode(response).await
    }

    /// One task.
    pub async fn get(&self, id: i64) -> Result<Task, ClientError> {
        let response = self
            .request(Method::GET, self.url(&format!("api/v1/tasks/{id}"))?)
            .send()
            .await?;
        Self::decode(response).await
    }

    /// Tasks, newest first.
    pub async fn list(&self, query: &ListTasks) -> Result<TaskPage, ClientError> {
        let mut url = self.url("api/v1/tasks")?;
        {
            let mut pairs = url.query_pairs_mut();
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
            if let Some(before) = query.before_id {
                pairs.append_pair("before_id", &before.to_string());
            }
            if let Some(status) = query.status {
                let value = serde_json::to_value(status).unwrap_or_default();
                pairs.append_pair("status", value.as_str().unwrap_or_default());
            }
        }
        let response = self.request(Method::GET, url).send().await?;
        Self::decode(response).await
    }

    /// Cancels a task (final tasks are returned unchanged).
    pub async fn cancel(&self, id: i64) -> Result<Task, ClientError> {
        let response = self
            .request(
                Method::POST,
                self.url(&format!("api/v1/tasks/{id}/cancel"))?,
            )
            .send()
            .await?;
        Self::decode(response).await
    }

    /// Plans `tasks` without creating or running anything: their order, waves and waiting set.
    /// An unknown key or id, a duplicate key or a cycle is an `invalid_parameter` error.
    pub async fn plan(&self, tasks: &[PlanTask]) -> Result<Plan, ClientError> {
        let response = self
            .request(Method::POST, self.url("api/v1/tasks/plan")?)
            .json(&serde_json::json!({ "tasks": tasks }))
            .send()
            .await?;
        Self::decode(response).await
    }

    /// Re-queues a blocked or failed task, with an optional operator reply.
    pub async fn unblock(&self, id: i64, message: Option<&str>) -> Result<Task, ClientError> {
        let response = self
            .request(
                Method::POST,
                self.url(&format!("api/v1/tasks/{id}/unblock"))?,
            )
            .json(&serde_json::json!({"message": message}))
            .send()
            .await?;
        Self::decode(response).await
    }

    /// Streams the task's record changes and run events until it is final (one SSE
    /// connection, authenticated with a single-use ticket).
    pub async fn watch(&self, id: i64) -> Result<TaskEvents, ClientError> {
        let ticket: Ticket = Self::decode(
            self.request(Method::POST, self.url("api/v1/events/tickets")?)
                .send()
                .await?,
        )
        .await?;
        let mut url = self.url(&format!("api/v1/tasks/{id}/events"))?;
        url.query_pairs_mut().append_pair("ticket", &ticket.ticket);
        let response = self
            .http
            .get(url)
            .header("accept", "text/event-stream")
            .send()
            .await?;
        if response.status() != StatusCode::OK {
            return Err(Self::decode::<Value>(response)
                .await
                .err()
                .unwrap_or(ClientError::StreamEnded));
        }
        let bytes = response.bytes_stream();
        let state = (bytes, sse::Parser::default(), VecDeque::<sse::Frame>::new());
        let stream =
            futures::stream::unfold(state, |(mut bytes, mut parser, mut frames)| async move {
                loop {
                    if let Some(frame) = frames.pop_front() {
                        let event = match frame.event.as_deref() {
                            Some("task") => serde_json::from_str(&frame.data)
                                .map(|t| TaskEvent::Task(Box::new(t))),
                            Some("run_event") => serde_json::from_str(&frame.data)
                                .map(|e| TaskEvent::RunEvent(Box::new(e))),
                            _ => continue,
                        }
                        .map_err(|err| ClientError::Decode(err.to_string()));
                        return Some((event, (bytes, parser, frames)));
                    }
                    match bytes.next().await? {
                        Ok(chunk) => frames.extend(parser.push(&chunk)),
                        Err(err) => {
                            return Some((Err(ClientError::Http(err)), (bytes, parser, frames)));
                        }
                    }
                }
            });
        Ok(Box::pin(stream))
    }

    /// Waits until the task is final and returns it (watching its stream).
    pub async fn wait(&self, id: i64) -> Result<Task, ClientError> {
        let mut events = self.watch(id).await?;
        let mut last = None;
        while let Some(event) = events.next().await {
            if let TaskEvent::Task(task) = event? {
                if task.status.is_final() {
                    return Ok(*task);
                }
                last = Some(task);
            }
        }
        match last {
            Some(task) if task.status.is_final() => Ok(*task),
            _ => Err(ClientError::StreamEnded),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_token() {
        let client = TasksClient::new("http://127.0.0.1:4000")
            .unwrap()
            .with_token("secret-token-0123456789");
        let shown = format!("{client:?}");
        assert!(!shown.contains("secret-token"), "{shown}");
        assert!(shown.contains("[REDACTED]"), "{shown}");
    }
}
