//! symphony-store — SQLite persistence for Symphony agent runs (new in the Rust port; the Elixir
//! implementation kept everything in memory).
//!
//! * [`Store`] — cloneable handle; operations are queued to a dedicated database thread and
//!   return [`Pending`] futures (await for the result, drop for fire-and-forget writes).
//! * Records — [`RunRecord`], [`RunEvent`], [`TotalsRecord`], [`TokenUsage`]; their serde field
//!   names are the JSON contract of the HTTP API and web UI.
//! * [`StoreConfig`] — `SYMPHONY_DB_PATH` / `SYMPHONY_DB_RETENTION_DAYS`.
//!
//! SQLite is bundled (`rusqlite/bundled`), opened in WAL mode with `busy_timeout` and
//! `foreign_keys=ON`; the schema is migrated through `PRAGMA user_version`.
//!
//! ```no_run
//! # async fn demo() -> symphony_store::Result<()> {
//! use symphony_store::{NewRun, RunStatus, Store, TokenUsage};
//!
//! let store = Store::open("./data/symphony.db")?;
//! store.mark_interrupted_runs().await?;
//! let run = store.start_run(NewRun::new("issue-1", "MT-1")).await?;
//! store.append_event(run, "session_started", None, None).detach();
//! store.update_tokens(run, TokenUsage { input: 4, output: 8, total: 12 }).detach();
//! store.finish_run(run, RunStatus::Succeeded, None).await?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod config;
mod error;
mod model;
mod ops;
mod schema;
mod store;

pub use config::{
    DEFAULT_DB_PATH, DEFAULT_KEEP_MIN_RUNS, DEFAULT_PRUNE_INTERVAL, DEFAULT_RETENTION_DAYS,
    ENV_DB_PATH, ENV_RETENTION_DAYS, StoreConfig, StoreLocation,
};
pub use error::{Result, StoreError};
pub use model::{
    DEFAULT_EVENT_LIMIT, DEFAULT_RUN_LIMIT, MAX_EVENT_LIMIT, MAX_MESSAGE_BYTES, MAX_PAYLOAD_BYTES,
    MAX_RUN_LIMIT, NewRun, PruneStats, RetryRecord, RunEvent, RunId, RunPage, RunQuery, RunRecord,
    RunStatus, TokenUsage, TotalsRecord,
};
pub use ops::{INTERRUPTED_ERROR, INTERRUPTED_EVENT_KIND};
pub use schema::{BUSY_TIMEOUT, SCHEMA_VERSION};
pub use store::{Pending, Store};
