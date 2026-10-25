//! Tasks: work submitted through the task API instead of a tracker (records and SQL).
//!
//! Lifecycle: `queued` → `running` (the orchestrator dispatched it) → `succeeded` / `failed` /
//! `cancelled` / `blocked`. `blocked` (the agent needs a human) and `failed` tasks can be
//! re-queued with an operator message; queued and blocked tasks can be cancelled directly, running
//! ones through the orchestrator. Tasks still `running` at startup were interrupted and go back to
//! `queued`.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};

/// Default page size of [`TaskQuery`].
pub const DEFAULT_TASK_LIMIT: u32 = 50;
/// Maximum page size of [`TaskQuery`].
pub const MAX_TASK_LIMIT: u32 = 200;
/// Longest task prompt, in bytes (also the API's limit for new tasks).
pub const MAX_TASK_PROMPT_BYTES: usize = 256 * 1024;
/// Longest stored result, in bytes (longer results are cut on a char boundary).
pub const MAX_TASK_RESULT_BYTES: usize = 256 * 1024;

/// Lifecycle status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Waiting for a free agent slot.
    Queued,
    /// An agent is working on it.
    Running,
    /// The agent finished; `result` holds its final message.
    Succeeded,
    /// The run failed (see `error`); can be re-queued.
    Failed,
    /// Cancelled by a client or the kill switch path.
    Cancelled,
    /// The agent needs operator input or approval; unblock re-queues it.
    Blocked,
}

impl TaskStatus {
    /// Every status.
    pub const ALL: [TaskStatus; 6] = [
        TaskStatus::Queued,
        TaskStatus::Running,
        TaskStatus::Succeeded,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
        TaskStatus::Blocked,
    ];

    /// Wire/database spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Blocked => "blocked",
        }
    }

    /// `true` once nothing more happens without a client action.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            TaskStatus::Succeeded | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskStatus {
    type Err = StoreError;

    fn from_str(s: &str) -> Result<Self> {
        TaskStatus::ALL
            .into_iter()
            .find(|status| status.as_str() == s)
            .ok_or_else(|| StoreError::InvalidArgument(format!("unknown task status {s:?}")))
    }
}

/// Limits of one task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLimits {
    /// Tokens (input + output) the task may use across its runs.
    pub max_tokens: Option<u64>,
    /// Wall-clock limit of one run in milliseconds.
    pub timeout_ms: Option<u64>,
}

/// Arguments of `Store::create_task` (validated by the caller).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewTask {
    /// Short label for dashboards.
    pub title: Option<String>,
    /// What the agent should do.
    pub prompt: String,
    /// Repository to clone into the workspace; `None` = empty workspace.
    pub repo: Option<String>,
    /// Branch or tag to clone.
    pub branch: Option<String>,
    /// `backends` entry; `None` = `agent.backend`.
    pub backend: Option<String>,
    /// Limits.
    pub limits: TaskLimits,
    /// Free-form JSON object kept with the task for the submitter.
    pub metadata: Option<serde_json::Value>,
}

/// One task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Task id.
    pub id: i64,
    /// Identifier of its runs in run history and the live state (`task-<id>`).
    pub identifier: String,
    /// Short label.
    pub title: Option<String>,
    /// The prompt (operator replies from unblock are appended).
    pub prompt: String,
    /// Repository cloned into the workspace.
    pub repo: Option<String>,
    /// Branch or tag.
    pub branch: Option<String>,
    /// Backend name (`None` = default).
    pub backend: Option<String>,
    /// Limits.
    pub limits: TaskLimits,
    /// Submitter metadata.
    pub metadata: Option<serde_json::Value>,
    /// Current status.
    pub status: TaskStatus,
    /// Final agent message of a succeeded task.
    pub result: Option<String>,
    /// Why it failed, was cancelled or is blocked.
    pub error: Option<String>,
    /// Runs started so far.
    pub attempts: u32,
    /// Creation time (millisecond precision).
    pub created_at: DateTime<Utc>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
    /// When the latest run started.
    pub started_at: Option<DateTime<Utc>>,
    /// When it reached `succeeded`, `failed` or `cancelled`.
    pub finished_at: Option<DateTime<Utc>>,
}

/// `task-<id>`: lowercase, so it never equals an (uppercase) Linear or Jira key; runs of a task
/// are matched by their issue id `task:<id>`, never by this identifier.
pub fn task_identifier(id: i64) -> String {
    format!("task-{id}")
}

/// Filters and cursor of `Store::list_tasks`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskQuery {
    /// Page size (default [`DEFAULT_TASK_LIMIT`], at most [`MAX_TASK_LIMIT`]).
    pub limit: Option<u32>,
    /// Only tasks with `id < before_id`.
    pub before_id: Option<i64>,
    /// Only tasks with this status.
    pub status: Option<TaskStatus>,
}

/// One page of tasks, newest first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskPage {
    /// Tasks by descending id.
    pub tasks: Vec<TaskRecord>,
    /// Cursor for the next (older) page.
    pub next_before_id: Option<i64>,
}

const COLUMNS: &str = "id, title, prompt, repo, branch, backend, max_tokens, timeout_ms, metadata, \
     status, result, error, attempts, created_at, updated_at, started_at, finished_at";

fn ts(column: usize, ms: i64) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Integer,
            Box::new(StoreError::CorruptRow(format!(
                "timestamp out of range: {ms}"
            ))),
        )
    })
}

fn opt_u64(value: Option<i64>) -> Option<u64> {
    value.and_then(|v| u64::try_from(v).ok())
}

fn to_sql(value: Option<u64>) -> Option<i64> {
    value.map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

fn task_from_row(row: &Row<'_>) -> rusqlite::Result<TaskRecord> {
    let status: String = row.get(9)?;
    let status = status
        .parse::<TaskStatus>()
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(9, Type::Text, Box::new(err)))?;
    let metadata = row
        .get::<_, Option<String>>(8)?
        .map(|text| {
            serde_json::from_str(&text).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(8, Type::Text, Box::new(err))
            })
        })
        .transpose()?;
    let id: i64 = row.get(0)?;
    Ok(TaskRecord {
        id,
        identifier: task_identifier(id),
        title: row.get(1)?,
        prompt: row.get(2)?,
        repo: row.get(3)?,
        branch: row.get(4)?,
        backend: row.get(5)?,
        limits: TaskLimits {
            max_tokens: opt_u64(row.get(6)?),
            timeout_ms: opt_u64(row.get(7)?),
        },
        metadata,
        status,
        result: row.get(10)?,
        error: row.get(11)?,
        attempts: u32::try_from(row.get::<_, i64>(12)?.max(0)).unwrap_or(u32::MAX),
        created_at: ts(13, row.get(13)?)?,
        updated_at: ts(14, row.get(14)?)?,
        started_at: row
            .get::<_, Option<i64>>(15)?
            .map(|ms| ts(15, ms))
            .transpose()?,
        finished_at: row
            .get::<_, Option<i64>>(16)?
            .map(|ms| ts(16, ms))
            .transpose()?,
    })
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

pub(crate) fn create_task(
    conn: &Connection,
    task: &NewTask,
    now: DateTime<Utc>,
) -> Result<TaskRecord> {
    if task.prompt.trim().is_empty() {
        return Err(StoreError::InvalidArgument(
            "prompt must not be empty".into(),
        ));
    }
    let metadata = task
        .metadata
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|err| StoreError::InvalidArgument(format!("metadata: {err}")))?;
    let now_ms = now.timestamp_millis();
    conn.prepare_cached(
        "INSERT INTO tasks (title, prompt, repo, branch, backend, max_tokens, timeout_ms, \
         metadata, status, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', ?9, ?9)",
    )?
    .execute(params![
        task.title,
        task.prompt,
        task.repo,
        task.branch,
        task.backend,
        to_sql(task.limits.max_tokens),
        to_sql(task.limits.timeout_ms),
        metadata,
        now_ms,
    ])?;
    let id = conn.last_insert_rowid();
    get_task(conn, id)?.ok_or_else(|| StoreError::Internal("inserted task vanished".into()))
}

pub(crate) fn get_task(conn: &Connection, id: i64) -> Result<Option<TaskRecord>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM tasks WHERE id = ?1"))?
        .query_row([id], task_from_row)
        .optional()?)
}

pub(crate) fn list_tasks(conn: &Connection, query: &TaskQuery) -> Result<TaskPage> {
    let limit = match query.limit {
        None | Some(0) => DEFAULT_TASK_LIMIT,
        Some(n) => n.min(MAX_TASK_LIMIT),
    };
    let mut clauses = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(before) = query.before_id {
        clauses.push("id < ?");
        values.push(before.into());
    }
    if let Some(status) = query.status {
        clauses.push("status = ?");
        values.push(status.as_str().to_owned().into());
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    values.push((i64::from(limit) + 1).into());
    let sql = format!("SELECT {COLUMNS} FROM tasks {where_sql} ORDER BY id DESC LIMIT ?");
    let mut tasks = conn
        .prepare_cached(&sql)?
        .query_map(params_from_iter(values), task_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next_before_id = if tasks.len() > limit as usize {
        tasks.truncate(limit as usize);
        tasks.last().map(|t| t.id)
    } else {
        None
    };
    Ok(TaskPage {
        tasks,
        next_before_id,
    })
}

pub(crate) fn queued_tasks(conn: &Connection, limit: u32) -> Result<Vec<TaskRecord>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM tasks WHERE status = 'queued' ORDER BY id ASC LIMIT ?1"
        ))?
        .query_map([i64::from(limit)], task_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `queued` → `running` (attempts + 1). `None` when the task was not queued (any more).
pub(crate) fn start_task(
    conn: &Connection,
    id: i64,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    let changed = conn
        .prepare_cached(
            "UPDATE tasks SET status = 'running', attempts = attempts + 1, started_at = ?2, \
             updated_at = ?2, error = NULL WHERE id = ?1 AND status = 'queued'",
        )?
        .execute(params![id, now.timestamp_millis()])?;
    if changed == 0 {
        return Ok(None);
    }
    get_task(conn, id)
}

/// `running` → `status` (a final status or `blocked`). `None` when the task was not running.
pub(crate) fn finish_task(
    conn: &Connection,
    id: i64,
    status: TaskStatus,
    result: Option<&str>,
    error: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    if matches!(status, TaskStatus::Queued | TaskStatus::Running) {
        return Err(StoreError::InvalidArgument(
            "finish_task needs a final or blocked status".into(),
        ));
    }
    let now_ms = now.timestamp_millis();
    let finished = status.is_final().then_some(now_ms);
    let changed = conn
        .prepare_cached(
            "UPDATE tasks SET status = ?2, result = ?3, error = ?4, updated_at = ?5, \
             finished_at = ?6 WHERE id = ?1 AND status = 'running'",
        )?
        .execute(params![
            id,
            status.as_str(),
            result.map(|r| truncate(r, MAX_TASK_RESULT_BYTES)),
            error.map(|e| truncate(e, 16 * 1024)),
            now_ms,
            finished,
        ])?;
    if changed == 0 {
        return Ok(None);
    }
    get_task(conn, id)
}

/// Cancels a `queued` or `blocked` task; anything else is returned unchanged (a running task is
/// cancelled by the orchestrator). `None` for an unknown id.
pub(crate) fn cancel_task(
    conn: &Connection,
    id: i64,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    let now_ms = now.timestamp_millis();
    conn.prepare_cached(
        "UPDATE tasks SET status = 'cancelled', error = ?2, updated_at = ?3, finished_at = ?3 \
         WHERE id = ?1 AND status IN ('queued', 'blocked')",
    )?
    .execute(params![id, reason, now_ms])?;
    get_task(conn, id)
}

/// Puts a `blocked` or `failed` task back to `queued`, appending `message` (the operator's reply)
/// to its prompt. Other statuses are refused with [`StoreError::InvalidArgument`].
pub(crate) fn requeue_task(
    conn: &Connection,
    id: i64,
    message: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    let Some(task) = get_task(conn, id)? else {
        return Ok(None);
    };
    if !matches!(task.status, TaskStatus::Blocked | TaskStatus::Failed) {
        return Err(StoreError::InvalidArgument(format!(
            "task {id} is {}; only blocked or failed tasks can be unblocked",
            task.status
        )));
    }
    let prompt = match message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(message) => format!("{}\n\nOperator reply:\n{message}", task.prompt),
        None => task.prompt,
    };
    // Every reply grows the prompt: refuse past the limit a new task's prompt has.
    if prompt.len() > MAX_TASK_PROMPT_BYTES {
        return Err(StoreError::InvalidArgument(format!(
            "task {id}: the prompt with this reply would exceed {} bytes",
            MAX_TASK_PROMPT_BYTES
        )));
    }
    conn.prepare_cached(
        "UPDATE tasks SET status = 'queued', prompt = ?2, error = NULL, result = NULL, \
         finished_at = NULL, updated_at = ?3 WHERE id = ?1 AND status IN ('blocked', 'failed')",
    )?
    .execute(params![id, prompt, now.timestamp_millis()])?;
    get_task(conn, id)
}

/// A running task whose run was stopped for an outside reason (kill switch, daily budget) goes
/// back to `queued`, keeping `reason` as its error. Other statuses are left alone.
pub(crate) fn release_task(
    conn: &Connection,
    id: i64,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    conn.prepare_cached(
        "UPDATE tasks SET status = 'queued', error = ?2, updated_at = ?3 \
         WHERE id = ?1 AND status = 'running'",
    )?
    .execute(params![id, reason, now.timestamp_millis()])?;
    get_task(conn, id)
}

/// Startup: tasks left `running` by a previous process go back to `queued`.
pub(crate) fn requeue_interrupted_tasks(conn: &Connection, now: DateTime<Utc>) -> Result<u64> {
    let changed = conn.execute(
        "UPDATE tasks SET status = 'queued', updated_at = ?1, \
         error = 'interrupted by restart; queued again' WHERE status = 'running'",
        [now.timestamp_millis()],
    )?;
    Ok(changed as u64)
}
