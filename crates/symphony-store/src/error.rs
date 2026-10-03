//! Typed store errors.

use crate::model::RunId;

/// Convenience alias for results returned by the store.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Every failure the store can report.
///
/// Errors are `Clone` (SQLite errors are captured as text) so they can travel over channels and be
/// logged by fire-and-forget callers. Display strings start with a snake_case reason tag.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// The database file could not be opened or created.
    #[error("store_open_failed: {path}: {reason}")]
    Open {
        /// Path that was being opened.
        path: String,
        /// Underlying cause.
        reason: String,
    },
    /// Any SQLite error raised while executing a statement.
    #[error("store_sqlite_error: {0}")]
    Sqlite(String),
    /// The on-disk schema was written by a newer Symphony; refusing to downgrade it.
    #[error(
        "store_schema_too_new: database schema version {found} is newer than supported version {supported}"
    )]
    SchemaTooNew {
        /// `PRAGMA user_version` found in the file.
        found: u32,
        /// Highest version this build knows.
        supported: u32,
    },
    /// Applying a schema migration failed (the migration was rolled back).
    #[error("store_migration_failed: version {version}: {reason}")]
    Migration {
        /// Version that failed to apply.
        version: u32,
        /// Underlying cause.
        reason: String,
    },
    /// The background database thread is gone (it was never started or it exited).
    #[error("store_unavailable: persistence thread is not running")]
    Unavailable,
    /// An operation panicked inside the database thread; the thread survived.
    #[error("store_internal_error: {0}")]
    Internal(String),
    /// No run with this id exists.
    #[error("run_not_found: {0}")]
    RunNotFound(RunId),
    /// `finish_run` was called on a run that is no longer `running`.
    #[error("run_already_finished: {0}")]
    RunAlreadyFinished(RunId),
    /// The caller passed an invalid argument.
    #[error("invalid_argument: {0}")]
    InvalidArgument(String),
    /// A stored row could not be decoded.
    #[error("store_corrupt_row: {0}")]
    CorruptRow(String),
    /// A store configuration value (env var) is invalid.
    #[error("store_config_invalid: {0}")]
    InvalidConfig(String),
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        match err {
            rusqlite::Error::FromSqlConversionFailure(column, _, cause) => {
                StoreError::CorruptRow(format!("column {column}: {cause}"))
            }
            other => StoreError::Sqlite(other.to_string()),
        }
    }
}
