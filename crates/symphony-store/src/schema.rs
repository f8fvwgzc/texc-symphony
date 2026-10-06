//! Connection pragmas and `PRAGMA user_version` based schema migrations.

use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

use crate::error::{Result, StoreError};

/// How long a statement waits for a lock held by another connection/process.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Embedded migrations; entry `i` upgrades the schema from version `i` to `i + 1`.
/// Append new files, never edit shipped ones.
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_init.sql"),
    include_str!("migrations/0002_retry_queue.sql"),
];

/// Schema version this build creates and understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Apply the per-connection settings: busy timeout, foreign keys, WAL (file databases only)
/// and `synchronous=NORMAL` (safe with WAL; fsync happens at checkpoints).
pub(crate) fn configure(conn: &Connection, file_backed: bool) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    if file_backed {
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(journal_mode = %mode, "sqlite refused WAL journal mode");
        }
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// Read `PRAGMA user_version`.
pub(crate) fn user_version(conn: &Connection) -> Result<u32> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    u32::try_from(version)
        .map_err(|_| StoreError::CorruptRow(format!("invalid user_version {version}")))
}

/// Bring the schema up to [`SCHEMA_VERSION`]. Idempotent: running it on an up-to-date database
/// does nothing. Each migration runs in its own `IMMEDIATE` transaction together with the
/// `user_version` bump, so concurrent openers never apply a migration twice.
///
/// Returns the number of migrations applied.
pub(crate) fn migrate(conn: &mut Connection) -> Result<u32> {
    let mut applied = 0;
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = user_version(&tx)?;
        if current > SCHEMA_VERSION {
            return Err(StoreError::SchemaTooNew {
                found: current,
                supported: SCHEMA_VERSION,
            });
        }
        let Some(sql) = MIGRATIONS.get(current as usize) else {
            return Ok(applied);
        };
        let next = current + 1;
        let migration_err = |err: rusqlite::Error| StoreError::Migration {
            version: next,
            reason: err.to_string(),
        };
        tx.execute_batch(sql).map_err(migration_err)?;
        // PRAGMA arguments cannot be bound; `next` is an integer we control.
        tx.execute_batch(&format!("PRAGMA user_version = {next}"))
            .map_err(migration_err)?;
        tx.commit().map_err(migration_err)?;
        tracing::info!(version = next, "applied symphony-store migration");
        applied += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent_on_one_connection() {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn, false).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 0);
        assert_eq!(migrate(&mut conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(migrate(&mut conn).unwrap(), 0);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        // Every migration is also safe to re-run by itself.
        for sql in MIGRATIONS {
            conn.execute_batch(sql).unwrap();
        }
    }

    #[test]
    fn refuses_newer_schema() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .unwrap();
        assert_eq!(
            migrate(&mut conn),
            Err(StoreError::SchemaTooNew {
                found: SCHEMA_VERSION + 1,
                supported: SCHEMA_VERSION
            })
        );
    }

    #[test]
    fn foreign_keys_are_enabled() {
        let conn = Connection::open_in_memory().unwrap();
        configure(&conn, false).unwrap();
        let on: i64 = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }
}
