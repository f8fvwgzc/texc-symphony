//! Synchronous SQL operations. They run on the store's database thread against SQLite or
//! PostgreSQL through [`Db`]; all timestamps are passed in by the caller.

// Modified 2026-10-25: run costs, usage sums, control values, payload retention (audit A1/A4);
// one SQL dialect for SQLite and PostgreSQL, interrupted runs judged by their dispatch lease.

use chrono::{DateTime, Utc};

use crate::db::{Db, Row, Val, in_transaction, vals};
use crate::error::{Result, StoreError};
use crate::model::{
    MAX_MESSAGE_BYTES, MAX_PAYLOAD_BYTES, NewRun, PruneStats, RetryRecord, RunEvent, RunId,
    RunPage, RunQuery, RunRecord, RunStatus, TokenUsage, TotalsRecord, UsageTotals,
    effective_event_limit,
};

/// Error text recorded on runs that were still `running` when the process restarted.
pub const INTERRUPTED_ERROR: &str = "interrupted by restart";
/// Event kind appended to runs closed by `mark_interrupted_runs`.
pub const INTERRUPTED_EVENT_KIND: &str = "run_interrupted";

const RUN_COLUMNS: &str = "runs.id, issue_id, issue_identifier, issue_title, attempt, worker_host, \
     workspace_path, status, error, turns, started_at, finished_at, duration_ms, \
     input_tokens, output_tokens, total_tokens, COALESCE(run_costs.cost_micro_usd, 0)";

/// `runs` with their cost (kept in `run_costs`, see migration 0003).
const RUNS_WITH_COST: &str = "runs LEFT JOIN run_costs ON run_costs.run_id = runs.id";

/// `x - started_at`, never negative (no `MAX(a, b)`/`GREATEST` that works on both databases).
const DURATION: &str = "CASE WHEN ?2 - started_at > 0 THEN ?2 - started_at ELSE 0 END";

/// An event as submitted by a caller (before a seq is allocated).
#[derive(Debug, Clone)]
pub(crate) struct NewEvent {
    pub run_id: RunId,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub message: Option<String>,
    pub payload: Option<serde_json::Value>,
}

pub(crate) fn insert_run(db: &mut dyn Db, run: &NewRun, now: DateTime<Utc>) -> Result<RunId> {
    if run.issue_id.trim().is_empty() {
        return Err(StoreError::InvalidArgument(
            "issue_id must not be empty".to_string(),
        ));
    }
    let identifier = if run.issue_identifier.trim().is_empty() {
        run.issue_id.as_str()
    } else {
        run.issue_identifier.as_str()
    };
    let started_at = run.started_at.unwrap_or(now);
    let row = db.query_one(
        "INSERT INTO runs (issue_id, issue_identifier, issue_title, attempt, worker_host, \
         workspace_path, status, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7) \
         RETURNING id",
        &vals![
            run.issue_id.as_str(),
            identifier,
            run.issue_title.as_deref(),
            run.attempt,
            run.worker_host.as_deref(),
            run.workspace_path.as_deref(),
            started_at.timestamp_millis(),
        ],
    )?;
    Ok(RunId(row.int(0)?))
}

/// Allocate the next seq and insert the event. Must run inside a transaction.
fn insert_event(db: &mut dyn Db, event: &NewEvent) -> Result<i64> {
    if event.kind.trim().is_empty() {
        return Err(StoreError::InvalidArgument(
            "event kind must not be empty".to_string(),
        ));
    }
    let seq = db
        .query_opt(
            "UPDATE runs SET last_event_seq = last_event_seq + 1 WHERE id = ?1 \
             RETURNING last_event_seq",
            &vals![event.run_id.0],
        )?
        .ok_or(StoreError::RunNotFound(event.run_id))?
        .int(0)?;
    let message = event.message.as_deref().map(truncate_message);
    let payload = event.payload.as_ref().map(encode_payload).transpose()?;
    db.execute(
        "INSERT INTO run_events (run_id, seq, at, kind, message, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        &vals![
            event.run_id.0,
            seq,
            event.at.timestamp_millis(),
            event.kind.as_str(),
            message,
            payload,
        ],
    )?;
    Ok(seq)
}

pub(crate) fn append_event(db: &mut dyn Db, event: &NewEvent) -> Result<i64> {
    in_transaction(db, |db| insert_event(db, event))
}

pub(crate) fn update_tokens(db: &mut dyn Db, run_id: RunId, tokens: TokenUsage) -> Result<()> {
    let changed = db.execute(
        "UPDATE runs SET input_tokens = ?2, output_tokens = ?3, total_tokens = ?4 WHERE id = ?1",
        &vals![
            run_id.0,
            to_sql_u64(tokens.input),
            to_sql_u64(tokens.output),
            to_sql_u64(tokens.total),
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::RunNotFound(run_id));
    }
    Ok(())
}

/// Replaces the run's cumulative token usage and cost.
pub(crate) fn update_usage(
    db: &mut dyn Db,
    run_id: RunId,
    tokens: TokenUsage,
    cost_micro_usd: u64,
) -> Result<()> {
    update_tokens(db, run_id, tokens)?;
    db.execute(
        "INSERT INTO run_costs (run_id, cost_micro_usd) VALUES (?1, ?2) \
         ON CONFLICT (run_id) DO UPDATE SET cost_micro_usd = excluded.cost_micro_usd",
        &vals![run_id.0, to_sql_u64(cost_micro_usd)],
    )?;
    Ok(())
}

const USAGE_COLUMNS: &str = "CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT), \
     CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT), \
     CAST(COALESCE(SUM(total_tokens), 0) AS BIGINT), \
     CAST(COALESCE(SUM(run_costs.cost_micro_usd), 0) AS BIGINT)";

fn usage_from_row(row: &Row) -> Result<UsageTotals> {
    Ok(UsageTotals {
        tokens: TokenUsage {
            input: clamp_u64(row.int(0)?),
            output: clamp_u64(row.int(1)?),
            total: clamp_u64(row.int(2)?),
        },
        cost_micro_usd: clamp_u64(row.int(3)?),
    })
}

/// Usage of the runs that started at or after `since`.
pub(crate) fn usage_since(db: &mut dyn Db, since: DateTime<Utc>) -> Result<UsageTotals> {
    let sql = format!("SELECT {USAGE_COLUMNS} FROM {RUNS_WITH_COST} WHERE started_at >= ?1");
    usage_from_row(&db.query_one(&sql, &vals![since.timestamp_millis()])?)
}

/// Usage of every retained run of `issue_id`.
pub(crate) fn issue_usage(db: &mut dyn Db, issue_id: &str) -> Result<UsageTotals> {
    let sql = format!("SELECT {USAGE_COLUMNS} FROM {RUNS_WITH_COST} WHERE issue_id = ?1");
    usage_from_row(&db.query_one(&sql, &vals![issue_id])?)
}

/// A value of the `control` table.
pub(crate) fn get_control(db: &mut dyn Db, key: &str) -> Result<Option<String>> {
    db.query_opt("SELECT value FROM control WHERE key = ?1", &vals![key])?
        .map(|row| row.text(0))
        .transpose()
}

/// Sets (`Some`) or removes (`None`) a value of the `control` table.
pub(crate) fn set_control(db: &mut dyn Db, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => db.execute(
            "INSERT INTO control (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &vals![key, value],
        )?,
        None => db.execute("DELETE FROM control WHERE key = ?1", &vals![key])?,
    };
    Ok(())
}

/// Clears the payloads (raw agent messages, command output) of events recorded before `cutoff`;
/// kinds and messages stay. Returns the number of events changed.
pub(crate) fn prune_payloads(db: &mut dyn Db, cutoff: DateTime<Utc>) -> Result<u64> {
    db.execute(
        "UPDATE run_events SET payload = NULL WHERE payload IS NOT NULL AND at < ?1",
        &vals![cutoff.timestamp_millis()],
    )
}

pub(crate) fn increment_turns(db: &mut dyn Db, run_id: RunId) -> Result<u32> {
    db.query_opt(
        "UPDATE runs SET turns = turns + 1 WHERE id = ?1 RETURNING turns",
        &vals![run_id.0],
    )?
    .map(|row| row.int(0).map(clamp_u32))
    .transpose()?
    .ok_or(StoreError::RunNotFound(run_id))
}

pub(crate) fn update_runtime_info(
    db: &mut dyn Db,
    run_id: RunId,
    worker_host: Option<&str>,
    workspace_path: Option<&str>,
) -> Result<()> {
    let changed = db.execute(
        "UPDATE runs SET worker_host = COALESCE(?2, worker_host), \
         workspace_path = COALESCE(?3, workspace_path) WHERE id = ?1",
        &vals![run_id.0, worker_host, workspace_path],
    )?;
    if changed == 0 {
        return Err(StoreError::RunNotFound(run_id));
    }
    Ok(())
}

pub(crate) fn finish_run(
    db: &mut dyn Db,
    run_id: RunId,
    status: RunStatus,
    error: Option<&str>,
    now: DateTime<Utc>,
) -> Result<RunRecord> {
    if !status.is_terminal() {
        return Err(StoreError::InvalidArgument(
            "finish_run requires a terminal status".to_string(),
        ));
    }
    let changed = db.execute(
        &format!(
            "UPDATE runs SET status = ?3, error = ?4, finished_at = ?2, duration_ms = {DURATION} \
             WHERE id = ?1 AND status = 'running'"
        ),
        &vals![run_id.0, now.timestamp_millis(), status.as_str(), error],
    )?;
    match get_run(db, run_id)? {
        Some(record) if changed > 0 => Ok(record),
        Some(_) => Err(StoreError::RunAlreadyFinished(run_id)),
        None => Err(StoreError::RunNotFound(run_id)),
    }
}

pub(crate) fn get_run(db: &mut dyn Db, run_id: RunId) -> Result<Option<RunRecord>> {
    let sql = format!("SELECT {RUN_COLUMNS} FROM {RUNS_WITH_COST} WHERE runs.id = ?1");
    db.query_opt(&sql, &vals![run_id.0])?
        .map(|row| run_from_row(&row))
        .transpose()
}

pub(crate) fn list_runs(db: &mut dyn Db, query: &RunQuery) -> Result<RunPage> {
    let limit = query.effective_limit();
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Val> = Vec::new();
    if let Some(before) = query.before_id {
        values.push(Val::Int(before));
        clauses.push(format!("runs.id < ?{}", values.len()));
    }
    if let Some(identifier) = &query.issue_identifier {
        values.push(Val::Text(identifier.clone()));
        clauses.push(format!("issue_identifier = ?{}", values.len()));
    }
    if let Some(issue_id) = &query.issue_id {
        values.push(Val::Text(issue_id.clone()));
        clauses.push(format!("issue_id = ?{}", values.len()));
    }
    if let Some(status) = query.status {
        values.push(Val::Text(status.as_str().to_string()));
        clauses.push(format!("status = ?{}", values.len()));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    // Fetch one extra row to know whether another page exists.
    values.push(Val::Int(i64::from(limit) + 1));
    let sql = format!(
        "SELECT {RUN_COLUMNS} FROM {RUNS_WITH_COST} {where_sql} ORDER BY runs.id DESC LIMIT ?{}",
        values.len()
    );
    let mut runs = db
        .query(&sql, &values)?
        .iter()
        .map(run_from_row)
        .collect::<Result<Vec<_>>>()?;
    let next_before_id = if runs.len() > limit as usize {
        runs.truncate(limit as usize);
        runs.last().map(|run| run.id.0)
    } else {
        None
    };
    Ok(RunPage {
        runs,
        next_before_id,
    })
}

pub(crate) fn list_events(
    db: &mut dyn Db,
    run_id: RunId,
    after_seq: Option<i64>,
    limit: Option<u32>,
) -> Result<Vec<RunEvent>> {
    let limit = effective_event_limit(limit);
    db.query(
        "SELECT run_id, seq, at, kind, message, payload FROM run_events \
         WHERE run_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
        &vals![run_id.0, after_seq.unwrap_or(0), i64::from(limit)],
    )?
    .iter()
    .map(event_from_row)
    .collect()
}

pub(crate) fn totals(db: &mut dyn Db) -> Result<TotalsRecord> {
    let row = db.query_one(
        "SELECT COUNT(*), \
         CAST(COALESCE(SUM(CASE WHEN status = 'succeeded' THEN 1 ELSE 0 END), 0) AS BIGINT), \
         CAST(COALESCE(SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) AS BIGINT), \
         CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT), \
         CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT), \
         CAST(COALESCE(SUM(total_tokens), 0) AS BIGINT), \
         CAST(COALESCE(SUM(CASE WHEN duration_ms > 0 THEN duration_ms ELSE 0 END), 0) AS BIGINT), \
         CAST(COALESCE(SUM(run_costs.cost_micro_usd), 0) AS BIGINT) \
         FROM runs LEFT JOIN run_costs ON run_costs.run_id = runs.id",
        &[],
    )?;
    Ok(TotalsRecord {
        runs_total: clamp_u64(row.int(0)?),
        runs_succeeded: clamp_u64(row.int(1)?),
        runs_failed: clamp_u64(row.int(2)?),
        tokens: TokenUsage {
            input: clamp_u64(row.int(3)?),
            output: clamp_u64(row.int(4)?),
            total: clamp_u64(row.int(5)?),
        },
        runtime_ms: clamp_u64(row.int(6)?),
        cost_micro_usd: clamp_u64(row.int(7)?),
    })
}

/// Closes runs left `running` by a process that is gone: runs whose issue no live dispatch lease
/// covers (see `leases`). With one process this is every running run at startup; with replicas
/// sharing a PostgreSQL database, runs of live replicas are left alone.
pub(crate) fn mark_interrupted_runs(db: &mut dyn Db, now: DateTime<Utc>) -> Result<u64> {
    in_transaction(db, |db| {
        let ids = db
            .query(
                &format!(
                    "UPDATE runs SET status = 'cancelled', error = ?1, finished_at = ?2, \
                     duration_ms = {DURATION} WHERE status = 'running' AND NOT EXISTS \
                     (SELECT 1 FROM leases WHERE leases.key = runs.issue_id \
                     AND leases.expires_at > ?2) RETURNING id"
                ),
                &vals![INTERRUPTED_ERROR, now.timestamp_millis()],
            )?
            .iter()
            .map(|row| row.int(0))
            .collect::<Result<Vec<_>>>()?;
        for id in &ids {
            insert_event(
                db,
                &NewEvent {
                    run_id: RunId(*id),
                    at: now,
                    kind: INTERRUPTED_EVENT_KIND.to_string(),
                    message: Some(INTERRUPTED_ERROR.to_string()),
                    payload: None,
                },
            )?;
        }
        Ok(ids.len() as u64)
    })
}

pub(crate) fn prune(
    db: &mut dyn Db,
    cutoff: DateTime<Utc>,
    keep_min_runs: u32,
) -> Result<PruneStats> {
    const DOOMED: &str = "SELECT id FROM runs WHERE status != 'running' AND started_at < ?1 \
         AND id NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT ?2)";
    in_transaction(db, |db| {
        let args = vals![cutoff.timestamp_millis(), i64::from(keep_min_runs)];
        // Delete events explicitly (not only via ON DELETE CASCADE) so the count is exact.
        let events_deleted = db.execute(
            &format!("DELETE FROM run_events WHERE run_id IN ({DOOMED})"),
            &args,
        )?;
        db.execute(
            &format!("DELETE FROM run_costs WHERE run_id IN ({DOOMED})"),
            &args,
        )?;
        let runs_deleted =
            db.execute(&format!("DELETE FROM runs WHERE id IN ({DOOMED})"), &args)?;
        Ok(PruneStats {
            runs_deleted,
            events_deleted,
        })
    })
}

fn run_from_row(row: &Row) -> Result<RunRecord> {
    let status = row.text(7)?.parse::<RunStatus>()?;
    Ok(RunRecord {
        id: RunId(row.int(0)?),
        issue_id: row.text(1)?,
        issue_identifier: row.text(2)?,
        issue_title: row.opt_text(3)?,
        attempt: clamp_u32(row.int(4)?),
        worker_host: row.opt_text(5)?,
        workspace_path: row.opt_text(6)?,
        status,
        error: row.opt_text(8)?,
        turns: clamp_u32(row.int(9)?),
        started_at: timestamp(row.int(10)?)?,
        finished_at: row.opt_int(11)?.map(timestamp).transpose()?,
        duration_ms: row.opt_int(12)?,
        tokens: TokenUsage {
            input: clamp_u64(row.int(13)?),
            output: clamp_u64(row.int(14)?),
            total: clamp_u64(row.int(15)?),
        },
        cost_micro_usd: clamp_u64(row.int(16)?),
    })
}

fn event_from_row(row: &Row) -> Result<RunEvent> {
    let payload = row
        .opt_text(5)?
        .map(|text| {
            serde_json::from_str(&text)
                .map_err(|err| StoreError::CorruptRow(format!("event payload: {err}")))
        })
        .transpose()?;
    Ok(RunEvent {
        run_id: RunId(row.int(0)?),
        seq: row.int(1)?,
        at: timestamp(row.int(2)?)?,
        kind: row.text(3)?,
        message: row.opt_text(4)?,
        payload,
    })
}

/// A unix-milliseconds column as a timestamp.
pub(crate) fn timestamp(ms: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(ms)
        .ok_or_else(|| StoreError::CorruptRow(format!("timestamp out of range: {ms}")))
}

fn clamp_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(crate) fn clamp_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

pub(crate) fn to_sql_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Cap `message` at [`MAX_MESSAGE_BYTES`], cutting on a char boundary.
fn truncate_message(message: &str) -> String {
    if message.len() <= MAX_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut end = MAX_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &message[..end])
}

/// Encode a payload as JSON text, replacing oversized documents with a marker object.
fn encode_payload(payload: &serde_json::Value) -> Result<String> {
    let text = serde_json::to_string(payload)
        .map_err(|err| StoreError::InvalidArgument(format!("payload is not valid JSON: {err}")))?;
    if text.len() <= MAX_PAYLOAD_BYTES {
        return Ok(text);
    }
    Ok(serde_json::json!({"truncated": true, "original_bytes": text.len()}).to_string())
}

/// Queues (or replaces) the retry of `retry.issue_id`.
pub(crate) fn save_retry(db: &mut dyn Db, retry: &RetryRecord) -> Result<()> {
    db.execute(
        "INSERT INTO retry_queue \
         (issue_id, attempt, due_at, identifier, issue_url, error, worker_host, workspace_path) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (issue_id) DO UPDATE SET attempt = excluded.attempt, \
         due_at = excluded.due_at, identifier = excluded.identifier, \
         issue_url = excluded.issue_url, error = excluded.error, \
         worker_host = excluded.worker_host, workspace_path = excluded.workspace_path",
        &vals![
            retry.issue_id.as_str(),
            retry.attempt,
            retry.due_at.timestamp_millis(),
            retry.identifier.as_str(),
            retry.issue_url.as_deref(),
            retry.error.as_deref(),
            retry.worker_host.as_deref(),
            retry.workspace_path.as_deref(),
        ],
    )?;
    Ok(())
}

/// Removes the queued retry of `issue_id`, if any.
pub(crate) fn delete_retry(db: &mut dyn Db, issue_id: &str) -> Result<()> {
    db.execute(
        "DELETE FROM retry_queue WHERE issue_id = ?1",
        &vals![issue_id],
    )?;
    Ok(())
}

/// Every queued retry, soonest first.
pub(crate) fn list_retries(db: &mut dyn Db) -> Result<Vec<RetryRecord>> {
    db.query(
        "SELECT issue_id, attempt, due_at, identifier, issue_url, error, worker_host, \
         workspace_path FROM retry_queue ORDER BY due_at, issue_id",
        &[],
    )?
    .iter()
    .map(|row| {
        let due_ms = row.int(2)?;
        Ok(RetryRecord {
            issue_id: row.text(0)?,
            attempt: clamp_u32(row.int(1)?),
            due_at: DateTime::from_timestamp_millis(due_ms)
                .ok_or_else(|| StoreError::CorruptRow(format!("invalid retry due_at {due_ms}")))?,
            identifier: row.text(3)?,
            issue_url: row.opt_text(4)?,
            error: row.opt_text(5)?,
            worker_host: row.opt_text(6)?,
            workspace_path: row.opt_text(7)?,
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_messages_are_truncated_on_char_boundaries() {
        let long = "é".repeat(MAX_MESSAGE_BYTES);
        let cut = truncate_message(&long);
        assert!(cut.ends_with("…[truncated]"));
        assert!(cut.len() <= MAX_MESSAGE_BYTES + "…[truncated]".len());
        assert_eq!(truncate_message("short"), "short");
    }

    #[test]
    fn oversized_payloads_are_replaced_by_a_marker() {
        let big = serde_json::json!({"blob": "x".repeat(MAX_PAYLOAD_BYTES)});
        let encoded: serde_json::Value =
            serde_json::from_str(&encode_payload(&big).unwrap()).unwrap();
        assert_eq!(encoded["truncated"], true);
        assert!(encoded["original_bytes"].as_u64().unwrap() > MAX_PAYLOAD_BYTES as u64);
    }

    #[test]
    fn clamps_out_of_range_integers() {
        assert_eq!(clamp_u64(-5), 0);
        assert_eq!(clamp_u32(i64::MAX), u32::MAX);
        assert_eq!(clamp_u32(-1), 0);
        assert_eq!(to_sql_u64(u64::MAX), i64::MAX);
    }
}
