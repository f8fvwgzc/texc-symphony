//! Worker lifecycle: runtime info, Codex events, completions (`DOWN`), termination and blocking.

use super::*;

impl Orchestrator<'_> {
    pub(crate) fn handle_worker_message(&mut self, message: WorkerMessage) {
        match message {
            WorkerMessage::RuntimeInfo {
                run_id,
                worker_host,
                workspace_path,
            } => {
                let Some(entry) = self
                    .state
                    .running
                    .values_mut()
                    .find(|entry| entry.run_id == run_id)
                else {
                    return;
                };
                if worker_host.is_some() {
                    entry.worker_host = worker_host;
                }
                entry.workspace_path = Some(workspace_path);
                entry.recorder.runtime_info(
                    entry.worker_host.as_deref(),
                    entry.workspace_path.as_deref(),
                );
                self.notify();
            }
        }
    }

    /// `handle_info({:codex_worker_update, ...})` for a running entry of the same run.
    pub(crate) fn handle_codex_event(&mut self, run_id: u64, event: CodexEvent) {
        let Some(entry) = self
            .state
            .running
            .values_mut()
            .find(|entry| entry.run_id == run_id)
        else {
            return;
        };
        let (delta, new_turn) = entry.integrate(&event, Instant::now());
        entry.recorder.event(&event);
        if new_turn {
            entry.recorder.turn();
        }
        if !delta.is_zero() {
            entry.recorder.tokens(entry.tokens.totals);
        }
        self.state.codex_totals.add_tokens(delta);
        if let Some(rate_limits) = event.rate_limits {
            self.state.codex_rate_limits = Some(rate_limits);
        }
        self.notify();
    }

    pub(crate) fn drain_worker_messages(&mut self) {
        while let Ok(message) = self.worker_rx.try_recv() {
            self.handle_worker_message(message);
        }
    }

    pub(crate) fn drain_codex_events(&mut self, run_id: u64) {
        if let Some(stream) = self.codex_streams.remove(&run_id) {
            let mut rx = stream.into_inner();
            while let Ok(event) = rx.try_recv() {
                self.handle_codex_event(run_id, event);
            }
        }
    }

    /// The `DOWN` handler: maps the completed task to its run, integrates the run's last messages
    /// first (they were sent before it finished), then decides block / continuation / retry.
    pub(crate) fn handle_worker_joined(
        &mut self,
        joined: Result<(tokio::task::Id, WorkerResult), JoinError>,
    ) {
        let (task_id, result) = match joined {
            Ok((id, result)) => (id, result),
            Err(err) => {
                let id = err.id();
                let result = if err.is_panic() {
                    let payload = err.into_panic();
                    let text = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "panic".to_owned());
                    Err(RunError::Other(format!("worker panicked: {text}")))
                } else {
                    Err(RunError::Cancelled)
                };
                (id, result)
            }
        };
        let Some((issue_id, run_id)) = self.workers.runs.remove(&task_id) else {
            return;
        };
        self.drain_worker_messages();
        self.drain_codex_events(run_id);
        if !self
            .state
            .running
            .get(&issue_id)
            .is_some_and(|entry| entry.run_id == run_id)
        {
            return;
        }
        let Some(entry) = self.state.running.remove(&issue_id) else {
            return;
        };
        self.record_session_completion_totals(&entry);
        let session_id = entry.session_id.clone().unwrap_or_else(|| "n/a".into());
        let reason = match &result {
            Ok(()) => "normal".to_owned(),
            Err(err) => err.to_string(),
        };
        self.handle_agent_down(&issue_id, entry, result);
        tracing::info!(
            "Agent task finished for issue_id={issue_id} session_id={session_id} reason={reason}"
        );
        self.notify();
    }

    pub(crate) fn handle_agent_down(
        &mut self,
        issue_id: &str,
        mut entry: RunningEntry,
        result: WorkerResult,
    ) {
        let session_id = entry.session_id.clone().unwrap_or_else(|| "n/a".into());
        let outcome_blocker = result.as_ref().err().and_then(RunError::blocker);
        if entry.is_blocker() || outcome_blocker.is_some() {
            let fallback = match (&result, outcome_blocker) {
                (_, Some(blocker)) => blocker.message().to_owned(),
                (Ok(()), None) => "agent exited: normal".to_owned(),
                (Err(err), None) => format!("agent exited: {err}"),
            };
            let error = blocker_error(
                entry.last_codex_event,
                entry.last_codex_message.as_ref(),
                &fallback,
            );
            tracing::warn!(
                "Agent task blocked for issue_id={issue_id} issue_identifier={} session_id={session_id}: {error}",
                entry.identifier
            );
            entry
                .recorder
                .finish(RunStatus::Blocked, Some(error.clone()));
            self.block_issue_from_entry(issue_id, entry, error);
            return;
        }
        let meta = RetryMeta {
            identifier: Some(entry.identifier.clone()),
            issue_url: entry.issue.url.clone(),
            error: None,
            worker_host: entry.worker_host.clone(),
            workspace_path: entry.workspace_path.clone(),
        };
        match result {
            Ok(()) => {
                tracing::info!(
                    "Agent task completed for issue_id={issue_id} session_id={session_id}; scheduling active-state continuation check"
                );
                entry.recorder.finish(RunStatus::Succeeded, None);
                self.state.completed.insert(issue_id.to_owned());
                self.schedule_issue_retry(issue_id, Some(1), meta, DelayType::Continuation);
            }
            Err(err) => {
                let error = format!("agent exited: {err}");
                tracing::warn!(
                    "Agent task exited for issue_id={issue_id} session_id={session_id} reason={err}; scheduling retry"
                );
                entry
                    .recorder
                    .finish(RunStatus::Failed, Some(error.clone()));
                let next = next_retry_attempt_from_running(&entry);
                self.schedule_issue_retry(
                    issue_id,
                    next,
                    RetryMeta {
                        error: Some(error),
                        ..meta
                    },
                    DelayType::Failure,
                );
            }
        }
    }

    pub(crate) fn record_session_completion_totals(&mut self, entry: &RunningEntry) {
        let seconds = Instant::now().duration_since(entry.started).as_secs();
        self.state.codex_totals.seconds_running = self
            .state
            .codex_totals
            .seconds_running
            .saturating_add(seconds);
    }

    pub(crate) fn block_issue_from_entry(
        &mut self,
        issue_id: &str,
        entry: RunningEntry,
        error: String,
    ) {
        self.remove_retry_entry(issue_id);
        self.state.claimed.insert(issue_id.to_owned());
        self.state.blocked.insert(
            issue_id.to_owned(),
            BlockedEntry {
                identifier: entry.identifier,
                issue: Some(entry.issue),
                worker_host: entry.worker_host,
                workspace_path: entry.workspace_path,
                session_id: entry.session_id,
                error,
                blocked_at: Utc::now(),
                last_codex_message: entry.last_codex_message,
                last_codex_event: entry.last_codex_event,
                last_codex_timestamp: entry.last_codex_timestamp,
            },
        );
    }

    /// Cancels a worker and waits until its future has been dropped: cooperative first (the runner
    /// stops Codex and runs `after_run`), abort after the grace period.
    pub(crate) async fn stop_worker(&self, worker: Option<WorkerHandle>) {
        let Some(WorkerHandle {
            cancel,
            abort,
            mut done,
        }) = worker
        else {
            return;
        };
        cancel.cancel();
        let grace = self.ctx.worker_cancel_grace + CANCEL_MARGIN;
        if tokio::time::timeout(grace, &mut done).await.is_err() {
            tracing::warn!(
                "Agent worker did not stop within {}ms of cancellation; aborting",
                grace.as_millis()
            );
            abort.abort();
            if tokio::time::timeout(ABORT_WAIT, done).await.is_err() {
                tracing::warn!("Aborted agent worker was not dropped in time");
            }
        }
    }

    /// `terminate_running_issue/3`: stops the worker (fully, before any cleanup), optionally removes
    /// the workspace, and releases the claim.
    pub(crate) async fn terminate_running_issue(
        &mut self,
        issue_id: &str,
        cleanup: bool,
        reason: &str,
    ) {
        let Some(mut entry) = self.state.running.remove(issue_id) else {
            self.release_issue_claim(issue_id);
            return;
        };
        self.record_session_completion_totals(&entry);
        self.codex_streams.remove(&entry.run_id);
        self.stop_worker(entry.worker.take()).await;
        entry
            .recorder
            .finish(RunStatus::Cancelled, Some(reason.to_owned()));
        if cleanup {
            self.cleanup_issue_workspace(
                Some(&entry.identifier),
                entry.workspace_path.as_deref(),
                entry.worker_host.as_deref(),
            )
            .await;
        }
        self.state.claimed.remove(issue_id);
        self.state.blocked.remove(issue_id);
        self.remove_retry_entry(issue_id);
    }

    pub(crate) async fn stop_all_workers(&mut self) {
        if let Some(timer) = self.state.tick_timer.take() {
            timer.abort();
        }
        for entry in self.state.retry_attempts.values() {
            if let Some(timer) = &entry.timer {
                timer.abort();
            }
        }
        let entries: Vec<RunningEntry> = self.state.running.drain().map(|(_, e)| e).collect();
        let mut recorders = Vec::new();
        let mut stops = Vec::new();
        for mut entry in entries {
            if let Some(worker) = entry.worker.take() {
                worker.cancel.cancel();
                stops.push(worker);
            }
            recorders.push(entry.recorder);
        }
        futures::future::join_all(stops.into_iter().map(|w| self.stop_worker(Some(w)))).await;
        for mut recorder in recorders {
            recorder.finish(RunStatus::Cancelled, Some("shutdown".into()));
        }
    }
}
