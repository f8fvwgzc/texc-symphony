//! The task API: work assigned by products directly, without a tracker issue.
//!
//! * `POST /api/v1/tasks` queues a task (prompt, optional repository, backend, limits,
//!   metadata, the existing tasks it depends on) and asks the orchestrator to poll now. A task
//!   waits (`waiting`) until its dependencies have succeeded, and fails once one of them fails.
//! * `POST /api/v1/tasks/plan` plans a set of tasks without creating or running anything
//!   ([`crate::plan`]): their order, waves and what each would wait for.
//! * `GET /api/v1/tasks` / `GET /api/v1/tasks/{id}` read them from the store.
//! * `POST /api/v1/tasks/{id}/cancel` stops a running task (through the orchestrator) or
//!   cancels a queued / blocked one.
//! * `POST /api/v1/tasks/{id}/unblock` re-queues a blocked (or failed) task, optionally with an
//!   operator reply appended to its prompt.
//! * `GET /api/v1/tasks/{id}/events` streams the task over SSE: `task` events whenever the record
//!   changes and `run_event` events with its runs' recorded events, until the task is final.
//!
//! Tasks need the run-history store (`503 store_disabled` otherwise). Their runs are ordinary runs
//! recorded under the identifier `task-<id>`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use serde::Deserialize;
use serde_json::Value;
use symphony_core::task::{MAX_TASK_PROMPT_BYTES, valid_branch, valid_repo_url};
use symphony_store::{
    MAX_TASK_DEPENDENCIES, NewTask, RunQuery, RunStatus, TaskLimits, TaskPage, TaskQuery,
    TaskRecord, TaskStatus,
};
use tokio::sync::OwnedSemaphorePermit;

use crate::app::AppState;
use crate::config::SSE_RETRY;
use crate::error::ApiError;
use crate::history::{enabled_store, invalid, limit_param, parse_query};

// The API's prompt limit is the store's (operator replies may not grow a prompt past it either).
const _: () = assert!(MAX_TASK_PROMPT_BYTES == symphony_store::MAX_TASK_PROMPT_BYTES);
// The messages above name the limit.
const _: () = assert!(MAX_TASK_DEPENDENCIES == 100);

/// Default and maximum page size of `GET /api/v1/tasks`.
const TASKS_LIMIT: (u32, u32) = (50, 200);
/// How often a task stream looks for changes.
pub const TASK_STREAM_POLL: Duration = Duration::from_millis(500);
/// Longest title, in characters.
const MAX_TITLE_CHARS: usize = 200;
/// Largest metadata object, in bytes of JSON.
const MAX_METADATA_BYTES: usize = 16 * 1024;

/// Body of `POST /api/v1/tasks`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateTask {
    title: Option<String>,
    prompt: String,
    repo: Option<String>,
    branch: Option<String>,
    backend: Option<String>,
    #[serde(default)]
    limits: Option<CreateLimits>,
    metadata: Option<Value>,
    #[serde(default)]
    depends_on: Vec<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateLimits {
    max_tokens: Option<u64>,
    timeout_ms: Option<u64>,
}

/// Body of `POST /api/v1/tasks/{id}/unblock` (optional).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Unblock {
    message: Option<String>,
}

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(value)| value).map_err(|rejection| {
        ApiError::InvalidParameter(format!("invalid body: {}", rejection.body_text()))
    })
}

fn validate(app: &AppState, request: CreateTask) -> Result<NewTask, ApiError> {
    let prompt = request.prompt;
    if prompt.trim().is_empty() {
        return Err(invalid("prompt must not be empty"));
    }
    if prompt.len() > MAX_TASK_PROMPT_BYTES {
        return Err(invalid("prompt must be at most 262144 bytes"));
    }
    let title = request
        .title
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty());
    if title
        .as_ref()
        .is_some_and(|t| t.chars().count() > MAX_TITLE_CHARS)
    {
        return Err(invalid("title must be at most 200 characters"));
    }
    let repo = request
        .repo
        .map(|r| r.trim().to_owned())
        .filter(|r| !r.is_empty());
    if repo.as_deref().is_some_and(|r| !valid_repo_url(r)) {
        return Err(invalid(
            "repo must be an https:// or ssh:// URL or user@host:path",
        ));
    }
    let branch = request.branch.filter(|b| !b.is_empty());
    if let Some(branch) = &branch {
        if repo.is_none() {
            return Err(invalid("branch needs repo"));
        }
        if !valid_branch(branch) {
            return Err(invalid("branch is invalid"));
        }
    }
    let backend = request.backend.filter(|b| !b.is_empty());
    if let Some(backend) = &backend
        && let Some(known) = app.control.backend_names()
        && !known.contains(backend)
    {
        return Err(ApiError::InvalidParameter(format!(
            "backend must be one of {}",
            known.join(", ")
        )));
    }
    let limits = request.limits.unwrap_or_default();
    if limits.max_tokens == Some(0) || limits.timeout_ms == Some(0) {
        return Err(invalid("limits must be greater than 0"));
    }
    if let Some(metadata) = &request.metadata {
        if !metadata.is_object() {
            return Err(invalid("metadata must be a JSON object"));
        }
        if metadata.to_string().len() > MAX_METADATA_BYTES {
            return Err(invalid("metadata must be at most 16384 bytes of JSON"));
        }
    }
    let mut depends_on = request.depends_on;
    depends_on.sort_unstable();
    depends_on.dedup();
    if depends_on.iter().any(|id| *id < 1) {
        return Err(invalid("depends_on must list positive task ids"));
    }
    if depends_on.len() > MAX_TASK_DEPENDENCIES {
        return Err(invalid("depends_on must list at most 100 tasks"));
    }
    Ok(NewTask {
        title,
        prompt,
        repo,
        branch,
        backend,
        limits: TaskLimits {
            max_tokens: limits.max_tokens,
            timeout_ms: limits.timeout_ms,
        },
        metadata: request.metadata,
        depends_on,
    })
}

fn task_id(path: Result<Path<String>, PathRejection>) -> Result<i64, ApiError> {
    path.ok()
        .and_then(|Path(raw)| raw.parse::<i64>().ok())
        .filter(|id| *id >= 1)
        .ok_or_else(|| invalid("id must be a positive integer"))
}

/// `POST /api/v1/tasks`: 201 with the queued task.
pub(crate) async fn create(
    State(app): State<AppState>,
    body: Result<Json<CreateTask>, JsonRejection>,
) -> Result<(StatusCode, Json<TaskRecord>), ApiError> {
    let store = enabled_store(&app)?.clone();
    let task = validate(&app, json_body(body)?)?;
    let task = store.create_task(task).await?;
    tracing::info!(task_id = task.id, "Task queued");
    // Best effort: without an orchestrator the task waits for the next poll.
    let _ = tokio::time::timeout(app.config.refresh_timeout, app.control.refresh()).await;
    Ok((StatusCode::CREATED, Json(task)))
}

/// `GET /api/v1/tasks?limit&before_id&status`.
pub(crate) async fn list(
    State(app): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<TaskPage>, ApiError> {
    let params = parse_query(query.as_deref());
    let limit = limit_param(&params, TASKS_LIMIT)?;
    let before_id = match params.get("before_id") {
        None => None,
        Some(raw) => Some(
            raw.parse::<i64>()
                .ok()
                .filter(|id| *id >= 1)
                .ok_or_else(|| invalid("before_id must be a positive integer"))?,
        ),
    };
    let status = match params.get("status") {
        None => None,
        Some(raw) => Some(raw.parse::<TaskStatus>().map_err(|_| {
            invalid(
                "status must be one of queued, waiting, running, succeeded, failed, cancelled, blocked",
            )
        })?),
    };
    let store = enabled_store(&app)?;
    Ok(Json(
        store
            .list_tasks(TaskQuery {
                limit: Some(limit),
                before_id,
                status,
            })
            .await?,
    ))
}

/// `GET /api/v1/tasks/{id}`.
pub(crate) async fn get(
    State(app): State<AppState>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<TaskRecord>, ApiError> {
    let id = task_id(id)?;
    let store = enabled_store(&app)?;
    store
        .get_task(id)
        .await?
        .map(Json)
        .ok_or(ApiError::TaskNotFound)
}

async fn cancel_running(app: &AppState, id: i64) -> Result<(), ApiError> {
    match tokio::time::timeout(app.config.refresh_timeout, app.control.cancel_task(id)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(ApiError::OrchestratorUnavailable),
    }
}

/// `POST /api/v1/tasks/{id}/cancel`: the task after cancellation (final tasks unchanged).
pub(crate) async fn cancel(
    State(app): State<AppState>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<TaskRecord>, ApiError> {
    let id = task_id(id)?;
    let store = enabled_store(&app)?.clone();
    let task = store.get_task(id).await?.ok_or(ApiError::TaskNotFound)?;
    if task.status.is_final() {
        return Ok(Json(task));
    }
    if task.status == TaskStatus::Running {
        cancel_running(&app, id).await?;
    } else {
        // Releases a blocked task's claim; harmless for a queued one.
        let _ = tokio::time::timeout(app.config.refresh_timeout, app.control.cancel_task(id)).await;
    }
    let mut task = store
        .cancel_task(id, "cancelled by client")
        .await?
        .ok_or(ApiError::TaskNotFound)?;
    if task.status == TaskStatus::Running {
        // Dispatched between the read and the cancel: stop that run too.
        cancel_running(&app, id).await?;
        task = store.get_task(id).await?.ok_or(ApiError::TaskNotFound)?;
    }
    Ok(Json(task))
}

/// `POST /api/v1/tasks/{id}/unblock` with an optional `{"message": "..."}`: re-queues a blocked
/// or failed task (409 `task_conflict` otherwise).
pub(crate) async fn unblock(
    State(app): State<AppState>,
    id: Result<Path<String>, PathRejection>,
    body: Option<Json<Unblock>>,
) -> Result<Json<TaskRecord>, ApiError> {
    let id = task_id(id)?;
    let message = body.map(|Json(b)| b).unwrap_or_default().message;
    if message
        .as_ref()
        .is_some_and(|m| m.len() > MAX_TASK_PROMPT_BYTES)
    {
        return Err(invalid("message must be at most 262144 bytes"));
    }
    let store = enabled_store(&app)?.clone();
    let task = match store.requeue_task(id, message).await {
        Ok(Some(task)) => task,
        Ok(None) => return Err(ApiError::TaskNotFound),
        Err(symphony_store::StoreError::InvalidArgument(message)) => {
            return Err(ApiError::Conflict(message));
        }
        Err(err) => return Err(err.into()),
    };
    let _ = tokio::time::timeout(app.config.refresh_timeout, app.control.unblock_task(id)).await;
    Ok(Json(task))
}

/// One open task stream.
struct Watch {
    app: AppState,
    id: i64,
    last: Option<TaskRecord>,
    /// Last event seq sent, per run id.
    seqs: Vec<(i64, i64)>,
    pending: std::collections::VecDeque<Event>,
    started: bool,
    done: bool,
    _slot: Option<OwnedSemaphorePermit>,
}

impl Watch {
    async fn poll(&mut self) -> Result<(), symphony_store::StoreError> {
        let store = &self.app.store;
        let task = store.get_task(self.id).await?;
        let runs = store
            .list_runs(RunQuery {
                limit: Some(20),
                // By issue id: a tracker issue may share the identifier.
                issue_id: Some(format!("task:{}", self.id)),
                ..RunQuery::default()
            })
            .await?;
        let mut runs = runs.runs;
        runs.sort_by_key(|run| run.id);
        let mut runs_done = true;
        for run in &runs {
            let after = self
                .seqs
                .iter()
                .find(|(id, _)| *id == run.id.get())
                .map_or(0, |(_, seq)| *seq);
            let events = store.list_events(run.id, Some(after), Some(500)).await?;
            if run.status == RunStatus::Running || events.len() == 500 {
                runs_done = false;
            }
            if let Some(last) = events.last() {
                match self.seqs.iter_mut().find(|(id, _)| *id == run.id.get()) {
                    Some(entry) => entry.1 = last.seq,
                    None => self.seqs.push((run.id.get(), last.seq)),
                }
            }
            for event in events {
                let data = serde_json::to_string(&event).unwrap_or_default();
                self.pending.push_back(
                    Event::default()
                        .event("run_event")
                        .id(format!("{}-{}", event.run_id, event.seq))
                        .data(data),
                );
            }
        }
        if let Some(task) = task {
            if self.last.as_ref() != Some(&task) {
                let data = serde_json::to_string(&task).unwrap_or_default();
                self.pending
                    .push_back(Event::default().event("task").data(data));
            }
            self.done = task.status.is_final() && runs_done;
            self.last = Some(task);
        } else {
            self.done = true;
        }
        Ok(())
    }

    async fn next(&mut self) -> Option<Event> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }
            if self.started {
                tokio::select! {
                    () = self.app.shutdown.cancelled() => return None,
                    () = tokio::time::sleep(TASK_STREAM_POLL) => {}
                }
            }
            self.started = true;
            if let Err(err) = self.poll().await {
                tracing::warn!(task_id = self.id, "task stream stopped: {err}");
                return None;
            }
        }
    }
}

fn stream(watch: Watch) -> impl Stream<Item = Result<Event, Infallible>> {
    futures::stream::unfold((watch, true), |(mut watch, first)| async move {
        if first {
            return Some((Ok(Event::default().retry(SSE_RETRY)), (watch, false)));
        }
        let event = watch.next().await?;
        Some((Ok(event), (watch, false)))
    })
}

/// `GET /api/v1/tasks/{id}/events`: Server-Sent Events of one task (see the module docs).
pub(crate) async fn events(
    State(app): State<AppState>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let id = task_id(id)?;
    let store = enabled_store(&app)?;
    if store.get_task(id).await?.is_none() {
        return Err(ApiError::TaskNotFound);
    }
    let slot = match &app.sse_slots {
        None => None,
        Some(slots) => Some(Arc::clone(slots).try_acquire_owned().map_err(|_| {
            ApiError::RequestFailed(
                StatusCode::SERVICE_UNAVAILABLE,
                "Too many live event streams".to_owned(),
            )
        })?),
    };
    let watch = Watch {
        app: app.clone(),
        id,
        last: None,
        seqs: Vec::new(),
        pending: Default::default(),
        started: false,
        done: false,
        _slot: slot,
    };
    let heartbeat = app.config.sse_heartbeat;
    let mut response = Sse::new(stream(watch))
        .keep_alive(KeepAlive::new().interval(heartbeat))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}
