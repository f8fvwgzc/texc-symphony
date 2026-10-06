//! Retry queue: scheduling with backoff, token-checked firing, and the retry lookup (B.5).

use super::*;

impl Orchestrator<'_> {
    pub(crate) fn remove_retry_entry(&mut self, issue_id: &str) -> Option<RetryEntry> {
        let entry = self.state.retry_attempts.remove(issue_id)?;
        self.ctx.store.delete_retry(issue_id).detach();
        // Tokens already neutralize the timer; aborting just avoids a pointless wake-up.
        if let Some(timer) = &entry.timer {
            timer.abort();
        }
        Some(entry)
    }

    /// Startup: re-queues the retries a previous process had pending, with their attempt counts and
    /// what is left of their delays (new in the Rust port). The issues are claimed again, and when a
    /// retry fires it goes through the normal path, which re-fetches the issue first, so an entry
    /// whose issue has since been closed or removed is simply released.
    pub(crate) async fn restore_retry_queue(&mut self) {
        let retries = match self.ctx.store.list_retries().await {
            Ok(retries) => retries,
            Err(err) => {
                tracing::warn!("Failed to restore the retry queue: {err}");
                return;
            }
        };
        if retries.is_empty() {
            return;
        }
        tracing::info!(
            "Restoring {} queued retries from the previous run",
            retries.len()
        );
        let now = chrono::Utc::now();
        for retry in retries {
            let delay = (retry.due_at - now).to_std().unwrap_or(Duration::ZERO);
            let token = self.tokens.next();
            let timer = self.spawn_timer(
                delay,
                TimerEvent::RetryDue {
                    issue_id: retry.issue_id.clone(),
                    token,
                },
            );
            self.state.claimed.insert(retry.issue_id.clone());
            self.state.retry_attempts.insert(
                retry.issue_id,
                RetryEntry {
                    attempt: retry.attempt,
                    token,
                    timer: Some(timer),
                    due_at: Instant::now() + delay,
                    identifier: retry.identifier,
                    issue_url: retry.issue_url,
                    error: retry.error,
                    worker_host: retry.worker_host,
                    workspace_path: retry.workspace_path,
                },
            );
        }
    }

    /// `release_issue_claim/2`.
    pub(crate) fn release_issue_claim(&mut self, issue_id: &str) {
        self.state.claimed.remove(issue_id);
        self.state.blocked.remove(issue_id);
        self.remove_retry_entry(issue_id);
    }

    /// `schedule_issue_retry/4`. `attempt = None` means "previous attempt + 1". The issue stays (or
    /// becomes) claimed while queued.
    pub(crate) fn schedule_issue_retry(
        &mut self,
        issue_id: &str,
        attempt: Option<u32>,
        meta: RetryMeta,
        delay_type: DelayType,
    ) {
        let previous = self.remove_retry_entry(issue_id);
        let next_attempt =
            attempt.unwrap_or_else(|| previous.as_ref().map_or(0, |p| p.attempt) + 1);
        let settings = self.settings();
        let delay = retry_delay(next_attempt, delay_type, &settings.agent);
        let token = self.tokens.next();
        let pick = |new: Option<String>, old: Option<&Option<String>>| {
            new.or_else(|| old.and_then(Clone::clone))
        };
        let identifier = meta
            .identifier
            .or_else(|| previous.as_ref().map(|p| p.identifier.clone()))
            .unwrap_or_else(|| issue_id.to_owned());
        let issue_url = pick(meta.issue_url, previous.as_ref().map(|p| &p.issue_url));
        let error = pick(meta.error, previous.as_ref().map(|p| &p.error));
        let worker_host = pick(meta.worker_host, previous.as_ref().map(|p| &p.worker_host));
        let workspace_path = pick(
            meta.workspace_path,
            previous.as_ref().map(|p| &p.workspace_path),
        );
        let timer = self.spawn_timer(
            delay,
            TimerEvent::RetryDue {
                issue_id: issue_id.to_owned(),
                token,
            },
        );
        let error_suffix = error
            .as_deref()
            .map(|e| format!(" error={e}"))
            .unwrap_or_default();
        tracing::warn!(
            "Retrying issue_id={issue_id} issue_identifier={identifier} in {}ms (attempt {next_attempt}){error_suffix}",
            delay.as_millis()
        );
        // Kept across restarts (a no-op without a store); see `restore_retry_queue`.
        let wait = chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX);
        let due = chrono::Utc::now()
            .checked_add_signed(wait)
            .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC);
        self.ctx
            .store
            .save_retry(RetryRecord {
                issue_id: issue_id.to_owned(),
                attempt: next_attempt,
                due_at: due,
                identifier: identifier.clone(),
                issue_url: issue_url.clone(),
                error: error.clone(),
                worker_host: worker_host.clone(),
                workspace_path: workspace_path.clone(),
            })
            .detach();
        self.state.claimed.insert(issue_id.to_owned());
        self.state.retry_attempts.insert(
            issue_id.to_owned(),
            RetryEntry {
                attempt: next_attempt,
                token,
                timer: Some(timer),
                due_at: Instant::now() + delay,
                identifier,
                issue_url,
                error,
                worker_host,
                workspace_path,
            },
        );
    }

    /// `handle_info({:retry_issue, id, token})`: only the entry with the matching token is consumed.
    pub(crate) async fn handle_retry_due(&mut self, issue_id: &str, token: u64) {
        let matches = self
            .state
            .retry_attempts
            .get(issue_id)
            .is_some_and(|entry| entry.token == token);
        if matches && let Some(entry) = self.remove_retry_entry(issue_id) {
            let attempt = entry.attempt;
            let meta = entry.meta();
            self.handle_retry_issue(issue_id, attempt, meta).await;
        }
        self.notify();
    }

    pub(crate) async fn handle_retry_issue(
        &mut self,
        issue_id: &str,
        attempt: u32,
        meta: RetryMeta,
    ) {
        let settings = self.settings();
        let ids = [issue_id.to_owned()];
        match self.ctx.tracker.fetch_issues_by_ids(&settings, &ids).await {
            Err(err) => {
                tracing::warn!(
                    "Retry poll failed for issue_id={issue_id} issue_identifier={}: {err}",
                    meta.identifier.as_deref().unwrap_or(issue_id)
                );
                self.schedule_issue_retry(
                    issue_id,
                    Some(attempt + 1),
                    RetryMeta {
                        error: Some(format!("retry poll failed: {err}")),
                        ..meta
                    },
                    DelayType::Failure,
                );
            }
            Ok(issues) => {
                let found = issues
                    .into_iter()
                    .find(|issue| issue.id.as_deref() == Some(issue_id));
                self.handle_retry_issue_lookup(found, issue_id, attempt, meta)
                    .await;
            }
        }
    }

    /// `handle_retry_issue_lookup/5`.
    pub(crate) async fn handle_retry_issue_lookup(
        &mut self,
        issue: Option<Issue>,
        issue_id: &str,
        attempt: u32,
        meta: RetryMeta,
    ) {
        let Some(issue) = issue else {
            tracing::debug!("Issue no longer visible, removing claim issue_id={issue_id}");
            self.release_issue_claim(issue_id);
            return;
        };
        let settings = self.settings();
        let sets = StateSets::from_settings(&settings);
        if candidate_issue(&issue, &sets, &settings) && !sets.is_terminal(issue.state.as_deref()) {
            self.handle_active_retry(issue, issue_id, attempt, meta)
                .await;
        } else {
            self.release_inactive_retry(&issue, issue_id, &meta, &sets)
                .await;
        }
    }

    /// The non-candidate branches of the lookup: terminal → clean + release, otherwise release.
    pub(crate) async fn release_inactive_retry(
        &mut self,
        issue: &Issue,
        issue_id: &str,
        meta: &RetryMeta,
        sets: &StateSets,
    ) {
        if sets.is_terminal(issue.state.as_deref()) {
            tracing::info!(
                "Issue state is terminal: issue_id={issue_id} issue_identifier={} state={}; removing associated workspace",
                issue.identifier.as_deref().unwrap_or(""),
                issue.state.as_deref().unwrap_or("")
            );
            self.cleanup_issue_workspace(
                issue.identifier.as_deref(),
                meta.workspace_path.as_deref(),
                meta.worker_host.as_deref(),
            )
            .await;
        } else {
            tracing::debug!(
                "Issue left active states, removing claim issue_id={issue_id} issue_identifier={}",
                issue.identifier.as_deref().unwrap_or("")
            );
        }
        self.release_issue_claim(issue_id);
    }

    pub(crate) async fn handle_active_retry(
        &mut self,
        issue: Issue,
        issue_id: &str,
        attempt: u32,
        meta: RetryMeta,
    ) {
        let settings = self.settings();
        let host = self
            .state
            .select_worker_host(&settings, meta.worker_host.as_deref());
        let no_slots = |meta: RetryMeta, issue: &Issue| RetryMeta {
            identifier: issue.identifier.clone().or(meta.identifier.clone()),
            error: Some("no available orchestrator slots".into()),
            ..meta
        };
        if self.state.available_slots() == 0
            || !self.state.state_slots_available(&issue, &settings)
            || host == HostChoice::NoCapacity
        {
            tracing::debug!(
                "No available slots for retrying {}; retrying again",
                issue_context(&issue)
            );
            let meta = no_slots(meta, &issue);
            self.schedule_issue_retry(issue_id, Some(attempt + 1), meta, DelayType::Failure);
            return;
        }
        match self.refresh_issue_for_dispatch(&issue).await {
            Revalidation::Ok(refreshed) => {
                let preferred = meta.worker_host.clone();
                if !self.do_dispatch_issue(refreshed, Some(attempt), preferred.as_deref()) {
                    // G7: never drop a popped retry (Elixir could leave a claim without a timer).
                    let meta = no_slots(meta, &issue);
                    self.schedule_issue_retry(
                        issue_id,
                        Some(attempt + 1),
                        meta,
                        DelayType::Failure,
                    );
                }
            }
            Revalidation::SkipMissing => self.release_issue_claim(issue_id),
            Revalidation::Skip(refreshed) => {
                let sets = StateSets::from_settings(&settings);
                self.release_inactive_retry(&refreshed, issue_id, &meta, &sets)
                    .await;
            }
            Revalidation::Error(err) => {
                let meta = RetryMeta {
                    identifier: issue.identifier.clone().or(meta.identifier.clone()),
                    error: Some(format!("retry dispatch refresh failed: {err}")),
                    ..meta
                };
                self.schedule_issue_retry(issue_id, Some(attempt + 1), meta, DelayType::Failure);
            }
        }
    }
}
