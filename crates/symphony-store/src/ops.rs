//! Synchronous SQL operations. They run on the store's database thread; every function takes the
//! connection it should use and all timestamps are passed in by the caller.

use chrono::{DateTime, Utc};
use rusqlite::types::{Type, Value};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, params_from_iter};

use crate::error::{Result, StoreError};
use crate::model::{
    MAX_MESSAGE_BYTES, MAX_PAYLOAD_BYTES, NewRun, PruneStats, RunEvent, RunId, RunPage, RunQuery,
    RunRecord, RunStatus, TokenUsage, TotalsRecord, effective_event_limit,
};

/// Error text recorded on runs that were still `running` when the process restarted.
pub const INTERRUPTED_ERROR: &str = "interrupted by restart";
/// Event kind appended to runs closed by `mark_interrupted_runs`.
pub const INTERRUPTED_EVENT_KIND: &str = "run_interrupted";

const RUN_COLUMNS: &str = "id, issue_id, issue_identifier, issue_title, attempt, worker_host, \
     workspace_path, status, error, turns, started_at, finished_at, duration_ms, \
     input_tokens, output_tokens, total_tokens";

/// An event as submitted by a caller (before a seq is allocated).
#[derive(Debug, Clone)]
pub(crate) struct NewEvent {
    pub run_id: RunId,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub message: Option<String>,
    pub payload: Option<serde_json::Value>,
}

pub(crate) fn insert_run(conn: &Connection, run: &NewRun, now: DateTime<Utc>) -> Result<RunId> {
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
    let mut stmt = conn.prepare_cached(
        "INSERT INTO runs (issue_id, issue_identifier, issue_title, attempt, worker_host, \
         workspace_path, status, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7)",
    )?;
    stmt.execute(params![
        run.issue_id,
        identifier,
        run.issue_title,
        run.attempt,
        run.worker_host,
        run.workspace_path,
        started_at.timestamp_millis(),
    ])?;
    Ok(RunId(conn.last_insert_rowid()))
}

/// Allocate the next seq and insert the event. Must run inside a transaction.
fn insert_event(conn: &Connection, event: &NewEvent) -> Result<i64> {
    if event.kind.trim().is_empty() {
        return Err(StoreError::InvalidArgument(
            "event kind must not be empty".to_string(),
        ));
    }
    let seq: Option<i64> = conn
        .prepare_cached(
            "UPDATE runs SET last_event_seq = last_event_seq + 1 WHERE id = ?1 \
             RETURNING last_event_seq",
        )?
        .query_row([event.run_id.0], |row| row.get(0))
        .optional()?;
    let seq = seq.ok_or(StoreError::RunNotFound(event.run_id))?;
    let message = event.message.as_deref().map(truncate_message);
    let payload = event.payload.as_ref().map(encode_payload).transpose()?;
    conn.prepare_cached(
        "INSERT INTO run_events (run_id, seq, at, kind, message, payload) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        event.run_id.0,
        seq,
        event.at.timestamp_millis(),
        event.kind,
        message,
        payload,
    ])?;
    Ok(seq)
}

pub(crate) fn append_event(conn: &mut Connection, event: &NewEvent) -> Result<i64> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let seq = insert_event(&tx, event)?;
    tx.commit()?;
    Ok(seq)
}

pub(crate) fn update_tokens(conn: &Connection, run_id: RunId, tokens: TokenUsage) -> Result<()> {
    let changed = conn
        .prepare_cached(
            "UPDATE runs SET input_tokens = ?2, output_tokens = ?3, total_tokens = ?4 \
             WHERE id = ?1",
        )?
        .execute(params![
            run_id.0,
            to_sql_u64(tokens.input),
            to_sql_u64(tokens.output),
            to_sql_u64(tokens.total),
        ])?;
    if changed == 0 {
        return Err(StoreError::RunNotFound(run_id));
    }
    Ok(())
}

pub(crate) fn increment_turns(conn: &Connection, run_id: RunId) -> Result<u32> {
    let turns: Option<i64> = conn
        .prepare_cached("UPDATE runs SET turns = turns + 1 WHERE id = ?1 RETURNING turns")?
        .query_row([run_id.0], |row| row.get(0))
        .optional()?;
    turns.map(clamp_u32).ok_or(StoreError::RunNotFound(run_id))
}

pub(crate) fn update_runtime_info(
    conn: &Connection,
    run_id: RunId,
    worker_host: Option<&str>,
    workspace_path: Option<&str>,
) -> Result<()> {
    let changed = conn
        .prepare_cached(
            "UPDATE runs SET worker_host = COALESCE(?2, worker_host), \
             workspace_path = COALESCE(?3, workspace_path) WHERE id = ?1",
        )?
        .execute(params![run_id.0, worker_host, workspace_path])?;
    if changed == 0 {
        return Err(StoreError::RunNotFound(run_id));
    }
    Ok(())
}

pub(crate) fn finish_run(
    conn: &Connection,
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
    let now_ms = now.timestamp_millis();
    let sql = format!(
        "UPDATE runs SET status = ?2, error = ?3, finished_at = ?4, \
         duration_ms = MAX(0, ?4 - started_at) WHERE id = ?1 AND status = 'running' \
         RETURNING {RUN_COLUMNS}"
    );
    let finished = conn
        .prepare_cached(&sql)?
        .query_row(
            params![run_id.0, status.as_str(), error, now_ms],
            run_from_row,
        )
        .optional()?;
    match finished {
        Some(record) => Ok(record),
        None if get_run(conn, run_id)?.is_some() => Err(StoreError::RunAlreadyFinished(run_id)),
        None => Err(StoreError::RunNotFound(run_id)),
    }
}

pub(crate) fn get_run(conn: &Connection, run_id: RunId) -> Result<Option<RunRecord>> {
    let sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1");
    Ok(conn
        .prepare_cached(&sql)?
        .query_row([run_id.0], run_from_row)
        .optional()?)
}

pub(crate) fn list_runs(conn: &Connection, query: &RunQuery) -> Result<RunPage> {
    let limit = query.effective_limit();
    let mut clauses: Vec<&str> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    if let Some(before) = query.before_id {
        clauses.push("id < ?");
        values.push(Value::Integer(before));
    }
    if let Some(identifier) = &query.issue_identifier {
        clauses.push("issue_identifier = ?");
        values.push(Value::Text(identifier.clone()));
    }
    if let Some(status) = query.status {
        clauses.push("status = ?");
        values.push(Value::Text(status.as_str().to_string()));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    // Fetch one extra row to know whether another page exists.
    values.push(Value::Integer(i64::from(limit) + 1));
    let sql = format!("SELECT {RUN_COLUMNS} FROM runs {where_sql} ORDER BY id DESC LIMIT ?");
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut runs = stmt
        .query_map(params_from_iter(values), run_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
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
    conn: &Connection,
    run_id: RunId,
    after_seq: Option<i64>,
    limit: Option<u32>,
) -> Result<Vec<RunEvent>> {
    let limit = effective_event_limit(limit);
    let mut stmt = conn.prepare_cached(
        "SELECT run_id, seq, at, kind, message, payload FROM run_events \
         WHERE run_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
    )?;
    let events = stmt
        .query_map(
            params![run_id.0, after_seq.unwrap_or(0), i64::from(limit)],
            event_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(events)
}

pub(crate) fn totals(conn: &Connection) -> Result<TotalsRecord> {
    let record = conn
        .prepare_cached(
            "SELECT COUNT(*), \
             COALESCE(SUM(status = 'succeeded'), 0), \
             COALESCE(SUM(status = 'failed'), 0), \
             COALESCE(SUM(input_tokens), 0), \
             COALESCE(SUM(output_tokens), 0), \
             COALESCE(SUM(total_tokens), 0), \
             COALESCE(SUM(MAX(duration_ms, 0)), 0) \
             FROM runs",
        )?
        .query_row([], |row| {
            Ok(TotalsRecord {
                runs_total: clamp_u64(row.get(0)?),
                runs_succeeded: clamp_u64(row.get(1)?),
                runs_failed: clamp_u64(row.get(2)?),
                tokens: TokenUsage {
                    input: clamp_u64(row.get(3)?),
                    output: clamp_u64(row.get(4)?),
                    total: clamp_u64(row.get(5)?),
                },
                runtime_ms: clamp_u64(row.get(6)?),
            })
        })?;
    Ok(record)
}

pub(crate) fn mark_interrupted_runs(conn: &mut Connection, now: DateTime<Utc>) -> Result<u64> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ids = tx
        .prepare_cached(
            "UPDATE runs SET status = 'cancelled', error = ?1, finished_at = ?2, \
             duration_ms = MAX(0, ?2 - started_at) WHERE status = 'running' RETURNING id",
        )?
        .query_map(params![INTERRUPTED_ERROR, now.timestamp_millis()], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in &ids {
        insert_event(
            &tx,
            &NewEvent {
                run_id: RunId(*id),
                at: now,
                kind: INTERRUPTED_EVENT_KIND.to_string(),
                message: Some(INTERRUPTED_ERROR.to_string()),
                payload: None,
            },
        )?;
    }
    tx.commit()?;
    Ok(ids.len() as u64)
}

pub(crate) fn prune(
    conn: &mut Connection,
    cutoff: DateTime<Utc>,
    keep_min_runs: u32,
) -> Result<PruneStats> {
    const DOOMED: &str = "SELECT id FROM runs WHERE status != 'running' AND started_at < ?1 \
         AND id NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT ?2)";
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let args = params![cutoff.timestamp_millis(), i64::from(keep_min_runs)];
    // Delete events explicitly (not only via ON DELETE CASCADE) so the count is exact.
    let events_deleted = tx.execute(
        &format!("DELETE FROM run_events WHERE run_id IN ({DOOMED})"),
        args,
    )?;
    let runs_deleted = tx.execute(&format!("DELETE FROM runs WHERE id IN ({DOOMED})"), args)?;
    tx.commit()?;
    Ok(PruneStats {
        runs_deleted: runs_deleted as u64,
        events_deleted: events_deleted as u64,
    })
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<RunRecord> {
    let status_text: String = row.get(7)?;
    let status = status_text
        .parse::<RunStatus>()
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(7, Type::Text, Box::new(err)))?;
    Ok(RunRecord {
        id: RunId(row.get(0)?),
        issue_id: row.get(1)?,
        issue_identifier: row.get(2)?,
        issue_title: row.get(3)?,
        attempt: clamp_u32(row.get(4)?),
        worker_host: row.get(5)?,
        workspace_path: row.get(6)?,
        status,
        error: row.get(8)?,
        turns: clamp_u32(row.get(9)?),
        started_at: timestamp(10, row.get(10)?)?,
        finished_at: row
            .get::<_, Option<i64>>(11)?
            .map(|ms| timestamp(11, ms))
            .transpose()?,
        duration_ms: row.get(12)?,
        tokens: TokenUsage {
            input: clamp_u64(row.get(13)?),
            output: clamp_u64(row.get(14)?),
            total: clamp_u64(row.get(15)?),
        },
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<RunEvent> {
    let payload = row
        .get::<_, Option<String>>(5)?
        .map(|text| {
            serde_json::from_str(&text).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(5, Type::Text, Box::new(err))
            })
        })
        .transpose()?;
    Ok(RunEvent {
        run_id: RunId(row.get(0)?),
        seq: row.get(1)?,
        at: timestamp(2, row.get(2)?)?,
        kind: row.get(3)?,
        message: row.get(4)?,
        payload,
    })
}

fn timestamp(column: usize, ms: i64) -> rusqlite::Result<DateTime<Utc>> {
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

fn clamp_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn clamp_u32(value: i64) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(u32::MAX)
}

fn to_sql_u64(value: u64) -> i64 {
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
