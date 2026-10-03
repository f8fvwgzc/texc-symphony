//! Reconciliation of running and blocked issues, stall detection, workspace cleanup and the startup
//! terminal cleanup (B.6, B.7).

use super::*;

impl Orchestrator<'_> {
    /// `reconcile_running_issues/1`: stall detection, then a tracker refresh of running issues.
    pub(crate) async fn reconcile_running_issues(&mut self) {
        self.reconcile_stalled_running_issues().await;
        let mut ids: Vec<String> = self.state.running.keys().cloned().collect();
        if ids.is_empty() {
            return;
        }
        ids.sort();
        let settings = self.settings();
        match self.ctx.tracker.fetch_issues_by_ids(&settings, &ids).await {
            Err(err) => tracing::debug!(
                "Failed to refresh running issue states: {err}; keeping active workers"
            ),
            Ok(issues) => {
                let visible: HashSet<String> = issues.iter().filter_map(|i| i.id.clone()).collect();
                self.reconcile_running_issue_states(issues).await;
                for id in ids.iter().filter(|id| !visible.contains(*id)) {
                    match self.state.running.get(id) {
                        Some(entry) => tracing::info!(
                            "Issue no longer visible during running-state refresh: issue_id={id} issue_identifier={}; stopping active agent",
                            entry.identifier
                        ),
                        None => tracing::info!(
                            "Issue no longer visible during running-state refresh: issue_id={id}; stopping active agent"
                        ),
                    }
                    self.terminate_running_issue(id, false, "issue no longer visible")
                        .await;
                }
            }
        }
    }

    /// `reconcile_running_issue_states/4`: terminal (stop + clean), unroutable / non-active (stop),
    /// active (refresh the entry's issue). The order of checks matters.
    pub(crate) async fn reconcile_running_issue_states(&mut self, issues: Vec<Issue>) {
        let settings = self.settings();
        let sets = StateSets::from_settings(&settings);
        for issue in issues {
            let Some(id) = issue.id.clone() else {
                continue;
            };
            let state = issue.state.as_deref().unwrap_or("");
            if sets.is_terminal(issue.state.as_deref()) {
                tracing::info!(
                    "Issue moved to terminal state: {} state={state}; stopping active agent",
                    issue_context(&issue)
                );
                self.terminate_running_issue(
                    &id,
                    true,
                    &format!("issue moved to terminal state {state}"),
                )
                .await;
            } else if !issue.routable(&settings.tracker.required_labels) {
                tracing::info!(
                    "Issue no longer routed to this worker: {} assignee={:?}; stopping active agent",
                    issue_context(&issue),
                    issue.assignee_id
                );
                self.terminate_running_issue(&id, false, "issue no longer routed to this worker")
                    .await;
            } else if sets.is_active(issue.state.as_deref()) {
                if let Some(entry) = self.state.running.get_mut(&id) {
                    entry.issue = issue;
                }
            } else {
                tracing::info!(
                    "Issue moved to non-active state: {} state={state}; stopping active agent",
                    issue_context(&issue)
                );
                self.terminate_running_issue(
                    &id,
                    false,
                    &format!("issue moved to non-active state {state}"),
                )
                .await;
            }
        }
    }

    /// Stall detection (B.6.2), on monotonic time since the last Codex activity.
    pub(crate) async fn reconcile_stalled_running_issues(&mut self) {
        let settings = self.settings();
        let timeout_ms = u128::from(settings.codex.stall_timeout_ms);
        if timeout_ms == 0 || self.state.running.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut ids: Vec<String> = self.state.running.keys().cloned().collect();
        ids.sort();
        for id in ids {
            if self.state.blocked.contains_key(&id) {
                continue;
            }
            let Some(entry) = self.state.running.get(&id) else {
                continue;
            };
            let elapsed_ms = now.duration_since(entry.last_activity).as_millis();
            if elapsed_ms <= timeout_ms {
                continue;
            }
            let session_id = entry.session_id.clone().unwrap_or_else(|| "n/a".into());
            let identifier = entry.identifier.clone();
            if entry.is_blocker() {
                let error = blocker_error(
                    entry.last_codex_event,
                    entry.last_codex_message.as_ref(),
                    &format!("stalled for {elapsed_ms}ms after Codex requested operator input"),
                );
                tracing::warn!(
                    "Issue blocked: issue_id={id} issue_identifier={identifier} session_id={session_id} elapsed_ms={elapsed_ms}; {error}"
                );
                let Some(mut entry) = self.state.running.remove(&id) else {
                    continue;
                };
                self.record_session_completion_totals(&entry);
                self.codex_streams.remove(&entry.run_id);
                self.stop_worker(entry.worker.take()).await;
                entry
                    .recorder
                    .finish(RunStatus::Blocked, Some(error.clone()));
                self.block_issue_from_entry(&id, entry, error);
            } else {
                tracing::warn!(
                    "Issue stalled: issue_id={id} issue_identifier={identifier} session_id={session_id} elapsed_ms={elapsed_ms}; restarting with backoff"
                );
                let error = format!("stalled for {elapsed_ms}ms without codex activity");
                let next = next_retry_attempt_from_running(entry);
                // G8 fix: keep the host and workspace so the retry prefers the same machine.
                let meta = RetryMeta {
                    identifier: Some(identifier),
                    issue_url: entry.issue.url.clone(),
                    error: Some(error.clone()),
                    worker_host: entry.worker_host.clone(),
                    workspace_path: entry.workspace_path.clone(),
                };
                self.terminate_running_issue(&id, false, &error).await;
                self.schedule_issue_retry(&id, next, meta, DelayType::Failure);
            }
        }
    }

    /// `reconcile_blocked_issues/1` (B.6.4). Blocked issues are never auto-retried.
    pub(crate) async fn reconcile_blocked_issues(&mut self) {
        let mut ids: Vec<String> = self.state.blocked.keys().cloned().collect();
        if ids.is_empty() {
            return;
        }
        ids.sort();
        let settings = self.settings();
        match self.ctx.tracker.fetch_issues_by_ids(&settings, &ids).await {
            Err(err) => tracing::debug!(
                "Failed to refresh blocked issue states: {err}; keeping blocked issues"
            ),
            Ok(issues) => {
                let visible: HashSet<String> = issues.iter().filter_map(|i| i.id.clone()).collect();
                self.reconcile_blocked_issue_states(issues).await;
                for id in ids.iter().filter(|id| !visible.contains(*id)) {
                    tracing::info!(
                        "Blocked issue no longer visible during state refresh: issue_id={id}; releasing block"
                    );
                    self.release_issue_claim(id);
                }
            }
        }
    }

    /// `reconcile_blocked_issue_states/4`.
    pub(crate) async fn reconcile_blocked_issue_states(&mut self, issues: Vec<Issue>) {
        let settings = self.settings();
        let sets = StateSets::from_settings(&settings);
        for issue in issues {
            let Some(id) = issue.id.clone() else {
                continue;
            };
            let state = issue.state.as_deref().unwrap_or("");
            if sets.is_terminal(issue.state.as_deref()) {
                tracing::info!(
                    "Blocked issue moved to terminal state: {} state={state}; releasing block",
                    issue_context(&issue)
                );
                let (path, host) = self
                    .state
                    .blocked
                    .get(&id)
                    .map(|b| (b.workspace_path.clone(), b.worker_host.clone()))
                    .unwrap_or_default();
                self.cleanup_issue_workspace(
                    issue.identifier.as_deref(),
                    path.as_deref(),
                    host.as_deref(),
                )
                .await;
                self.release_issue_claim(&id);
            } else if !issue.routable(&settings.tracker.required_labels) {
                tracing::info!(
                    "Blocked issue no longer routed to this worker: {} assignee={:?}; releasing block",
                    issue_context(&issue),
                    issue.assignee_id
                );
                self.release_issue_claim(&id);
            } else if sets.is_active(issue.state.as_deref()) {
                if let Some(entry) = self.state.blocked.get_mut(&id) {
                    entry.issue = Some(issue);
                }
            } else {
                tracing::info!(
                    "Blocked issue moved to non-active state: {} state={state}; releasing block",
                    issue_context(&issue)
                );
                self.release_issue_claim(&id);
            }
        }
    }

    /// `cleanup_issue_workspace/2`: the recorded path when known (even if `workspace.root` changed),
    /// else `<root>/<key>` for the identifier. Failures are logged.
    pub(crate) async fn cleanup_issue_workspace(
        &self,
        identifier: Option<&str>,
        workspace_path: Option<&str>,
        worker_host: Option<&str>,
    ) {
        let settings = self.settings();
        let manager = self.workspace_manager(&settings);
        match workspace_path.filter(|p| !p.is_empty()) {
            Some(path) => {
                if let Err(err) = manager.remove_recorded(path, worker_host).await {
                    tracing::warn!(
                        "Failed to remove workspace path={path} worker_host={}: {err}",
                        worker_host.unwrap_or("local")
                    );
                }
            }
            None => {
                manager
                    .remove_issue_workspaces(identifier, worker_host)
                    .await
            }
        }
    }

    /// `run_terminal_workspace_cleanup/0` (B.7): removes the workspaces of every terminal issue,
    /// concurrently and bounded (Elixir went issue by issue and host by host, G9).
    pub(crate) async fn run_terminal_workspace_cleanup(&mut self) {
        let settings = self.settings();
        let issues = match self
            .ctx
            .tracker
            .fetch_issues_by_states(&settings, settings.terminal_states())
            .await
        {
            Ok(issues) => issues,
            Err(err) => {
                tracing::warn!(
                    "Skipping startup terminal workspace cleanup; failed to fetch terminal issues: {err}"
                );
                return;
            }
        };
        let manager = self.workspace_manager(&settings);
        let hosts = worker_hosts(&settings);
        let targets: Vec<(String, Option<String>)> = issues
            .into_iter()
            .filter_map(|issue| issue.identifier)
            .flat_map(|identifier| {
                if hosts.is_empty() {
                    vec![(identifier, None)]
                } else {
                    hosts
                        .iter()
                        .map(|host| (identifier.clone(), Some(host.clone())))
                        .collect()
                }
            })
            .collect();
        let manager = &manager;
        futures::stream::iter(targets)
            .map(|(identifier, host)| async move {
                manager
                    .remove_issue_workspaces(Some(&identifier), host.as_deref())
                    .await;
            })
            .buffer_unordered(self.ctx.startup_cleanup_concurrency)
            .collect::<Vec<()>>()
            .await;
    }
}
