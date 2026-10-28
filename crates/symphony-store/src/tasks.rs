//! Tasks: work submitted through the task API instead of a tracker (records and SQL).
//!
//! Lifecycle: `queued` → `running` (the orchestrator dispatched it) → `succeeded` / `failed` /
//! `cancelled` / `blocked`. `blocked` (the agent needs a human) and `failed` tasks can be
//! re-queued with an operator message; queued and blocked tasks can be cancelled directly, running
//! ones through the orchestrator. A `running` task whose dispatch lease (`task:<id>`, see
//! `leases`) is gone was interrupted (its process stopped) and goes back to `queued`.
//!
//! Retries (`task_retry`): a run that ends in a failure the task's policy retries (`error`,
//! `timeout`, `interrupted`) while fewer than `max_attempts` attempts have failed goes back to
//! `queued`, not before `backoff_ms * 2^(failures - 1)` (at most `max_backoff_ms`; at once after an
//! interruption). A run handed off on purpose (kill switch, daily budget, shutdown) does not count.
//! Writes for a running task can be fenced by its dispatch lease: a replica that lost the lease
//! (another took the task over) cannot finish or release it.
//!
//! Dependencies (`task_dependencies`, set at creation and only on tasks that already exist, so
//! there are no cycles): a queued task with a dependency that has not succeeded is `waiting`, a
//! status derived from the dependencies rather than stored, and is not dispatched. Once a
//! dependency has failed or was cancelled, its waiting dependants fail too, transitively
//! ([`settle_dependants`]: when a task finishes, and on every dispatch pass for writes that
//! raced).

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::{Db, Row, Val, in_transaction, vals};
use crate::error::{Result, StoreError};
use crate::ops::{clamp_u32, timestamp};

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
    /// Queued, but a dependency has not succeeded yet (derived, never stored).
    Waiting,
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
    pub const ALL: [TaskStatus; 7] = [
        TaskStatus::Queued,
        TaskStatus::Waiting,
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
            TaskStatus::Waiting => "waiting",
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

/// A kind of failed run a task's retry policy can retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskFailure {
    /// The agent's run failed.
    Error,
    /// The run hit its `timeout_ms` or stalled.
    Timeout,
    /// The replica running it stopped (its dispatch lease expired).
    Interrupted,
}

impl TaskFailure {
    /// Every kind.
    pub const ALL: [TaskFailure; 3] = [
        TaskFailure::Error,
        TaskFailure::Timeout,
        TaskFailure::Interrupted,
    ];

    /// Wire/database spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskFailure::Error => "error",
            TaskFailure::Timeout => "timeout",
            TaskFailure::Interrupted => "interrupted",
        }
    }
}

/// Default [`TaskRetry::max_attempts`].
pub const DEFAULT_TASK_MAX_ATTEMPTS: u32 = 3;
/// Most attempts a retry policy may allow.
pub const MAX_TASK_ATTEMPTS: u32 = 20;
/// Default [`TaskRetry::backoff_ms`].
pub const DEFAULT_TASK_BACKOFF_MS: u64 = 10_000;
/// Default [`TaskRetry::max_backoff_ms`].
pub const DEFAULT_TASK_MAX_BACKOFF_MS: u64 = 300_000;

/// A task's retry policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRetry {
    /// Attempts that may fail before the task does (`1..=20`; `1`: never retried).
    pub max_attempts: u32,
    /// Delay before the first retry; doubled per failed attempt.
    pub backoff_ms: u64,
    /// Longest delay between attempts.
    pub max_backoff_ms: u64,
    /// Failures that are retried (an interrupted run is retried at once).
    pub on: Vec<TaskFailure>,
}

impl Default for TaskRetry {
    /// Three attempts, retrying only runs interrupted by a stopped replica.
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_TASK_MAX_ATTEMPTS,
            backoff_ms: DEFAULT_TASK_BACKOFF_MS,
            max_backoff_ms: DEFAULT_TASK_MAX_BACKOFF_MS,
            on: vec![TaskFailure::Interrupted],
        }
    }
}

impl TaskRetry {
    /// Delay before the attempt after `failures` failed ones (`failures >= 1`).
    pub fn delay_ms(&self, failures: u32, failure: TaskFailure) -> u64 {
        if failure == TaskFailure::Interrupted {
            return 0;
        }
        let power = failures.saturating_sub(1).min(30);
        self.backoff_ms
            .saturating_mul(1u64 << power)
            .min(self.max_backoff_ms)
    }

    fn on_text(&self) -> String {
        let kinds: Vec<&str> = TaskFailure::ALL
            .into_iter()
            .filter(|kind| self.on.contains(kind))
            .map(TaskFailure::as_str)
            .collect();
        kinds.join(",")
    }

    fn on_from_text(text: &str) -> Vec<TaskFailure> {
        TaskFailure::ALL
            .into_iter()
            .filter(|kind| text.split(',').any(|part| part == kind.as_str()))
            .collect()
    }
}

/// Most dependencies one task may have.
pub const MAX_TASK_DEPENDENCIES: usize = 100;

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
    /// Existing tasks that must succeed before this one is dispatched.
    #[serde(default)]
    pub depends_on: Vec<i64>,
    /// Retry policy.
    #[serde(default)]
    pub retry: TaskRetry,
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
    /// Tasks that must succeed before this one runs (ascending ids).
    #[serde(default)]
    pub depends_on: Vec<i64>,
    /// Current status.
    pub status: TaskStatus,
    /// Final agent message of a succeeded task.
    pub result: Option<String>,
    /// Why it failed, was cancelled or is blocked.
    pub error: Option<String>,
    /// Runs started so far.
    pub attempts: u32,
    /// Retry policy.
    pub retry: TaskRetry,
    /// Failed attempts counted against `retry.max_attempts` (since creation or the last unblock).
    pub failed_attempts: u32,
    /// When a retried task may run again (`None`: as soon as a slot is free).
    pub next_attempt_at: Option<DateTime<Utc>>,
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

/// A dependency of the `tasks` row in scope that has not succeeded.
macro_rules! unmet_dependency {
    () => {
        "EXISTS (SELECT 1 FROM task_dependencies d JOIN tasks p ON p.id = d.depends_on \
         WHERE d.task_id = tasks.id AND p.status <> 'succeeded')"
    };
}

/// A dependency of the `tasks` row in scope that failed or was cancelled.
macro_rules! failed_dependency {
    () => {
        "EXISTS (SELECT 1 FROM task_dependencies d JOIN tasks p ON p.id = d.depends_on \
         WHERE d.task_id = tasks.id AND p.status IN ('failed', 'cancelled'))"
    };
}

/// The reported status: `waiting` for a queued task with an unmet dependency.
const STATUS: &str = concat!(
    "(CASE WHEN tasks.status = 'queued' AND ",
    unmet_dependency!(),
    " THEN 'waiting' ELSE tasks.status END)"
);

const COLUMNS: &str = concat!(
    "id, title, prompt, repo, branch, backend, max_tokens, timeout_ms, metadata, ",
    "(CASE WHEN tasks.status = 'queued' AND ",
    unmet_dependency!(),
    " THEN 'waiting' ELSE tasks.status END), ",
    "result, error, attempts, created_at, updated_at, started_at, finished_at, ",
    "COALESCE(r.max_attempts, 3), COALESCE(r.backoff_ms, 10000), ",
    "COALESCE(r.max_backoff_ms, 300000), COALESCE(r.retry_on, 'interrupted'), ",
    "COALESCE(r.failures, 0), r.not_before"
);

/// `tasks` with their retry policy (`r`).
const TASKS: &str = "tasks LEFT JOIN task_retry r ON r.task_id = tasks.id";

/// The `task:<id>` dispatch lease of the `tasks` row in scope is live and held by `?owner`.
macro_rules! held_by {
    ($owner:literal, $now:literal) => {
        concat!(
            "EXISTS (SELECT 1 FROM leases WHERE leases.key = 'task:' || tasks.id ",
            "AND leases.owner = CAST(",
            $owner,
            " AS TEXT) AND leases.expires_at > ",
            $now,
            ")"
        )
    };
}

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
        depends_on: Vec::new(),
        status,
        result: row.opt_text(10)?,
        error: row.opt_text(11)?,
        attempts: clamp_u32(row.int(12)?),
        created_at: timestamp(row.int(13)?)?,
        updated_at: timestamp(row.int(14)?)?,
        started_at: row.opt_int(15)?.map(timestamp).transpose()?,
        finished_at: row.opt_int(16)?.map(timestamp).transpose()?,
        retry: TaskRetry {
            max_attempts: clamp_u32(row.int(17)?),
            backoff_ms: opt_u64(Some(row.int(18)?)).unwrap_or(0),
            max_backoff_ms: opt_u64(Some(row.int(19)?)).unwrap_or(0),
            on: TaskRetry::on_from_text(&row.text(20)?),
        },
        failed_attempts: clamp_u32(row.int(21)?),
        next_attempt_at: row.opt_int(22)?.map(timestamp).transpose()?,
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
    if !(1..=MAX_TASK_ATTEMPTS).contains(&task.retry.max_attempts)
        || task.retry.max_backoff_ms < task.retry.backoff_ms
    {
        return Err(StoreError::InvalidArgument(format!(
            "retry: max_attempts must be 1-{MAX_TASK_ATTEMPTS} and max_backoff_ms at least backoff_ms"
        )));
    }
    let mut depends_on = task.depends_on.clone();
    depends_on.sort_unstable();
    depends_on.dedup();
    if depends_on.len() > MAX_TASK_DEPENDENCIES {
        return Err(StoreError::InvalidArgument(format!(
            "depends_on must list at most {MAX_TASK_DEPENDENCIES} tasks"
        )));
    }
    let metadata = task
        .metadata
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|err| StoreError::InvalidArgument(format!("metadata: {err}")))?;
    let now_ms = now.timestamp_millis();
    let id = in_transaction(db, |db| {
        let states = task_states(db, &depends_on)?;
        for dependency in &depends_on {
            match states.iter().find(|(id, _)| id == dependency) {
                None => {
                    return Err(StoreError::InvalidArgument(format!(
                        "depends_on: task {dependency} does not exist"
                    )));
                }
                Some((_, status @ (TaskStatus::Failed | TaskStatus::Cancelled))) => {
                    return Err(StoreError::Conflict(format!(
                        "dependency {} is {status}; unblock it first",
                        task_identifier(*dependency)
                    )));
                }
                Some(_) => {}
            }
        }
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
        for dependency in &depends_on {
            db.execute(
                "INSERT INTO task_dependencies (task_id, depends_on) VALUES (?1, ?2)",
                &vals![id, *dependency],
            )?;
        }
        db.execute(
            "INSERT INTO task_retry (task_id, max_attempts, backoff_ms, max_backoff_ms, retry_on) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            &vals![
                id,
                i64::from(task.retry.max_attempts),
                to_sql(Some(task.retry.backoff_ms)),
                to_sql(Some(task.retry.max_backoff_ms)),
                task.retry.on_text(),
            ],
        )?;
        Ok(id)
    })?;
    get_task(db, id)?.ok_or_else(|| StoreError::Internal("inserted task vanished".into()))
}

/// The reported status of each of `ids` that exists.
pub(crate) fn task_states(db: &mut dyn Db, ids: &[i64]) -> Result<Vec<(i64, TaskStatus)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
    let values: Vec<Val> = ids.iter().map(|id| Val::Int(*id)).collect();
    db.query(
        &format!(
            "SELECT id, {STATUS} FROM tasks WHERE id IN ({}) ORDER BY id",
            placeholders.join(", ")
        ),
        &values,
    )?
    .iter()
    .map(|row| Ok((row.int(0)?, row.text(1)?.parse::<TaskStatus>()?)))
    .collect()
}

/// Fills `depends_on` of `tasks`.
fn attach_dependencies(db: &mut dyn Db, tasks: &mut [TaskRecord]) -> Result<()> {
    if tasks.is_empty() {
        return Ok(());
    }
    let placeholders: Vec<String> = (1..=tasks.len()).map(|i| format!("?{i}")).collect();
    let values: Vec<Val> = tasks.iter().map(|t| Val::Int(t.id)).collect();
    let rows = db.query(
        &format!(
            "SELECT task_id, depends_on FROM task_dependencies WHERE task_id IN ({}) \
             ORDER BY task_id, depends_on",
            placeholders.join(", ")
        ),
        &values,
    )?;
    for row in rows {
        let (task_id, dependency) = (row.int(0)?, row.int(1)?);
        if let Some(task) = tasks.iter_mut().find(|t| t.id == task_id) {
            task.depends_on.push(dependency);
        }
    }
    Ok(())
}

fn with_dependencies(db: &mut dyn Db, mut tasks: Vec<TaskRecord>) -> Result<Vec<TaskRecord>> {
    attach_dependencies(db, &mut tasks)?;
    Ok(tasks)
}

pub(crate) fn get_task(db: &mut dyn Db, id: i64) -> Result<Option<TaskRecord>> {
    let task = db
        .query_opt(
            &format!("SELECT {COLUMNS} FROM {TASKS} WHERE id = ?1"),
            &vals![id],
        )?
        .map(|row| task_from_row(&row))
        .transpose()?;
    Ok(match task {
        Some(task) => with_dependencies(db, vec![task])?.pop(),
        None => None,
    })
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
        clauses.push(format!("{STATUS} = ?{}", values.len()));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    values.push(Val::Int(i64::from(limit) + 1));
    let sql = format!(
        "SELECT {COLUMNS} FROM {TASKS} {where_sql} ORDER BY id DESC LIMIT ?{}",
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
        tasks: with_dependencies(db, tasks)?,
        next_before_id,
    })
}

/// Queued tasks whose dependencies have all succeeded and whose retry delay is over, oldest first.
pub(crate) fn queued_tasks(
    db: &mut dyn Db,
    limit: u32,
    now: DateTime<Utc>,
) -> Result<Vec<TaskRecord>> {
    let tasks = db
        .query(
            &format!(
                "SELECT {COLUMNS} FROM {TASKS} WHERE status = 'queued' AND NOT {} \
                 AND (r.not_before IS NULL OR r.not_before <= ?2) ORDER BY id ASC LIMIT ?1",
                unmet_dependency!()
            ),
            &vals![i64::from(limit), now.timestamp_millis()],
        )?
        .iter()
        .map(task_from_row)
        .collect::<Result<Vec<_>>>()?;
    with_dependencies(db, tasks)
}

/// `queued` → `running` (attempts + 1). `None` when the task was not queued (any more) or waits
/// for a dependency: the conditional update makes one claimant win, also across replicas.
pub(crate) fn start_task(
    db: &mut dyn Db,
    id: i64,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    let changed = db.execute(
        concat!(
            "UPDATE tasks SET status = 'running', attempts = attempts + 1, started_at = ?2, ",
            "updated_at = ?2, error = NULL WHERE id = ?1 AND status = 'queued' AND NOT ",
            unmet_dependency!()
        ),
        &vals![id, now.timestamp_millis()],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get_task(db, id)
}

/// Fails every queued task with a dependency that failed or was cancelled, transitively; returns
/// how many.
pub(crate) fn settle_dependants(db: &mut dyn Db, now: DateTime<Utc>) -> Result<u64> {
    let mut failed = 0;
    // Each pass fails one more level of dependants; a level with none ends it.
    loop {
        let changed = db.execute(
            concat!(
                "UPDATE tasks SET status = 'failed', updated_at = ?1, finished_at = ?1, ",
                "error = 'dependency task-' || (SELECT MIN(p.id) FROM task_dependencies d ",
                "JOIN tasks p ON p.id = d.depends_on WHERE d.task_id = tasks.id ",
                "AND p.status IN ('failed', 'cancelled')) || ' did not succeed' ",
                "WHERE status = 'queued' AND ",
                failed_dependency!()
            ),
            &vals![now.timestamp_millis()],
        )?;
        if changed == 0 {
            return Ok(failed);
        }
        failed += changed;
    }
}

/// `running` → `status` (a final status or `blocked`). `None` when the task was not running, or
/// when `owner` is given and no longer holds the task's live dispatch lease (another replica took
/// it over). A `failed` run of a kind the task's policy retries ([`TaskFailure`]), with attempts
/// left, goes back to `queued` instead (`next_attempt_at` after its backoff). A task that ends
/// failed or cancelled fails its waiting dependants.
#[allow(clippy::too_many_arguments)] // One row's whole outcome; a struct would only rename them.
pub(crate) fn finish_task(
    db: &mut dyn Db,
    id: i64,
    status: TaskStatus,
    result: Option<&str>,
    error: Option<&str>,
    failure: Option<TaskFailure>,
    owner: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    if matches!(
        status,
        TaskStatus::Queued | TaskStatus::Waiting | TaskStatus::Running
    ) {
        return Err(StoreError::InvalidArgument(
            "finish_task needs a final or blocked status".into(),
        ));
    }
    let now_ms = now.timestamp_millis();
    let finished = status.is_final().then_some(now_ms);
    let changed = in_transaction(db, |db| {
        let changed = db.execute(
            concat!(
                "UPDATE tasks SET status = ?2, result = ?3, error = ?4, updated_at = ?5, ",
                "finished_at = ?6 WHERE id = ?1 AND status = 'running' ",
                "AND (CAST(?7 AS TEXT) IS NULL OR ",
                held_by!("?7", "?5"),
                ")"
            ),
            &vals![
                id,
                status.as_str(),
                result.map(|r| truncate(r, MAX_TASK_RESULT_BYTES)),
                error.map(|e| truncate(e, 16 * 1024)),
                now_ms,
                finished,
                owner,
            ],
        )?;
        if changed == 0 {
            return Ok(0);
        }
        if let (TaskStatus::Failed, Some(failure)) = (status, failure) {
            retry_failed(db, id, failure, now)?;
        }
        settle_dependants(db, now)?;
        Ok(changed)
    })?;
    if changed == 0 {
        return Ok(None);
    }
    get_task(db, id)
}

/// Counts a failed attempt of the just-failed task `id` and, when its policy retries `failure`
/// and attempts are left, queues it again after its backoff.
fn retry_failed(db: &mut dyn Db, id: i64, failure: TaskFailure, now: DateTime<Utc>) -> Result<()> {
    db.execute(
        "UPDATE task_retry SET failures = failures + 1 WHERE task_id = ?1",
        &vals![id],
    )?;
    let Some(task) = get_task(db, id)? else {
        return Ok(());
    };
    if !task.retry.on.contains(&failure) || task.failed_attempts >= task.retry.max_attempts {
        return Ok(());
    }
    let delay = task.retry.delay_ms(task.failed_attempts, failure);
    let not_before = now
        .timestamp_millis()
        .saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
    db.execute(
        "UPDATE tasks SET status = 'queued', finished_at = NULL WHERE id = ?1",
        &vals![id],
    )?;
    db.execute(
        "UPDATE task_retry SET not_before = ?2 WHERE task_id = ?1",
        &vals![id, (delay > 0).then_some(not_before)],
    )?;
    Ok(())
}

/// Cancels a `queued` (or `waiting`) or `blocked` task, failing its waiting dependants; anything
/// else is returned unchanged (a running task is cancelled by the orchestrator). `None` for an
/// unknown id.
pub(crate) fn cancel_task(
    db: &mut dyn Db,
    id: i64,
    reason: &str,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    in_transaction(db, |db| {
        let changed = db.execute(
            "UPDATE tasks SET status = 'cancelled', error = ?2, updated_at = ?3, finished_at = ?3 \
             WHERE id = ?1 AND status IN ('queued', 'blocked')",
            &vals![id, reason, now.timestamp_millis()],
        )?;
        if changed > 0 {
            settle_dependants(db, now)?;
        }
        Ok(())
    })?;
    get_task(db, id)
}

/// Puts a `blocked` or `failed` task back to `queued` (or `waiting`), appending `message` (the
/// operator's reply) to its prompt. Other statuses are refused with
/// [`StoreError::InvalidArgument`], and a task with a failed or cancelled dependency with
/// [`StoreError::Conflict`] (unblock the dependency first).
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
    if let Some((dependency, status)) = task_states(db, &task.depends_on)?
        .into_iter()
        .find(|(_, status)| matches!(status, TaskStatus::Failed | TaskStatus::Cancelled))
    {
        return Err(StoreError::Conflict(format!(
            "task {id} depends on {}, which is {status}; unblock it first",
            task_identifier(dependency)
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
    in_transaction(db, |db| {
        db.execute(
            "UPDATE tasks SET status = 'queued', prompt = ?2, error = NULL, result = NULL, \
             finished_at = NULL, updated_at = ?3 WHERE id = ?1 AND status IN ('blocked', 'failed')",
            &vals![id, prompt, now.timestamp_millis()],
        )?;
        // An unblocked task gets its policy's attempts again.
        db.execute(
            "UPDATE task_retry SET failures = 0, not_before = NULL WHERE task_id = ?1",
            &vals![id],
        )
    })?;
    get_task(db, id)
}

/// A running task whose run was stopped for an outside reason (kill switch, daily budget,
/// shutdown) goes back to `queued`, keeping `reason` as its error; the attempt does not count
/// against its retry policy. Other statuses, and a task whose lease `owner` no longer holds, are
/// left alone.
pub(crate) fn release_task(
    db: &mut dyn Db,
    id: i64,
    reason: &str,
    owner: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<TaskRecord>> {
    db.execute(
        concat!(
            "UPDATE tasks SET status = 'queued', error = ?2, updated_at = ?3 ",
            "WHERE id = ?1 AND status = 'running' AND (CAST(?4 AS TEXT) IS NULL OR ",
            held_by!("?4", "?3"),
            ")"
        ),
        &vals![id, reason, now.timestamp_millis(), owner],
    )?;
    get_task(db, id)
}

/// Tasks left `running` by a process that is gone (no live `task:<id>` lease) were interrupted:
/// each counts a failed attempt and goes back to `queued` while its policy retries `interrupted`
/// with attempts left, and fails otherwise (failing its waiting dependants). Returns how many.
pub(crate) fn requeue_interrupted_tasks(db: &mut dyn Db, now: DateTime<Utc>) -> Result<u64> {
    in_transaction(db, |db| {
        let orphaned = "status = 'running' AND NOT EXISTS (SELECT 1 FROM leases \
             WHERE leases.key = 'task:' || tasks.id AND leases.expires_at > ?1)";
        db.execute(
            &format!(
                "UPDATE task_retry SET failures = failures + 1 \
                 WHERE task_id IN (SELECT id FROM tasks WHERE {orphaned})"
            ),
            &vals![now.timestamp_millis()],
        )?;
        let retried = "EXISTS (SELECT 1 FROM task_retry r WHERE r.task_id = tasks.id \
             AND r.retry_on LIKE '%interrupted%' AND r.failures < r.max_attempts)";
        let changed = db.execute(
            &format!(
                "UPDATE tasks SET updated_at = ?1, \
                 status = CASE WHEN {retried} THEN 'queued' ELSE 'failed' END, \
                 finished_at = CASE WHEN {retried} THEN NULL ELSE ?1 END, \
                 error = CASE WHEN {retried} THEN 'interrupted by restart; queued again' \
                 ELSE 'interrupted by restart; no attempts left' END \
                 WHERE {orphaned}"
            ),
            &vals![now.timestamp_millis()],
        )?;
        settle_dependants(db, now)?;
        Ok(changed)
    })
}
