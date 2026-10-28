//! symphony-store — SQLite or PostgreSQL persistence for Symphony agent runs, tasks and dispatch
//! leases (new in the Rust port; the Elixir implementation kept everything in memory).
//!
//! * [`Store`] — cloneable handle; operations are queued to a dedicated database thread and
//!   return [`Pending`] futures (await for the result, drop for fire-and-forget writes).
//! * Records — [`RunRecord`], [`RunEvent`], [`TotalsRecord`], [`TokenUsage`]; their serde field
//!   names are the JSON contract of the HTTP API and web UI.
//! * [`StoreConfig`] — `SYMPHONY_DATABASE_URL` / `SYMPHONY_DB_PATH` / `SYMPHONY_DB_RETENTION_DAYS`.
//!
//! SQLite is bundled (`rusqlite/bundled`), opened in WAL mode with `busy_timeout` and
//! `foreign_keys=ON`; the schema is migrated through `PRAGMA user_version`. PostgreSQL
//! ([`Store::open_postgres`]) gets its own schema, migrated under an advisory lock; several
//! Symphony replicas can share it, coordinated by dispatch leases. Every operation is written
//! once, in a dialect both databases run (`db`).
//!
//! ```no_run
//! # async fn demo() -> symphony_store::Result<()> {
//! use symphony_store::{NewRun, RunStatus, Store, TokenUsage};
//!
//! let store = Store::open("./data/symphony.db")?;
//! store.mark_interrupted_runs().await?;
//! let run = store.start_run(NewRun::new("issue-1", "MT-1")).await?;
//! store.append_event(run, "session_started", None, None).detach();
//! store.update_tokens(run, TokenUsage { input: 4, cached: 0, output: 8, total: 12 }).detach();
//! store.finish_run(run, RunStatus::Succeeded, None).await?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod config;
mod db;
mod error;
mod leases;
mod model;
mod ops;
mod schema;
mod store;
mod tasks;

pub use config::{
    DEFAULT_DB_PATH, DEFAULT_KEEP_MIN_RUNS, DEFAULT_PAYLOAD_RETENTION_DAYS, DEFAULT_PRUNE_INTERVAL,
    DEFAULT_RETENTION_DAYS, ENV_DATABASE_URL, ENV_DB_PATH, ENV_PAYLOAD_RETENTION_DAYS,
    ENV_RETENTION_DAYS, StoreConfig, StoreLocation,
};
pub use error::{Result, StoreError};
pub use model::{
    DEFAULT_EVENT_LIMIT, DEFAULT_RUN_LIMIT, MAX_EVENT_LIMIT, MAX_MESSAGE_BYTES, MAX_PAYLOAD_BYTES,
    MAX_RUN_LIMIT, NewRun, PruneStats, RetryRecord, RunEvent, RunId, RunPage, RunQuery, RunRecord,
    RunStatus, TokenUsage, TotalsRecord, UsageTotals,
};
pub use ops::{INTERRUPTED_ERROR, INTERRUPTED_EVENT_KIND};
pub use schema::{BUSY_TIMEOUT, SCHEMA_VERSION};
pub use store::{Pending, STORE_QUEUE_CAPACITY, Store};
pub use tasks::{
    DEFAULT_TASK_LIMIT, MAX_TASK_LIMIT, MAX_TASK_PROMPT_BYTES, MAX_TASK_RESULT_BYTES, NewTask,
    TaskLimits, TaskPage, TaskQuery, TaskRecord, TaskStatus, task_identifier,
};
