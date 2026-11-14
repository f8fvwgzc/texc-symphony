//! The system under test, assembled the way the `symphony` binary assembles it: a workflow file
//! with `tracker.kind: memory`, a SQLite store, the runtime, and the HTTP server on a loopback
//! port. Only the worker factory (fake workers) and the tracker list (the harness's board) differ.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use symphony_cli::control::RuntimeControlPlane;
use symphony_core::WorkflowStore;
use symphony_runtime::{FnWorkerFactory, Runtime, RuntimeHandle, RuntimeOptions, WorkerContext};
use symphony_server::{BoundServer, ServerConfig};
use symphony_store::Store;
use symphony_trackers::TrackerDeps;
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::Res;
use crate::world::{ACTIVE_STATE, Plan, SlowTracker, World};

/// The prompt of the repository's own `WORKFLOW.md` (everything after its front matter), so the
/// workflow file has the size of a real one: the runtime re-reads it often.
fn repository_prompt() -> &'static str {
    const WORKFLOW: &str = include_str!("../../../../WORKFLOW.md");
    WORKFLOW
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map_or(WORKFLOW, |(_, prompt)| prompt)
}

/// Which run-history store the runtime writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum StoreKind {
    /// A SQLite file in the run's temporary directory (WAL, as in production).
    Sqlite,
    /// `Store::disabled()`: no persistence.
    None,
}

/// What to build.
#[derive(Debug)]
pub struct RigConfig {
    /// `agent.max_concurrent_agents`.
    pub agents: u32,
    /// `polling.interval_ms`.
    pub poll_interval_ms: u64,
    pub store: StoreKind,
    /// Delay added to every tracker call.
    pub tracker_latency: Duration,
    /// Serve the HTTP API (needed for the SSE probe).
    pub serve: bool,
    pub plan: Plan,
}

/// A running system under test.
pub struct Rig {
    pub world: Arc<World>,
    pub handle: RuntimeHandle,
    pub store: Store,
    pub tracker: Arc<SlowTracker>,
    pub db_path: Option<PathBuf>,
    /// `http://127.0.0.1:<port>` when the server runs.
    pub base_url: Option<String>,
    runtime: Runtime,
    server: Option<BoundServer>,
    server_shutdown: CancellationToken,
    board: JoinHandle<()>,
    _dir: TempDir,
}

impl Rig {
    pub async fn start(config: RigConfig) -> Res<Self> {
        let dir = tempfile::tempdir()?;
        let workflow_path = dir.path().join("WORKFLOW.md");
        let front_matter = json!({
            "tracker": {
                "kind": "memory",
                "active_states": [ACTIVE_STATE, "In Progress"],
                "terminal_states": ["Closed", "Cancelled", "Canceled", "Duplicate", "Done"],
            },
            "polling": {"interval_ms": config.poll_interval_ms},
            "workspace": {"root": dir.path().join("workspaces").to_string_lossy()},
            "agent": {"max_concurrent_agents": config.agents},
        });
        std::fs::write(
            &workflow_path,
            format!(
                "---\n{}\n---\n{}",
                serde_json::to_string_pretty(&front_matter)?,
                repository_prompt()
            ),
        )?;
        let workflow = WorkflowStore::start(Some(workflow_path))?;

        let (store, db_path) = match config.store {
            StoreKind::Sqlite => {
                let path = dir.path().join("symphony.db");
                (Store::open(&path)?, Some(path))
            }
            StoreKind::None => (Store::disabled(), None),
        };

        let world = World::new(config.plan);
        let tracker = Arc::new(SlowTracker::new(
            world.tracker_issues(),
            config.tracker_latency,
        ));
        let workers = Arc::clone(&world);
        let factory =
            FnWorkerFactory(move |ctx: WorkerContext| Arc::clone(&workers).run_worker(ctx));
        let options = RuntimeOptions::new(Arc::clone(&workflow), TrackerDeps::new()?)
            .with_tracker(Arc::clone(&tracker) as Arc<dyn symphony_trackers::Tracker>)
            .with_store(store.clone())
            .with_worker_factory(Arc::new(factory));
        let runtime = Runtime::start(options);
        let handle = runtime.handle();

        let server_shutdown = CancellationToken::new();
        let (server, base_url) = if config.serve {
            let control = Arc::new(RuntimeControlPlane::new(handle.clone(), workflow));
            let server = symphony_server::serve(
                ServerConfig::new("127.0.0.1", 0),
                control,
                store.clone(),
                server_shutdown.clone(),
            )
            .await?;
            let url = format!("http://{}", server.local_addr());
            (Some(server), Some(url))
        } else {
            (None, None)
        };

        let board = world.spawn_board();
        let rig = Self {
            world,
            handle,
            store,
            tracker,
            db_path,
            base_url,
            runtime,
            server,
            server_shutdown,
            board,
            _dir: dir,
        };
        rig.wait_for_first_poll().await?;
        Ok(rig)
    }

    /// Waits until the startup poll cycle is over, so a later refresh starts a fresh one.
    async fn wait_for_first_poll(&self) -> Res<()> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snapshot = self.handle.snapshot().await?;
            if !snapshot.polling.checking && snapshot.polling.next_poll_in_ms.is_some() {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err("the orchestrator did not finish its startup poll".into());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Waits until `count` workers have started in total.
    pub async fn wait_started(&self, count: u64, limit: Duration) -> Res<()> {
        let deadline = Instant::now() + limit;
        while self
            .world
            .metrics
            .started
            .load(std::sync::atomic::Ordering::Relaxed)
            < count
        {
            if Instant::now() > deadline {
                return Err(format!("fewer than {count} workers started within {limit:?}").into());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok(())
    }

    /// URL of the live event stream.
    pub fn events_url(&self) -> Res<String> {
        let base = self
            .base_url
            .as_ref()
            .ok_or("the HTTP server is not running")?;
        Ok(format!("{base}/api/v1/events"))
    }

    /// Events recorded in run history so far (0 without a store).
    pub async fn persisted_events(&self) -> Res<u64> {
        let Some(path) = self.db_path.clone() else {
            return Ok(0);
        };
        tokio::task::spawn_blocking(move || StoreCounts::read(&path).map(|counts| counts.events))
            .await?
    }

    /// Stops the runtime and the server, waits until run history has been written, and returns
    /// what the SQLite file holds.
    pub async fn stop(self) -> Res<StoreCounts> {
        self.board.abort();
        self.runtime.shutdown().await?;
        self.server_shutdown.cancel();
        if let Some(server) = self.server {
            server.wait().await?;
        }
        let Some(path) = &self.db_path else {
            return Ok(StoreCounts::default());
        };
        // Each run's recorder still writes what it had queued: wait until the count is stable.
        let mut counts = StoreCounts::read(path)?;
        for _ in 0..1_200 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            self.store.flush().await?;
            let now = StoreCounts::read(path)?;
            let stable = now.events == counts.events;
            counts = now;
            if stable {
                break;
            }
        }
        Ok(counts)
    }
}

/// Row counts of a run's SQLite file.
#[derive(Debug, Clone, Copy, Default)]
pub struct StoreCounts {
    pub runs: u64,
    pub events: u64,
    /// `events_dropped` markers (one per run that lost events).
    pub runs_with_dropped_events: u64,
    pub db_bytes: u64,
    pub wal_bytes: u64,
}

impl StoreCounts {
    fn read(path: &std::path::Path) -> Res<Self> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let count = |sql: &str| -> Res<u64> {
            let value: i64 = conn.query_row(sql, [], |row| row.get(0))?;
            Ok(u64::try_from(value).unwrap_or(0))
        };
        let size = |path: std::path::PathBuf| std::fs::metadata(path).map_or(0, |meta| meta.len());
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        Ok(Self {
            runs: count("SELECT COUNT(*) FROM runs")?,
            // Each run counts its events, so this does not scan the event table.
            events: count("SELECT COALESCE(SUM(last_event_seq), 0) FROM runs")?,
            runs_with_dropped_events: count(
                "SELECT COUNT(*) FROM run_events WHERE kind = 'events_dropped'",
            )?,
            db_bytes: size(path.to_path_buf()),
            wal_bytes: size(wal.into()),
        })
    }

    pub fn json(&self) -> serde_json::Value {
        json!({
            "runs": self.runs,
            "events": self.events,
            "runs_with_dropped_events": self.runs_with_dropped_events,
            "db_bytes": self.db_bytes,
            "wal_bytes": self.wal_bytes,
        })
    }
}
