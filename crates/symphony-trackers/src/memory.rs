//! In-memory tracker (`SymphonyElixir.Tracker.Memory`) for tests and local development.
//!
//! Issues come from a shared, mutable list (the Elixir `:memory_tracker_issues` application env) that
//! is read on every call. `tracker.kind: memory` is legal in production workflows; it simply starts
//! empty.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use symphony_core::Issue;
use symphony_core::config::TrackerSettings;
use symphony_core::issue::normalize_state;

use crate::{Tracker, TrackerError};

/// Shared handle to the memory tracker's issue list (cheap to clone).
#[derive(Debug, Clone, Default)]
pub struct MemoryIssues {
    inner: Arc<RwLock<Vec<Issue>>>,
}

impl MemoryIssues {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the configured issues.
    pub fn set(&self, issues: Vec<Issue>) {
        // A poisoned lock only means a writer panicked mid-assignment of a whole Vec; the data is
        // still a valid Vec, so recover it.
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = issues;
    }

    /// Snapshot of the configured issues.
    pub fn get(&self) -> Vec<Issue> {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The `memory` adapter.
#[derive(Debug, Clone, Default)]
pub struct MemoryTracker {
    issues: MemoryIssues,
}

impl MemoryTracker {
    /// A tracker reading from `issues`.
    pub fn new(issues: MemoryIssues) -> Self {
        Self { issues }
    }

    /// The shared issue list.
    pub fn issues(&self) -> &MemoryIssues {
        &self.issues
    }
}

#[async_trait]
impl Tracker for MemoryTracker {
    fn kind(&self) -> &'static str {
        "memory"
    }

    /// Issues (in configured order) whose trimmed, lowercased state is requested. An issue without a
    /// state normalizes to `""`.
    async fn fetch_issues_by_states(
        &self,
        _settings: &TrackerSettings,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let wanted: HashSet<String> = states.iter().map(|s| normalize_state(s)).collect();
        Ok(self
            .issues
            .get()
            .into_iter()
            .filter(|issue| wanted.contains(&normalize_state(issue.state.as_deref().unwrap_or(""))))
            .collect())
    }

    /// Issues (in configured order, duplicates preserved) whose exact id is requested.
    async fn fetch_issues_by_ids(
        &self,
        _settings: &TrackerSettings,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        Ok(self
            .issues
            .get()
            .into_iter()
            .filter(|issue| issue.id.as_deref().is_some_and(|id| wanted.contains(id)))
            .collect())
    }

    fn secret_environment_names(&self, _settings: &TrackerSettings) -> Vec<String> {
        Vec::new()
    }
}
