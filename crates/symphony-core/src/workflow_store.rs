//! Last-known-good workflow cache with stamp-based reload (`SymphonyElixir.WorkflowStore`).
//!
//! - [`WorkflowStore::start`] loads and validates the workflow; an invalid workflow **refuses to boot**.
//! - Every read ([`WorkflowStore::current`], [`WorkflowStore::settings`], ...) re-stamps the file
//!   (`mtime` seconds, size, content hash) and reloads when it changed or the configured path changed.
//! - A failed reload keeps the last good snapshot and logs
//!   `Failed to reload workflow path=... reason=...; keeping last known good configuration`; only
//!   [`WorkflowStore::force_reload`] (Elixir `Config.validate!/0`) surfaces the error.
//! - [`WorkflowStore::spawn_poller`] re-checks every second.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use arc_swap::ArcSwap;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{self, CodexRuntimeSettings, Settings};
use crate::env::{EnvSource, ProcessEnv};
use crate::error::{ConfigError, IoReason};
use crate::path_safety::PathError;
use crate::prompt;
use crate::workflow::{self, LoadedWorkflow, WORKFLOW_FILE_NAME};

/// Poll interval of the background reload task.
pub const POLL_INTERVAL: Duration = Duration::from_millis(1_000);

/// Identical consecutive reload failures are logged at most this often (Elixir logs on every poll).
pub const RELOAD_ERROR_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// One consistent, validated view of the workflow file.
#[derive(Debug, Clone)]
pub struct WorkflowSnapshot {
    /// The file the snapshot was loaded from.
    pub path: PathBuf,
    /// Parsed workflow (front matter + prompt).
    pub workflow: Arc<LoadedWorkflow>,
    /// Cast, finalized and validated settings.
    pub settings: Arc<Settings>,
}

/// Change-detection stamp: `(mtime seconds, size, content hash)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    mtime_secs: i64,
    size: u64,
    hash: u64,
}

fn mtime_secs(meta: &fs::Metadata) -> i64 {
    match meta.modified() {
        Ok(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            Err(before) => -i64::try_from(before.duration().as_secs()).unwrap_or(i64::MAX),
        },
        Err(_) => 0,
    }
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn current_stamp(path: &Path) -> Result<Stamp, IoReason> {
    let meta = fs::metadata(path).map_err(|e| IoReason::from_io(&e))?;
    let bytes = fs::read(path).map_err(|e| IoReason::from_io(&e))?;
    Ok(Stamp {
        mtime_secs: mtime_secs(&meta),
        size: meta.len(),
        hash: content_hash(&bytes),
    })
}

/// `load_state/1` without a running store: load, parse, validate, then stamp.
pub fn load_snapshot(path: &Path, env: &dyn EnvSource) -> Result<WorkflowSnapshot, ConfigError> {
    load_state(path, env).map(|(snapshot, _)| snapshot)
}

fn load_state(path: &Path, env: &dyn EnvSource) -> Result<(WorkflowSnapshot, Stamp), ConfigError> {
    let loaded = workflow::load(path)?;
    let settings = config::parse(&loaded.config, env)?;
    config::validate_settings(&settings, env)?;
    let stamp = current_stamp(path).map_err(|reason| ConfigError::WorkflowFileUnreadable {
        path: path.to_path_buf(),
        reason,
    })?;
    Ok((
        WorkflowSnapshot {
            path: path.to_path_buf(),
            workflow: Arc::new(loaded),
            settings: Arc::new(settings),
        },
        stamp,
    ))
}

/// Construction options for [`WorkflowStore`].
#[derive(Debug, Clone)]
pub struct WorkflowStoreOptions {
    /// Explicit workflow path (Elixir app env `:workflow_file_path`); `None` means `<cwd>/WORKFLOW.md`.
    pub path: Option<PathBuf>,
    /// Directory used for the default path (captured once; defaults to the process CWD).
    pub cwd: PathBuf,
    /// Environment used for `$VAR` resolution and adapter fallbacks.
    pub env: Arc<dyn EnvSource>,
}

impl Default for WorkflowStoreOptions {
    fn default() -> Self {
        Self {
            path: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            env: Arc::new(ProcessEnv),
        }
    }
}

#[derive(Debug)]
struct ReloadState {
    path: PathBuf,
    stamp: Stamp,
}

#[derive(Debug, Default)]
struct ErrorLog {
    last: Option<(String, Instant)>,
}

/// Last-known-good workflow store (see the module docs).
#[derive(Debug)]
pub struct WorkflowStore {
    env: Arc<dyn EnvSource>,
    cwd: PathBuf,
    path_override: RwLock<Option<PathBuf>>,
    state: Mutex<ReloadState>,
    snapshot: ArcSwap<WorkflowSnapshot>,
    error_log: Mutex<ErrorLog>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock cannot leave the state half-written (fields are replaced whole).
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl WorkflowStore {
    /// Starts a store on `path` (or `<cwd>/WORKFLOW.md`) with the process environment.
    pub fn start(path: Option<PathBuf>) -> Result<Arc<Self>, ConfigError> {
        Self::start_with(WorkflowStoreOptions {
            path,
            ..WorkflowStoreOptions::default()
        })
    }

    /// Starts a store; fails (refuses to boot) when the workflow is missing or invalid.
    pub fn start_with(options: WorkflowStoreOptions) -> Result<Arc<Self>, ConfigError> {
        let WorkflowStoreOptions { path, cwd, env } = options;
        let initial_path = path.clone().unwrap_or_else(|| cwd.join(WORKFLOW_FILE_NAME));
        let (snapshot, stamp) = load_state(&initial_path, env.as_ref())?;
        Ok(Arc::new(Self {
            env,
            cwd,
            path_override: RwLock::new(path),
            state: Mutex::new(ReloadState {
                path: initial_path,
                stamp,
            }),
            snapshot: ArcSwap::from_pointee(snapshot),
            error_log: Mutex::new(ErrorLog::default()),
        }))
    }

    /// `Workflow.workflow_file_path/0`: the configured path, else `<cwd>/WORKFLOW.md`.
    pub fn workflow_file_path(&self) -> PathBuf {
        let configured = self
            .path_override
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        configured.unwrap_or_else(|| self.cwd.join(WORKFLOW_FILE_NAME))
    }

    /// `Workflow.set_workflow_file_path/1`: switches the path and force-reloads (result ignored; a bad new
    /// path keeps the last good snapshot).
    pub fn set_workflow_file_path(&self, path: impl Into<PathBuf>) {
        *self
            .path_override
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(path.into());
        let _ = self.force_reload();
    }

    /// `Workflow.clear_workflow_file_path/0`: back to `<cwd>/WORKFLOW.md`, then force-reload.
    pub fn clear_workflow_file_path(&self) {
        *self
            .path_override
            .write()
            .unwrap_or_else(PoisonError::into_inner) = None;
        let _ = self.force_reload();
    }

    /// The environment used for resolution.
    pub fn env(&self) -> &dyn EnvSource {
        self.env.as_ref()
    }

    /// Reloads if needed and returns the (possibly unchanged) last good snapshot. Never fails.
    pub fn snapshot(&self) -> Arc<WorkflowSnapshot> {
        let _ = self.reload();
        self.snapshot.load_full()
    }

    /// The last good snapshot without touching the file system.
    pub fn last_good(&self) -> Arc<WorkflowSnapshot> {
        self.snapshot.load_full()
    }

    /// `WorkflowStore.current/0`: reload-if-changed, then the last good workflow.
    pub fn current(&self) -> Arc<LoadedWorkflow> {
        self.snapshot().workflow.clone()
    }

    /// `WorkflowStore.settings/0` / `Config.settings!/0`: reload-if-changed, then the last good settings.
    pub fn settings(&self) -> Arc<Settings> {
        self.snapshot().settings.clone()
    }

    /// `WorkflowStore.force_reload/0` / `Config.validate!/0`: reload if changed and report the failure
    /// (the last good snapshot is kept). An unchanged file is `Ok(())`.
    pub fn force_reload(&self) -> Result<(), ConfigError> {
        self.reload()
    }

    /// One background poll (`handle_info(:poll)`): reload if changed, errors are logged and swallowed.
    pub fn poll(&self) {
        let _ = self.reload();
    }

    /// Spawns the 1 s poll loop on the current tokio runtime; stops when `cancel` fires.
    pub fn spawn_poller(self: &Arc<Self>, cancel: CancellationToken) -> JoinHandle<()> {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick completes immediately; the store was just loaded.
            interval.tick().await;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => store.poll(),
                }
            }
        })
    }

    /// `Config.workflow_prompt/0` on the current workflow.
    pub fn workflow_prompt(&self) -> String {
        prompt::workflow_prompt(&self.current()).to_owned()
    }

    /// `Config.local_workspace_root/0`: `workspace.root` resolved against the workflow file's directory.
    pub fn local_workspace_root(&self) -> PathBuf {
        let snapshot = self.snapshot();
        snapshot
            .settings
            .local_workspace_root(&self.workflow_file_path())
    }

    /// `Config.codex_runtime_settings/2`, resolving a relative local root against the workflow directory.
    pub fn codex_runtime_settings(
        &self,
        workspace: Option<&str>,
        remote: bool,
    ) -> Result<CodexRuntimeSettings, PathError> {
        let snapshot = self.snapshot();
        let workflow = crate::path_safety::expand_path(self.workflow_file_path(), None);
        let base = workflow.parent().map(Path::to_path_buf);
        snapshot
            .settings
            .codex_runtime_settings(workspace, remote, base.as_deref())
    }

    fn reload(&self) -> Result<(), ConfigError> {
        let path = self.workflow_file_path();
        let mut state = lock(&self.state);
        if path != state.path {
            return self.reload_path(&mut state, path);
        }
        match current_stamp(&path) {
            Ok(stamp) if stamp == state.stamp => Ok(()),
            Ok(_) => self.reload_path(&mut state, path),
            Err(reason) => {
                let err = ConfigError::WorkflowFileUnreadable { path, reason };
                self.log_reload_error(&err);
                Err(err)
            }
        }
    }

    fn reload_path(&self, state: &mut ReloadState, path: PathBuf) -> Result<(), ConfigError> {
        match load_state(&path, self.env.as_ref()) {
            Ok((snapshot, stamp)) => {
                state.path = path;
                state.stamp = stamp;
                self.snapshot.store(Arc::new(snapshot));
                lock(&self.error_log).last = None;
                Ok(())
            }
            Err(err) => {
                self.log_reload_error(&err);
                Err(err)
            }
        }
    }

    fn log_reload_error(&self, err: &ConfigError) {
        let (path, reason) = match err {
            ConfigError::WorkflowFileUnreadable { path, reason } => {
                (path.clone(), reason.to_string())
            }
            other => (self.workflow_file_path(), other.to_string()),
        };
        let line = format!(
            "Failed to reload workflow path={} reason={reason}; keeping last known good configuration",
            path.display()
        );
        let mut log = lock(&self.error_log);
        let now = Instant::now();
        let should_log = match &log.last {
            Some((previous, at)) => {
                previous != &line || now.duration_since(*at) >= RELOAD_ERROR_LOG_INTERVAL
            }
            None => true,
        };
        if should_log {
            tracing::error!("{line}");
            log.last = Some((line, now));
        }
    }
}
