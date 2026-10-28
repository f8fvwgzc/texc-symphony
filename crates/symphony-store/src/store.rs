//! The `Store` handle: a cheap, cloneable front for one database connection (SQLite file or
//! memory, or PostgreSQL) owned by a dedicated database thread.

// Modified 2026-10-25: bounded job queue, usage/control/payload-retention operations (audit A1/A4);
// PostgreSQL connections and dispatch leases (Part C).

use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use tokio::sync::{mpsc, oneshot};

use crate::db::{Db, PostgresDb, SqliteDb};
use crate::leases;

use crate::error::{Result, StoreError};
use crate::model::{
    NewRun, PruneStats, RetryRecord, RunEvent, RunId, RunPage, RunQuery, RunRecord, RunStatus,
    TokenUsage, TotalsRecord, UsageTotals,
};
use crate::ops::{self, NewEvent};
use crate::schema;
use crate::tasks::{self, NewTask, TaskPage, TaskQuery, TaskRecord, TaskStatus};

type Job = Box<dyn FnOnce(&mut dyn Db) + Send + 'static>;

/// Operations queued for the database thread at most; beyond this, submissions fail with
/// [`StoreError::Busy`] instead of growing memory without bound.
pub const STORE_QUEUE_CAPACITY: usize = 8192;

/// Handle to the run history database.
///
/// `Store` is `Clone + Send + Sync`; clones share one connection that lives on a dedicated
/// `symphony-store` thread. Every operation is queued to that thread in submission order and
/// returns a [`Pending`] future:
///
/// * **await it** to get the result (`store.start_run(run).await?`);
/// * **drop it** for fire-and-forget writes — the write still happens and a failure is logged
///   with `tracing::warn!`. Submitting never blocks the caller; when [`STORE_QUEUE_CAPACITY`]
///   operations are already queued the operation is refused with [`StoreError::Busy`] (logged).
///
/// Errors are always returned, never raised as panics. A [`Store::disabled`] store accepts every
/// call and persists nothing. The database thread exits once every clone has been dropped and the
/// queue is drained; call [`Store::flush`] before shutdown to wait for queued writes.
#[derive(Clone)]
pub struct Store {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    jobs: mpsc::Sender<Job>,
    path: Option<PathBuf>,
    /// What `Debug` shows: the file, `:memory:` or the PostgreSQL URL without its password.
    label: String,
    /// `true` for PostgreSQL: other Symphony replicas may use the same database.
    shared: bool,
    /// `false` while the (PostgreSQL) connection is lost; always `true` for SQLite.
    healthy: Arc<AtomicBool>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            None => f.write_str("Store(disabled)"),
            Some(inner) => write!(f, "Store({})", inner.label),
        }
    }
}

impl Store {
    /// Open (creating if needed) the database at `path`, creating parent directories, enabling
    /// WAL and applying pending migrations. Blocks briefly; call it during startup.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let path = path.as_ref().to_path_buf();
        let open_err = |reason: String| StoreError::Open {
            path: path.display().to_string(),
            reason,
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|err| open_err(err.to_string()))?;
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut conn =
            Connection::open_with_flags(&path, flags).map_err(|err| open_err(err.to_string()))?;
        schema::configure(&conn, true)?;
        schema::migrate(&mut conn)?;
        // One process per SQLite file: leases of an earlier process are void.
        conn.execute("DELETE FROM leases", [])?;
        let label = path.display().to_string();
        Store::spawn(label, Some(path), false, always_healthy(), move || {
            Ok(Box::new(SqliteDb(conn)))
        })
    }

    /// Open a private in-memory database (tests, `SYMPHONY_DB_PATH=:memory:`).
    pub fn open_in_memory() -> Result<Store> {
        let mut conn = Connection::open_in_memory()?;
        schema::configure(&conn, false)?;
        schema::migrate(&mut conn)?;
        Store::spawn(
            ":memory:".into(),
            None,
            false,
            always_healthy(),
            move || Ok(Box::new(SqliteDb(conn))),
        )
    }

    /// A private in-memory database that reports [`Store::is_shared`] as `true`, so tests can
    /// exercise the multi-replica dispatch path (lease renewal, kill-switch sync, orphan
    /// recovery, spend enforcement) without a live PostgreSQL. SQLite cannot actually be shared
    /// between processes; this only forces the in-process coordination that a shared store drives.
    ///
    /// Behind the `test-util` feature: the production binary never enables it, so a real build can
    /// never construct a SQLite store that reports [`Store::is_postgres`].
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn open_in_memory_shared() -> Result<Store> {
        let mut conn = Connection::open_in_memory()?;
        schema::configure(&conn, false)?;
        schema::migrate(&mut conn)?;
        Store::spawn(":memory:".into(), None, true, always_healthy(), move || {
            Ok(Box::new(SqliteDb(conn)))
        })
    }

    /// Connect to the PostgreSQL database at `url` (`postgres://user:pass@host:port/db`, libpq
    /// keywords such as `sslmode=require` included) and apply pending migrations. Several
    /// Symphony replicas may share one database: migrations are serialized and dispatch is
    /// coordinated by leases. TLS uses the system roots (`sslmode=disable` turns it off).
    pub fn open_postgres(url: &str) -> Result<Store> {
        let label = redact_url(url);
        let config = postgres_config(url).map_err(|reason| StoreError::Open {
            path: label.clone(),
            reason,
        })?;
        let open_label = label.clone();
        let healthy = Arc::new(AtomicBool::new(true));
        let db_healthy = Arc::clone(&healthy);
        let reconnect_label = label.clone();
        Store::spawn(label, None, true, healthy, move || {
            // One throttled attempt per reconnect, so a backlog of jobs during an outage does not
            // each sleep through a backoff; the backoff below is only for the first connect.
            let reconnect_config = config.clone();
            let reconnect = move || {
                connect_postgres(&reconnect_config).map_err(|reason| StoreError::Open {
                    path: reconnect_label.clone(),
                    reason,
                })
            };
            let mut client =
                connect_postgres_with_backoff(&config).map_err(|reason| StoreError::Open {
                    path: open_label.clone(),
                    reason,
                })?;
            schema::migrate_postgres(&mut client)?;
            Ok(Box::new(PostgresDb::new(
                client,
                Box::new(reconnect),
                db_healthy,
            )))
        })
    }

    /// A no-op store used when persistence is turned off: writes succeed without storing
    /// anything (`start_run` returns [`RunId::DISABLED`]) and reads return empty results.
    pub fn disabled() -> Store {
        Store { inner: None }
    }

    /// `false` for [`Store::disabled`].
    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// `true` for a PostgreSQL store (replicas may share it); `false` for SQLite and disabled.
    pub fn is_postgres(&self) -> bool {
        self.inner.as_ref().is_some_and(|inner| inner.shared)
    }

    /// `true` when the store's connection is currently lost (PostgreSQL only); health probes
    /// report the store as degraded. Always `false` for SQLite and disabled stores.
    pub fn is_degraded(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| !inner.healthy.load(Ordering::Relaxed))
    }

    /// `true` when other Symphony replicas may share the database (PostgreSQL): dispatch is then
    /// coordinated by leases and dead replicas' work is recovered periodically.
    pub fn is_shared(&self) -> bool {
        self.inner.as_ref().is_some_and(|inner| inner.shared)
    }

    /// Database file path; `None` for in-memory and disabled stores.
    pub fn path(&self) -> Option<&Path> {
        self.inner.as_ref().and_then(|inner| inner.path.as_deref())
    }

    /// Starts the database thread; `connect` runs on it (a PostgreSQL client must live on the
    /// thread that created it) and its error is returned here.
    fn spawn(
        label: String,
        path: Option<PathBuf>,
        shared: bool,
        healthy: Arc<AtomicBool>,
        connect: impl FnOnce() -> Result<Box<dyn Db>> + Send + 'static,
    ) -> Result<Store> {
        let (jobs, mut queue) = mpsc::channel::<Job>(STORE_QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        std::thread::Builder::new()
            .name("symphony-store".to_string())
            .spawn(move || {
                let mut db = match connect() {
                    Ok(db) => {
                        let _ = ready_tx.send(Ok(()));
                        db
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                        return;
                    }
                };
                while let Some(job) = queue.blocking_recv() {
                    job(db.as_mut());
                }
            })
            .map_err(|err| StoreError::Open {
                path: label.clone(),
                reason: format!("cannot start database thread: {err}"),
            })?;
        ready_rx.recv().map_err(|_| StoreError::Open {
            path: label.clone(),
            reason: "the database thread stopped while connecting".into(),
        })??;
        Ok(Store {
            inner: Some(Arc::new(Inner {
                jobs,
                path,
                label,
                shared,
                healthy,
            })),
        })
    }

    /// Queue `op` on the database thread. `disabled` produces the result of a disabled store.
    fn submit<T, F>(&self, name: &'static str, disabled: impl FnOnce() -> T, op: F) -> Pending<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut dyn Db) -> Result<T> + Send + 'static,
    {
        let Some(inner) = &self.inner else {
            return Pending::ready(Ok(disabled()));
        };
        let (reply, receiver) = oneshot::channel();
        let job: Job = Box::new(move |conn| {
            let result = catch_unwind(AssertUnwindSafe(|| op(conn)))
                .unwrap_or_else(|_| Err(StoreError::Internal(format!("{name} panicked"))));
            // The caller dropped the Pending (fire-and-forget): surface failures in the log.
            if let Err(Err(err)) = reply.send(result) {
                tracing::warn!(op = name, error = %err, "symphony-store operation failed");
            }
        });
        match inner.jobs.try_send(job) {
            Ok(()) => Pending::waiting(receiver),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    op = name,
                    "symphony-store queue is full; dropping the operation"
                );
                Pending::ready(Err(StoreError::Busy))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(op = name, "symphony-store thread is not running");
                Pending::ready(Err(StoreError::Unavailable))
            }
        }
    }

    /// Record a new `running` run and return its id.
    pub fn start_run(&self, run: NewRun) -> Pending<RunId> {
        let now = Utc::now();
        self.submit(
            "start_run",
            || RunId::DISABLED,
            move |conn| ops::insert_run(conn, &run, now),
        )
    }

    /// Append an event to a run's log and return its seq (1, 2, 3, … per run, in submission
    /// order). The event timestamp is taken at call time. Oversized messages/payloads are
    /// truncated (see [`crate::MAX_MESSAGE_BYTES`], [`crate::MAX_PAYLOAD_BYTES`]).
    pub fn append_event(
        &self,
        run_id: RunId,
        kind: impl Into<String>,
        message: Option<String>,
        payload: Option<serde_json::Value>,
    ) -> Pending<i64> {
        let event = NewEvent {
            run_id,
            at: Utc::now(),
            kind: kind.into(),
            message,
            payload,
        };
        self.submit(
            "append_event",
            || 0,
            move |conn| ops::append_event(conn, &event),
        )
    }

    /// Replace the run's cumulative token usage (the orchestrator's per-run counters).
    pub fn update_tokens(&self, run_id: RunId, tokens: TokenUsage) -> Pending<()> {
        self.submit(
            "update_tokens",
            || (),
            move |conn| ops::update_tokens(conn, run_id, tokens),
        )
    }

    /// Replace the run's cumulative token usage and its cost in micro-USD.
    pub fn update_usage(
        &self,
        run_id: RunId,
        tokens: TokenUsage,
        cost_micro_usd: u64,
    ) -> Pending<()> {
        self.submit(
            "update_usage",
            || (),
            move |conn| ops::update_usage(conn, run_id, tokens, cost_micro_usd),
        )
    }

    /// Token and cost totals of the runs that started at or after `since` (budget accounting).
    pub fn usage_since(&self, since: DateTime<Utc>) -> Pending<UsageTotals> {
        self.submit("usage_since", UsageTotals::default, move |conn| {
            ops::usage_since(conn, since)
        })
    }

    /// Token and cost totals of every retained run of `issue_id`.
    pub fn issue_usage(&self, issue_id: impl Into<String>) -> Pending<UsageTotals> {
        let issue_id = issue_id.into();
        self.submit("issue_usage", UsageTotals::default, move |conn| {
            ops::issue_usage(conn, &issue_id)
        })
    }

    /// An operator control value (e.g. the dispatch kill switch); `None` when unset or disabled.
    pub fn control(&self, key: impl Into<String>) -> Pending<Option<String>> {
        let key = key.into();
        self.submit("control", || None, move |conn| ops::get_control(conn, &key))
    }

    /// Sets (`Some`) or clears (`None`) an operator control value.
    pub fn set_control(&self, key: impl Into<String>, value: Option<String>) -> Pending<()> {
        let key = key.into();
        self.submit(
            "set_control",
            || (),
            move |conn| ops::set_control(conn, &key, value.as_deref()),
        )
    }

    /// Payload retention: clears the payloads (raw agent messages and command output) of events
    /// recorded more than `older_than` ago, keeping their kinds and messages. Returns the number
    /// of events changed.
    pub fn prune_payloads(&self, older_than: Duration) -> Pending<u64> {
        let cutoff = cutoff(Utc::now(), older_than);
        self.submit(
            "prune_payloads",
            || 0,
            move |conn| ops::prune_payloads(conn, cutoff),
        )
    }

    /// Queue a new task.
    /// A disabled store cannot hold tasks: [`StoreError::Unavailable`].
    pub fn create_task(&self, task: NewTask) -> Pending<TaskRecord> {
        if !self.is_enabled() {
            return Pending::ready(Err(StoreError::Unavailable));
        }
        let now = Utc::now();
        self.submit(
            "create_task",
            // Impossible: a disabled store returned above.
            || unreachable!("checked above"),
            move |conn| tasks::create_task(conn, &task, now),
        )
    }

    /// One task.
    pub fn get_task(&self, id: i64) -> Pending<Option<TaskRecord>> {
        self.submit("get_task", || None, move |conn| tasks::get_task(conn, id))
    }

    /// Tasks newest first, filtered and paginated by `query`.
    pub fn list_tasks(&self, query: TaskQuery) -> Pending<TaskPage> {
        self.submit("list_tasks", TaskPage::default, move |conn| {
            tasks::list_tasks(conn, &query)
        })
    }

    /// The reported status of each of `ids` that exists (`waiting` included).
    pub fn task_states(&self, ids: Vec<i64>) -> Pending<Vec<(i64, TaskStatus)>> {
        self.submit("task_states", Vec::new, move |conn| {
            tasks::task_states(conn, &ids)
        })
    }

    /// Fails every queued task whose dependency failed or was cancelled, transitively (the
    /// dispatch pass runs it for writes that raced); returns how many.
    pub fn settle_task_dependencies(&self) -> Pending<u64> {
        let now = Utc::now();
        self.submit(
            "settle_task_dependencies",
            || 0,
            move |conn| tasks::settle_dependants(conn, now),
        )
    }

    /// Up to `limit` queued tasks whose dependencies have succeeded, oldest first.
    pub fn queued_tasks(&self, limit: u32) -> Pending<Vec<TaskRecord>> {
        self.submit("queued_tasks", Vec::new, move |conn| {
            tasks::queued_tasks(conn, limit)
        })
    }

    /// Claims a queued task for a run (`queued` → `running`); `None` when it is not queued.
    pub fn start_task(&self, id: i64) -> Pending<Option<TaskRecord>> {
        let now = Utc::now();
        self.submit(
            "start_task",
            || None,
            move |conn| tasks::start_task(conn, id, now),
        )
    }

    /// Ends a running task's run with `status` (final, or `blocked`); `None` when it was not
    /// running.
    pub fn finish_task(
        &self,
        id: i64,
        status: TaskStatus,
        result: Option<String>,
        error: Option<String>,
    ) -> Pending<Option<TaskRecord>> {
        let now = Utc::now();
        self.submit(
            "finish_task",
            || None,
            move |conn| {
                tasks::finish_task(conn, id, status, result.as_deref(), error.as_deref(), now)
            },
        )
    }

    /// Cancels a queued (or waiting) or blocked task, failing its waiting dependants (others are
    /// returned unchanged); `None` for an unknown id.
    pub fn cancel_task(&self, id: i64, reason: impl Into<String>) -> Pending<Option<TaskRecord>> {
        let now = Utc::now();
        let reason = reason.into();
        self.submit(
            "cancel_task",
            || None,
            move |conn| tasks::cancel_task(conn, id, &reason, now),
        )
    }

    /// Re-queues a blocked or failed task with an optional operator reply appended to its prompt
    /// ([`StoreError::Conflict`] while one of its dependencies is failed or cancelled).
    pub fn requeue_task(&self, id: i64, message: Option<String>) -> Pending<Option<TaskRecord>> {
        let now = Utc::now();
        self.submit(
            "requeue_task",
            || None,
            move |conn| tasks::requeue_task(conn, id, message.as_deref(), now),
        )
    }

    /// A running task whose run was stopped for an outside reason goes back to `queued`.
    pub fn release_task(&self, id: i64, reason: impl Into<String>) -> Pending<Option<TaskRecord>> {
        let now = Utc::now();
        let reason = reason.into();
        self.submit(
            "release_task",
            || None,
            move |conn| tasks::release_task(conn, id, &reason, now),
        )
    }

    /// Takes the dispatch lease of `key` (an issue id, `task:<id>` for tasks) for `owner` until
    /// `ttl` from now. `true` when `owner` holds it afterwards. A disabled store grants every
    /// lease (there is nothing to share).
    pub fn acquire_lease(
        &self,
        key: impl Into<String>,
        owner: impl Into<String>,
        ttl: Duration,
    ) -> Pending<bool> {
        let (key, owner, now) = (key.into(), owner.into(), Utc::now());
        self.submit(
            "acquire_lease",
            || true,
            move |conn| leases::acquire(conn, &key, &owner, now, ttl),
        )
    }

    /// Extends `owner`'s leases on `keys` by `ttl`; returns the keys it still held (all of them
    /// without a database: there is nobody to lose them to).
    pub fn renew_leases(
        &self,
        owner: impl Into<String>,
        keys: Vec<String>,
        ttl: Duration,
    ) -> Pending<Vec<String>> {
        let (owner, now) = (owner.into(), Utc::now());
        let all = keys.clone();
        self.submit(
            "renew_leases",
            move || all,
            move |conn| leases::renew(conn, &owner, &keys, now, ttl),
        )
    }

    /// Gives up `owner`'s lease on `key`.
    pub fn release_lease(&self, key: impl Into<String>, owner: impl Into<String>) -> Pending<()> {
        let (key, owner) = (key.into(), owner.into());
        self.submit(
            "release_lease",
            || (),
            move |conn| leases::release(conn, &key, &owner),
        )
    }

    /// Drops every lease of `owner` (startup of a replica with a stable id).
    pub fn release_owner_leases(&self, owner: impl Into<String>) -> Pending<u64> {
        let owner = owner.into();
        self.submit(
            "release_owner_leases",
            || 0,
            move |conn| leases::release_owner(conn, &owner),
        )
    }

    /// The live holder of the lease on `key`, if any.
    pub fn lease_holder(&self, key: impl Into<String>) -> Pending<Option<String>> {
        let (key, now) = (key.into(), Utc::now());
        self.submit(
            "lease_holder",
            || None,
            move |conn| leases::holder(conn, &key, now),
        )
    }

    /// Recovery: tasks left `running` by a process that is gone (no live lease) go back to
    /// `queued`. Returns how many.
    pub fn requeue_interrupted_tasks(&self) -> Pending<u64> {
        let now = Utc::now();
        self.submit(
            "requeue_interrupted_tasks",
            || 0,
            move |conn| tasks::requeue_interrupted_tasks(conn, now),
        )
    }

    /// Increment the run's turn counter and return the new value.
    pub fn increment_turns(&self, run_id: RunId) -> Pending<u32> {
        self.submit(
            "increment_turns",
            || 0,
            move |conn| ops::increment_turns(conn, run_id),
        )
    }

    /// Set the worker host and/or workspace path once the agent reports them
    /// (`None` leaves a field unchanged).
    pub fn update_runtime_info(
        &self,
        run_id: RunId,
        worker_host: Option<String>,
        workspace_path: Option<String>,
    ) -> Pending<()> {
        self.submit(
            "update_runtime_info",
            || (),
            move |conn| {
                ops::update_runtime_info(
                    conn,
                    run_id,
                    worker_host.as_deref(),
                    workspace_path.as_deref(),
                )
            },
        )
    }

    /// Close a `running` run with a terminal `status`, setting `finished_at` (call time) and
    /// `duration_ms`. Returns the updated record (`None` for a disabled store).
    ///
    /// Errors: [`StoreError::InvalidArgument`] for `RunStatus::Running`,
    /// [`StoreError::RunAlreadyFinished`], [`StoreError::RunNotFound`].
    pub fn finish_run(
        &self,
        run_id: RunId,
        status: RunStatus,
        error: Option<String>,
    ) -> Pending<Option<RunRecord>> {
        let now = Utc::now();
        self.submit(
            "finish_run",
            || None,
            move |conn| ops::finish_run(conn, run_id, status, error.as_deref(), now).map(Some),
        )
    }

    /// Fetch one run.
    pub fn get_run(&self, run_id: RunId) -> Pending<Option<RunRecord>> {
        self.submit("get_run", || None, move |conn| ops::get_run(conn, run_id))
    }

    /// List runs newest first, filtered and paginated by `query`.
    pub fn list_runs(&self, query: RunQuery) -> Pending<RunPage> {
        self.submit("list_runs", RunPage::default, move |conn| {
            ops::list_runs(conn, &query)
        })
    }

    /// List a run's events with `seq > after_seq` in seq order (default limit 200, max 1000).
    pub fn list_events(
        &self,
        run_id: RunId,
        after_seq: Option<i64>,
        limit: Option<u32>,
    ) -> Pending<Vec<RunEvent>> {
        self.submit("list_events", Vec::new, move |conn| {
            ops::list_events(conn, run_id, after_seq, limit)
        })
    }

    /// Aggregate counts, tokens and runtime over all retained runs.
    pub fn totals(&self) -> Pending<TotalsRecord> {
        self.submit("totals", TotalsRecord::default, |conn| ops::totals(conn))
    }

    /// Startup recovery: every run still `running` (the previous process died) becomes
    /// `cancelled` with error `"interrupted by restart"` and gets a `run_interrupted` event.
    /// Returns the number of runs closed.
    pub fn mark_interrupted_runs(&self) -> Pending<u64> {
        let now = Utc::now();
        self.submit(
            "mark_interrupted_runs",
            || 0,
            move |conn| ops::mark_interrupted_runs(conn, now),
        )
    }

    /// Queue (or replace) the retry of `retry.issue_id` so it survives a restart.
    pub fn save_retry(&self, retry: RetryRecord) -> Pending<()> {
        self.submit(
            "save_retry",
            || (),
            move |conn| ops::save_retry(conn, &retry),
        )
    }

    /// Remove the queued retry of `issue_id`, if any.
    pub fn delete_retry(&self, issue_id: impl Into<String>) -> Pending<()> {
        let issue_id = issue_id.into();
        self.submit(
            "delete_retry",
            || (),
            move |conn| ops::delete_retry(conn, &issue_id),
        )
    }

    /// Every queued retry, soonest first (empty for a disabled store).
    pub fn list_retries(&self) -> Pending<Vec<RetryRecord>> {
        self.submit("list_retries", Vec::new, |conn| ops::list_retries(conn))
    }

    /// Retention: delete finished runs (and their events) that started more than `older_than`
    /// ago, always keeping the newest `keep_min_runs` runs and every `running` run.
    pub fn prune(&self, older_than: Duration, keep_min_runs: u32) -> Pending<PruneStats> {
        let cutoff = cutoff(Utc::now(), older_than);
        self.submit("prune", PruneStats::default, move |conn| {
            ops::prune(conn, cutoff, keep_min_runs)
        })
    }

    /// Resolves once every operation submitted before this call has been applied.
    pub fn flush(&self) -> Pending<()> {
        self.submit("flush", || (), |_| Ok(()))
    }
}

/// `now - older_than`, saturating at the earliest representable instant.
fn cutoff(now: DateTime<Utc>, older_than: Duration) -> DateTime<Utc> {
    chrono::Duration::from_std(older_than)
        .ok()
        .and_then(|age| now.checked_sub_signed(age))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

/// Result of a queued store operation. Await it for the outcome, or drop it to let the
/// operation complete in the background (failures are then logged).
pub struct Pending<T> {
    state: PendingState<T>,
}

enum PendingState<T> {
    Ready(Option<Result<T>>),
    Waiting(oneshot::Receiver<Result<T>>),
}

// `Pending` never pins its contents (the value is moved out, the receiver is `Unpin`).
impl<T> Unpin for Pending<T> {}

impl<T> Pending<T> {
    fn ready(result: Result<T>) -> Self {
        Pending {
            state: PendingState::Ready(Some(result)),
        }
    }

    fn waiting(receiver: oneshot::Receiver<Result<T>>) -> Self {
        Pending {
            state: PendingState::Waiting(receiver),
        }
    }

    /// Explicitly fire-and-forget: the operation still runs, failures are logged.
    pub fn detach(self) {}
}

impl<T> fmt::Debug for Pending<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &self.state {
            PendingState::Ready(_) => "ready",
            PendingState::Waiting(_) => "waiting",
        };
        f.debug_struct("Pending").field("state", &state).finish()
    }
}

impl<T> Future for Pending<T> {
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.get_mut().state {
            // Polling again after completion is a caller bug; answer instead of panicking.
            PendingState::Ready(value) => {
                Poll::Ready(value.take().unwrap_or(Err(StoreError::Unavailable)))
            }
            PendingState::Waiting(receiver) => Pin::new(receiver)
                .poll(cx)
                .map(|reply| reply.unwrap_or(Err(StoreError::Unavailable))),
        }
    }
}

/// A health flag for a store that never loses its connection (SQLite, in-memory).
fn always_healthy() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(true))
}

/// Bound on opening a PostgreSQL connection (unless the URL sets `connect_timeout`).
pub const POSTGRES_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long unacknowledged data may stay on a PostgreSQL connection before the kernel drops it
/// (unless the URL sets `tcp_user_timeout`; Linux), with keepalives after this much idleness: a
/// silently partitioned database surfaces as an error, not a hang.
pub const POSTGRES_NETWORK_TIMEOUT: Duration = Duration::from_secs(30);

/// The client configuration for `url`. TLS unless `sslmode=disable`: `prefer` (also the
/// default) becomes `require`, because it would fall back to plaintext when anyone on the path
/// answers the TLS request with a refusal. Connecting and a silent network are bounded.
pub(crate) fn postgres_config(url: &str) -> std::result::Result<postgres::Config, String> {
    use postgres::config::SslMode;
    let mut config: postgres::Config = url
        .parse()
        .map_err(|err: postgres::Error| err.to_string())?;
    if config.get_ssl_mode() == SslMode::Prefer {
        config.ssl_mode(SslMode::Require);
    }
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(POSTGRES_CONNECT_TIMEOUT);
    }
    if config.get_tcp_user_timeout().is_none() {
        config.tcp_user_timeout(POSTGRES_NETWORK_TIMEOUT);
    }
    // The driver's default is two hours of idleness before the first keepalive.
    if config.get_keepalives_idle() > POSTGRES_NETWORK_TIMEOUT {
        config
            .keepalives(true)
            .keepalives_idle(POSTGRES_NETWORK_TIMEOUT);
    }
    Ok(config)
}

/// Retries a lost connection this many times (1 s, 2 s, 4 s) before giving up, so a brief
/// database restart or failover recovers without the whole fleet crash-looping.
const POSTGRES_CONNECT_ATTEMPTS: u32 = 3;

/// A new connection for `config`, with a short backoff, TLS with the system roots unless
/// `sslmode=disable`, and a server-side `statement_timeout`/`lock_timeout` so a query the
/// server itself wedges on returns instead of pinning the store thread.
fn connect_postgres(config: &postgres::Config) -> std::result::Result<postgres::Client, String> {
    let mut client = match config.get_ssl_mode() {
        postgres::config::SslMode::Disable => config.connect(postgres::NoTls),
        _ => config.connect(postgres_tls().map_err(|err| err.to_string())?),
    }
    .map_err(|err| err.to_string())?;
    // A statement that runs or waits for a lock past the network timeout is aborted, so no single
    // query pins the store thread indefinitely.
    let ms = POSTGRES_NETWORK_TIMEOUT.as_millis();
    client
        .batch_execute(&format!(
            "SET statement_timeout = {ms}; SET lock_timeout = {ms}"
        ))
        .map_err(|err| err.to_string())?;
    Ok(client)
}

/// Opens the first connection, retrying with a short backoff so a database that is briefly down
/// at startup does not fail the whole process. Used only for the initial connect; a mid-run
/// reconnect is a single throttled attempt (see [`db::PostgresDb`]).
fn connect_postgres_with_backoff(
    config: &postgres::Config,
) -> std::result::Result<postgres::Client, String> {
    let mut last = String::new();
    for attempt in 0..POSTGRES_CONNECT_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1 << (attempt - 1)));
        }
        match connect_postgres(config) {
            Ok(client) => return Ok(client),
            Err(err) => last = err,
        }
    }
    Err(last)
}

/// `url` without its password, in the userinfo or a `password` parameter (for logs and errors).
pub(crate) fn redact_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut parsed) => {
            if parsed.password().is_some() {
                let _ = parsed.set_password(Some("***"));
            }
            if parsed
                .query_pairs()
                .any(|(key, _)| key.eq_ignore_ascii_case("password"))
            {
                let pairs: Vec<(String, String)> = parsed
                    .query_pairs()
                    .map(|(key, value)| {
                        let value = if key.eq_ignore_ascii_case("password") {
                            "***".to_owned()
                        } else {
                            value.into_owned()
                        };
                        (key.into_owned(), value)
                    })
                    .collect();
                parsed.query_pairs_mut().clear().extend_pairs(pairs);
            }
            parsed.to_string()
        }
        // Key/value connection strings: drop the password field.
        Err(_) => url
            .split_whitespace()
            .map(|part| {
                if part.to_ascii_lowercase().starts_with("password=") {
                    "password=***"
                } else {
                    part
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// TLS for PostgreSQL with the system's root certificates.
fn postgres_tls() -> Result<tokio_postgres_rustls::MakeRustlsConnect> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        // A certificate the platform store holds but rustls cannot parse is skipped.
        let _ = roots.add(cert);
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|err| StoreError::Internal(format!("TLS setup failed: {err}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_never_falls_back_to_plaintext_and_does_not_hang_on_a_silent_network() {
        use postgres::config::SslMode;
        let config = postgres_config("postgres://u@db.internal/symphony").unwrap();
        assert_eq!(
            config.get_ssl_mode(),
            SslMode::Require,
            "the default `prefer`"
        );
        let config = postgres_config("postgres://u@db.internal/symphony?sslmode=prefer").unwrap();
        assert_eq!(config.get_ssl_mode(), SslMode::Require);
        let config = postgres_config("postgres://u@db.internal/symphony?sslmode=disable").unwrap();
        assert_eq!(
            config.get_ssl_mode(),
            SslMode::Disable,
            "an explicit opt-out"
        );
        assert_eq!(
            config.get_connect_timeout(),
            Some(&POSTGRES_CONNECT_TIMEOUT)
        );
        assert_eq!(
            config.get_tcp_user_timeout(),
            Some(&POSTGRES_NETWORK_TIMEOUT)
        );
        assert_eq!(config.get_keepalives_idle(), POSTGRES_NETWORK_TIMEOUT);
        let config = postgres_config(
            "postgres://u@db.internal/symphony?connect_timeout=3&tcp_user_timeout=5",
        )
        .unwrap();
        assert_eq!(config.get_connect_timeout(), Some(&Duration::from_secs(3)));
        assert_eq!(config.get_tcp_user_timeout(), Some(&Duration::from_secs(5)));
        assert!(postgres_config("postgres://u@db/x?sslmode=sometimes").is_err());
    }

    #[test]
    fn urls_are_logged_without_any_password() {
        assert_eq!(
            redact_url("postgres://symphony:hunter2@db:5432/s?sslmode=require"),
            "postgres://symphony:***@db:5432/s?sslmode=require"
        );
        let redacted = redact_url("postgres://symphony@db/s?password=hunter2&sslmode=require");
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(redacted.contains("sslmode=require"), "{redacted}");
        assert_eq!(
            redact_url("host=db user=symphony password=hunter2"),
            "host=db user=symphony password=***"
        );
    }

    #[test]
    fn handle_is_clone_send_sync() {
        fn assert_traits<T: Clone + Send + Sync + 'static>() {}
        fn assert_send<T: Send>() {}
        assert_traits::<Store>();
        assert_send::<Pending<RunRecord>>();
    }

    #[test]
    fn cutoff_saturates() {
        let now = Utc::now();
        assert_eq!(cutoff(now, Duration::ZERO), now);
        assert_eq!(cutoff(now, Duration::MAX), DateTime::<Utc>::MIN_UTC);
    }
}
