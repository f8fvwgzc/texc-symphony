//! Test doubles (also handy for running the server without an orchestrator).

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::watch;

use crate::control::{ControlPlane, StateError, Unavailable};
use crate::view::{IssueView, RefreshAccepted, StateView};

/// A [`ControlPlane`] that serves whatever state it was last given (Elixir test
/// `StaticOrchestrator`). Every [`set_state`](Self::set_state) bumps the generation.
#[derive(Debug)]
pub struct StaticControlPlane {
    inner: Mutex<Inner>,
    state_calls: AtomicUsize,
    issue_calls: AtomicUsize,
    refresh_calls: AtomicUsize,
}

#[derive(Debug)]
struct Inner {
    state: Result<StateView, StateError>,
    refresh: Result<bool, Unavailable>,
    delay: Option<Duration>,
    panic_on_state: bool,
    workspace_root: String,
    /// `None` once [`StaticControlPlane::close_changes`] dropped the sender.
    changes: Option<watch::Sender<u64>>,
    /// Last generation (kept after the sender is dropped).
    generation: u64,
}

impl StaticControlPlane {
    /// Serves `state`; refresh answers `coalesced: false`; workspace root `/tmp/symphony_workspaces`.
    pub fn new(state: Result<StateView, StateError>) -> Self {
        StaticControlPlane {
            inner: Mutex::new(Inner {
                state,
                refresh: Ok(false),
                delay: None,
                panic_on_state: false,
                workspace_root: "/tmp/symphony_workspaces".to_owned(),
                changes: Some(watch::Sender::new(0)),
                generation: 0,
            }),
            state_calls: AtomicUsize::new(0),
            issue_calls: AtomicUsize::new(0),
            refresh_calls: AtomicUsize::new(0),
        }
    }

    /// An orchestrator that is not running: snapshots are unavailable, refresh fails.
    pub fn unavailable() -> Self {
        let control = StaticControlPlane::new(Err(StateError::Unavailable));
        control.set_refresh(Err(Unavailable));
        control
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock only means another test thread panicked; the data is still usable.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replace the state and bump the generation (like `notify_update()`).
    pub fn set_state(&self, state: Result<StateView, StateError>) {
        self.lock().state = state;
        self.notify();
    }

    /// Bump the generation without changing the state.
    pub fn notify(&self) {
        let mut inner = self.lock();
        inner.generation += 1;
        let generation = inner.generation;
        if let Some(changes) = &inner.changes {
            changes.send_replace(generation);
        }
    }

    /// Current generation.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// `Ok(coalesced)` or `Err(Unavailable)` for refresh requests.
    pub fn set_refresh(&self, outcome: Result<bool, Unavailable>) {
        self.lock().refresh = outcome;
    }

    /// Sleep this long inside `state()` and `refresh()` (Elixir test `SlowOrchestrator`).
    pub fn set_delay(&self, delay: Option<Duration>) {
        self.lock().delay = delay;
    }

    /// Make `state()` panic (exercises the server's panic handling).
    pub fn set_panic_on_state(&self, panic: bool) {
        self.lock().panic_on_state = panic;
    }

    /// Workspace root returned by [`ControlPlane::workspace_root`].
    pub fn set_workspace_root(&self, root: impl Into<String>) {
        self.lock().workspace_root = root.into();
    }

    /// Number of `state()` calls so far (including those made by `issue()`).
    pub fn state_calls(&self) -> usize {
        self.state_calls.load(Ordering::SeqCst)
    }

    /// Number of `issue()` calls so far.
    pub fn issue_calls(&self) -> usize {
        self.issue_calls.load(Ordering::SeqCst)
    }

    /// Number of `refresh()` calls so far.
    pub fn refresh_calls(&self) -> usize {
        self.refresh_calls.load(Ordering::SeqCst)
    }

    /// Drop the change sender (the orchestrator went away): live streams keep their heartbeats
    /// but stop receiving snapshots, and later `changes()` calls return a closed receiver.
    pub fn close_changes(&self) {
        self.lock().changes = None;
    }

    async fn pause(&self) {
        let delay = self.lock().delay;
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
    }
}

#[async_trait]
impl ControlPlane for StaticControlPlane {
    async fn state(&self) -> Result<StateView, StateError> {
        self.state_calls.fetch_add(1, Ordering::SeqCst);
        self.pause().await;
        let inner = self.lock();
        if inner.panic_on_state {
            drop(inner);
            panic!("StaticControlPlane::state panicked on purpose");
        }
        inner.state.clone()
    }

    async fn refresh(&self) -> Result<RefreshAccepted, Unavailable> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        self.pause().await;
        let outcome = self.lock().refresh;
        outcome.map(|coalesced| RefreshAccepted::new(coalesced, Utc::now()))
    }

    fn workspace_root(&self) -> String {
        self.lock().workspace_root.clone()
    }

    async fn issue(&self, identifier: &str) -> Option<IssueView> {
        self.issue_calls.fetch_add(1, Ordering::SeqCst);
        let view = self.state().await.ok()?;
        crate::presenter::issue_view(identifier, &view, &self.workspace_root())
    }

    fn changes(&self) -> watch::Receiver<u64> {
        let inner = self.lock();
        match &inner.changes {
            Some(changes) => changes.subscribe(),
            // A receiver whose sender is already gone.
            None => watch::channel(inner.generation).1,
        }
    }
}
