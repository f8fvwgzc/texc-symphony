//! Tasks: work submitted through the task API instead of a tracker (records and SQL).
//!
//! Lifecycle: `queued` → `running` (the orchestrator dispatched it) → `succeeded` / `failed` /
//! `cancelled` / `blocked`. `blocked` (the agent needs a human) and `failed` tasks can be
//! re-queued with an operator message; queued and blocked tasks can be cancelled directly, running
//! ones through the orchestrator. A `running` task whose dispatch lease (`task:<id>`, see
//! `leases`) is gone was interrupted (its process stopped) and goes back to `queued`.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::{Db, Row, Val, vals};
use crate::error::{Result, StoreError};
use crate::ops::{clamp_u32, timestamp};

/// Default page size of [`TaskQuery`].
pub const DEFAULT_TASK_LIMIT: u32 = 50;
/// Maximum page size of [`TaskQuery`].
pub const MAX_TASK_LIMIT: u32 = 200;
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
    /// Identifier of its runs in run history and the live state (`TASK-<id>`).
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

/// `TASK-<id>`.
pub fn task_identifier(id: i64) -> String {
    format!("TASK-{id}")
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

fn opt_u64(value: Option<i64>) -> Option<u64> {
    value.and_then(|v| u64::try_from(v).ok())
}

fn to_sql(value: Option<u64>) -> Option<i64> {
    value.map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

fn task_from_row(row: &Row) -> Result<TaskRecord> {
    let status = row.text(9)?.parse::<TaskStatus>()?;
    let metadata = row
        .opt_text(8)?
        .map(|text| {
            serde_json::from_str(&text)
                .map_err(|err| StoreError::CorruptRow(format!("task metadata: {err}")))
        })
        .transpose()?;
    let id = row.int(0)?;
    Ok(TaskRecord {
        id,
        identifier: task_identifier(id),
        title: row.opt_text(1)?,
        prompt: row.text(2)?,
        repo: row.opt_text(3)?,
        branch: row.opt_text(4)?,
        backend: row.opt_text(5)?,
        limits: TaskLimits {
            max_tokens: opt_u64(row.opt_int(6)?),
            timeout_ms: opt_u64(row.opt_int(7)?),
        },
        metadata,
        status,
        result: row.opt_text(10)?,
        error: row.opt_text(11)?,
        attempts: clamp_u32(row.int(12)?),
        created_at: timestamp(row.int(13)?)?,
        updated_at: timestamp(row.int(14)?)?,
        started_at: row.opt_int(15)?.map(timestamp).transpose()?,
        finished_at: row.opt_int(16)?.map(timestamp).transpose()?,
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
    db: &mut dyn Db,
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
    let id = db
        .query_one(
            "INSERT INTO tasks (title, prompt, repo, branch, backend, max_tokens, timeout_ms, \
             metadata, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', ?9, ?9) RETURNING id",
            &vals![
                task.title.as_deref(),
                task.prompt.as_str(),
                task.repo.as_deref(),
                task.branch.as_deref(),
                task.backend.as_deref(),
                to_sql(task.limits.max_tokens),
                to_sql(task.limits.timeout_ms),
                metadata,
                now_ms,
            ],
        )?
        .int(0)?;
    get_task(db, id)?.ok_or_else(|| StoreError::Internal("inserted task vanished".into()))
}

pub(crate) fn get_task(db: &mut dyn Db, id: i64) -> Result<Option<TaskRecord>> {
    db.query_opt(
        &format!("SELECT {COLUMNS} FROM tasks WHERE id = ?1"),
        &vals![id],
    )?
    .map(|row| task_from_row(&row))
    .transpose()
}

pub(crate) fn list_tasks(db: &mut dyn Db, query: &TaskQuery) -> Result<TaskPage> {
    let limit = match query.limit {
        None | Some(0) => DEFAULT_TASK_LIMIT,
        Some(n) => n.min(MAX_TASK_LIMIT),
    };
    let mut clauses = Vec::new();
    let mut values: Vec<Val> = Vec::new();
    if let Some(before) = query.before_id {
        values.push(Val::Int(before));
        clauses.push(format!("id < ?{}", values.len()));
    }
    if let Some(status) = query.status {
        values.push(Val::Text(status.as_str().to_owned()));
        clauses.push(format!("status = ?{}", values.len()));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    values.push(Val::Int(i64::from(limit) + 1));
    let sql = format!(
        "SELECT {COLUMNS} FROM tasks {where_sql} ORDER BY id DESC LIMIT ?{}",
        values.len()
    );
    let mut tasks = db
        .query(&sql, &values)?
        .iter()
        .map(task_from_row)
        .collect::<Result<Vec<_>>>()?;
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

pub(crate) fn queued_tasks(db: &mut dyn Db, limit: u32) -> Result<Vec<TaskRecord>> {
    db.query(
        &format!("SELECT {COLUMNS} FROM tasks WHERE status = 'queued' ORDER BY id ASC LIMIT ?1"),
        &vals![i64::from(limit)],
    )?
    .iter()
    .map(task_from_row)
    .collect()
}

/// `queued` → `running` (attempts + 1). `None` when the task was not queued (any more): the
/// conditional update makes one claimant win, also across replicas.
pub(crate) fn start_task(
    db: &mut dyn Db,
    id: i64,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    let changed = db.execute(
        "UPDATE tasks SET status = 'running', attempts = attempts + 1, started_at = ?2, \
         updated_at = ?2, error = NULL WHERE id = ?1 AND status = 'queued'",
        &vals![id, now.timestamp_millis()],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get_task(db, id)
}

/// `running` → `status` (a final status or `blocked`). `None` when the task was not running.
pub(crate) fn finish_task(
    db: &mut dyn Db,
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
    let changed = db.execute(
        "UPDATE tasks SET status = ?2, result = ?3, error = ?4, updated_at = ?5, \
         finished_at = ?6 WHERE id = ?1 AND status = 'running'",
        &vals![
            id,
            status.as_str(),
            result.map(|r| truncate(r, MAX_TASK_RESULT_BYTES)),
            error.map(|e| truncate(e, 16 * 1024)),
            now_ms,
            finished,
        ],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get_task(db, id)
}

/// Cancels a `queued` or `blocked` task; anything else is returned unchanged (a running task is
/// cancelled by the orchestrator). `None` for an unknown id.
pub(crate) fn cancel_task(
    db: &mut dyn Db,
    id: i64,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    db.execute(
        "UPDATE tasks SET status = 'cancelled', error = ?2, updated_at = ?3, finished_at = ?3 \
         WHERE id = ?1 AND status IN ('queued', 'blocked')",
        &vals![id, reason, now.timestamp_millis()],
    )?;
    get_task(db, id)
}

/// Puts a `blocked` or `failed` task back to `queued`, appending `message` (the operator's reply)
/// to its prompt. Other statuses are refused with [`StoreError::InvalidArgument`].
pub(crate) fn requeue_task(
    db: &mut dyn Db,
    id: i64,
    message: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    let Some(task) = get_task(db, id)? else {
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
    db.execute(
        "UPDATE tasks SET status = 'queued', prompt = ?2, error = NULL, result = NULL, \
         finished_at = NULL, updated_at = ?3 WHERE id = ?1 AND status IN ('blocked', 'failed')",
        &vals![id, prompt, now.timestamp_millis()],
    )?;
    get_task(db, id)
}

/// A running task whose run was stopped for an outside reason (kill switch, daily budget) goes
/// back to `queued`, keeping `reason` as its error. Other statuses are left alone.
pub(crate) fn release_task(
    db: &mut dyn Db,
    id: i64,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    db.execute(
        "UPDATE tasks SET status = 'queued', error = ?2, updated_at = ?3 \
         WHERE id = ?1 AND status = 'running'",
        &vals![id, reason, now.timestamp_millis()],
    )?;
    get_task(db, id)
}

/// Tasks left `running` by a process that is gone (no live `task:<id>` lease) go back to
/// `queued`. Returns how many.
pub(crate) fn requeue_interrupted_tasks(db: &mut dyn Db, now: DateTime<Utc>) -> Result<u64> {
    db.execute(
        "UPDATE tasks SET status = 'queued', updated_at = ?1, \
         error = 'interrupted by restart; queued again' WHERE status = 'running' \
         AND NOT EXISTS (SELECT 1 FROM leases WHERE leases.key = 'task:' || tasks.id \
         AND leases.expires_at > ?1)",
        &vals![now.timestamp_millis()],
    )
}
