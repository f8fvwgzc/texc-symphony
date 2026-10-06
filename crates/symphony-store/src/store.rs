//! The `Store` handle: a cheap, cloneable front for one SQLite connection owned by a dedicated
//! database thread.

use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use tokio::sync::oneshot;

use crate::error::{Result, StoreError};
use crate::model::{
    NewRun, PruneStats, RetryRecord, RunEvent, RunId, RunPage, RunQuery, RunRecord, RunStatus,
    TokenUsage, TotalsRecord,
};
use crate::ops::{self, NewEvent};
use crate::schema;

type Job = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// Handle to the run history database.
///
/// `Store` is `Clone + Send + Sync`; clones share one connection that lives on a dedicated
/// `symphony-store` thread. Every operation is queued to that thread in submission order and
/// returns a [`Pending`] future:
///
/// * **await it** to get the result (`store.start_run(run).await?`);
/// * **drop it** for fire-and-forget writes — the write still happens and a failure is logged
///   with `tracing::warn!`. Submitting never blocks the caller.
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
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            None => f.write_str("Store(disabled)"),
            Some(inner) => match &inner.path {
                Some(path) => write!(f, "Store({})", path.display()),
                None => f.write_str("Store(:memory:)"),
            },
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
        Store::spawn(conn, Some(path))
    }

    /// Open a private in-memory database (tests, `SYMPHONY_DB_PATH=:memory:`).
    pub fn open_in_memory() -> Result<Store> {
        let mut conn = Connection::open_in_memory()?;
        schema::configure(&conn, false)?;
        schema::migrate(&mut conn)?;
        Store::spawn(conn, None)
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

    /// Database file path; `None` for in-memory and disabled stores.
    pub fn path(&self) -> Option<&Path> {
        self.inner.as_ref().and_then(|inner| inner.path.as_deref())
    }

    fn spawn(mut conn: Connection, path: Option<PathBuf>) -> Result<Store> {
        let (jobs, queue) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("symphony-store".to_string())
            .spawn(move || {
                while let Ok(job) = queue.recv() {
                    job(&mut conn);
                }
                if let Err((_, err)) = conn.close() {
                    tracing::warn!(error = %err, "closing symphony-store database failed");
                }
            })
            .map_err(|err| StoreError::Open {
                path: path
                    .as_ref()
                    .map_or_else(|| ":memory:".to_string(), |p| p.display().to_string()),
                reason: format!("cannot start database thread: {err}"),
            })?;
        Ok(Store {
            inner: Some(Arc::new(Inner { jobs, path })),
        })
    }

    /// Queue `op` on the database thread. `disabled` produces the result of a disabled store.
    fn submit<T, F>(&self, name: &'static str, disabled: impl FnOnce() -> T, op: F) -> Pending<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
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
        match inner.jobs.send(job) {
            Ok(()) => Pending::waiting(receiver),
            Err(_) => {
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

#[cfg(test)]
mod tests {
    use super::*;

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
