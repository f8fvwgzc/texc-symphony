//! Poll cycle: preflight, candidate fetch, revalidation and dispatch (B.2.7, B.4).

use super::*;

impl Orchestrator<'_> {
    /// `maybe_dispatch/1`: reconcile, validate config, fetch candidates, dispatch in priority order.
    pub(crate) async fn maybe_dispatch(&mut self) {
        self.reconcile_running_issues().await;
        self.reconcile_blocked_issues().await;
        if let Err(err) = self.ctx.workflow.force_reload() {
            log_preflight_error(&err);
            return;
        }
        let settings = self.settings();
        let issues = match self
            .ctx
            .tracker
            .fetch_issues_by_states(&settings, settings.active_states())
            .await
        {
            Ok(issues) => issues,
            Err(err) => {
                log_fetch_error(&err);
                return;
            }
        };
        if self.state.available_slots() == 0 {
            return;
        }
        let sets = StateSets::from_settings(&settings);
        for issue in sort_issues_for_dispatch(issues) {
            if self.state.should_dispatch(&issue, &settings, &sets) {
                self.dispatch_issue(issue, None, None).await;
            }
        }
    }

    pub(crate) async fn dispatch_issue(
        &mut self,
        issue: Issue,
        attempt: Option<u32>,
        preferred: Option<&str>,
    ) {
        if let Revalidation::Ok(refreshed) = self.refresh_issue_for_dispatch(&issue).await {
            self.do_dispatch_issue(refreshed, attempt, preferred);
        }
    }

    /// `revalidate_issue_for_dispatch/3`: one `fetch_issues_by_ids([id])` right before dispatch.
    pub(crate) async fn revalidate_issue_for_dispatch(&self, issue: &Issue) -> Revalidation {
        let Some(id) = issue.id.clone() else {
            return Revalidation::Ok(issue.clone());
        };
        let settings = self.settings();
        match self.ctx.tracker.fetch_issues_by_ids(&settings, &[id]).await {
            Err(err) => Revalidation::Error(err),
            Ok(issues) => match issues.into_iter().next() {
                None => Revalidation::SkipMissing,
                Some(first) => {
                    let sets = StateSets::from_settings(&settings);
                    if candidate_issue(&first, &sets, &settings) {
                        Revalidation::Ok(first)
                    } else {
                        Revalidation::Skip(first)
                    }
                }
            },
        }
    }

    pub(crate) async fn refresh_issue_for_dispatch(&self, issue: &Issue) -> Revalidation {
        let result = self.revalidate_issue_for_dispatch(issue).await;
        match &result {
            Revalidation::Ok(_) => {}
            Revalidation::SkipMissing => tracing::info!(
                "Skipping dispatch; issue no longer active or visible: {}",
                issue_context(issue)
            ),
            Revalidation::Skip(refreshed) => tracing::info!(
                "Skipping stale dispatch after issue refresh: {} state={:?} blocked_by={}",
                issue_context(refreshed),
                refreshed.state,
                refreshed.blocked_by.len()
            ),
            Revalidation::Error(err) => tracing::warn!(
                "Skipping dispatch; issue refresh failed for {}: {err}",
                issue_context(issue)
            ),
        }
        result
    }

    /// `do_dispatch_issue/4`: picks the host and spawns. Returns `false` when no SSH host has capacity.
    pub(crate) fn do_dispatch_issue(
        &mut self,
        issue: Issue,
        attempt: Option<u32>,
        preferred: Option<&str>,
    ) -> bool {
        let settings = self.settings();
        match self.state.select_worker_host(&settings, preferred) {
            HostChoice::NoCapacity => {
                tracing::debug!(
                    "No SSH worker slots available for {} preferred_worker_host={preferred:?}",
                    issue_context(&issue)
                );
                false
            }
            choice => {
                self.spawn_issue_on_worker_host(issue, attempt, choice.host());
                true
            }
        }
    }

    pub(crate) fn spawn_issue_on_worker_host(
        &mut self,
        issue: Issue,
        attempt: Option<u32>,
        worker_host: Option<String>,
    ) {
        let Some(issue_id) = issue.id.clone() else {
            return;
        };
        let identifier = issue.identifier.clone().unwrap_or_else(|| issue_id.clone());
        let run_id = self.tokens.next();
        let (codex_tx, codex_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let cancel = self.shutdown.child_token();
        let ctx = WorkerContext {
            issue: issue.clone(),
            attempt,
            worker_host: worker_host.clone(),
            run_id,
            events: EventSink::new(codex_tx),
            reporter: crate::worker::WorkerReporter::new(run_id, self.worker_tx.clone()),
            cancel: cancel.clone(),
            cancel_grace: self.ctx.worker_cancel_grace,
        };
        let future = self.ctx.factory.start(ctx);
        let span = tracing::info_span!(
            "worker",
            issue_id = %issue_id,
            issue_identifier = %identifier,
            run_id,
            attempt,
            worker_host = worker_host.as_deref().unwrap_or("local")
        );
        let abort = self.workers.set.spawn(
            async move {
                let _done = done_tx;
                future.await
            }
            .instrument(span),
        );
        self.workers
            .runs
            .insert(abort.id(), (issue_id.clone(), run_id));
        self.codex_streams
            .insert(run_id, UnboundedReceiverStream::new(codex_rx));
        tracing::info!(
            "Dispatching issue to agent: {} run_id={run_id} attempt={attempt:?} worker_host={}",
            issue_context(&issue),
            worker_host.as_deref().unwrap_or("local")
        );
        let started_at = Utc::now();
        let recorder = RunRecorder::start(
            &self.ctx.store,
            NewRun {
                issue_id: issue_id.clone(),
                issue_identifier: identifier.clone(),
                issue_title: issue.title.clone(),
                attempt: attempt.unwrap_or(0),
                worker_host: worker_host.clone(),
                workspace_path: None,
                started_at: Some(started_at),
            },
        );
        let now = Instant::now();
        self.state.running.insert(
            issue_id.clone(),
            RunningEntry {
                run_id,
                worker: Some(WorkerHandle {
                    cancel,
                    abort,
                    done: done_rx,
                }),
                identifier,
                issue,
                worker_host,
                workspace_path: None,
                session_id: None,
                last_codex_message: None,
                last_codex_timestamp: None,
                last_codex_event: None,
                last_activity: now,
                codex_app_server_pid: None,
                tokens: Default::default(),
                turn_count: 0,
                retry_attempt: attempt.filter(|a| *a > 0).unwrap_or(0),
                started_at,
                started: now,
                recorder,
            },
        );
        self.state.claimed.insert(issue_id.clone());
        self.remove_retry_entry(&issue_id);
    }
}

fn log_config_error(err: &ConfigError) -> String {
    match err {
        ConfigError::Tracker(TrackerConfigError::MissingLinearApiToken) => {
            "Tracker API token missing in WORKFLOW.md".into()
        }
        ConfigError::Tracker(TrackerConfigError::MissingLinearProjectSlug) => {
            "Tracker project scope missing in WORKFLOW.md".into()
        }
        ConfigError::MissingTrackerKind => "Tracker kind missing in WORKFLOW.md".into(),
        ConfigError::UnsupportedTrackerKind(kind) => {
            format!("Unsupported tracker kind in WORKFLOW.md: {kind:?}")
        }
        ConfigError::InvalidWorkflowConfig(message) => {
            format!("Invalid WORKFLOW.md config: {message}")
        }
        ConfigError::MissingWorkflowFile { path, reason } => {
            // Elixir logs `inspect(reason)`, i.e. the atom form `:enoent` (matches ConfigError::user_message).
            format!("Missing WORKFLOW.md at {}: :{reason}", path.display())
        }
        ConfigError::WorkflowFrontMatterNotAMap => {
            "Failed to parse WORKFLOW.md: workflow front matter must decode to a map".into()
        }
        ConfigError::WorkflowParseError(reason) => format!("Failed to parse WORKFLOW.md: {reason}"),
        other => format!("Failed to fetch from issue tracker: {other}"),
    }
}

fn log_preflight_error(err: &ConfigError) {
    tracing::error!("{}", log_config_error(err));
}

fn log_fetch_error(err: &FetchError) {
    let message = match err {
        FetchError::Tracker(TrackerError::Config(config)) => {
            log_config_error(&ConfigError::Tracker(*config))
        }
        FetchError::Tracker(TrackerError::MissingTrackerKind) => {
            log_config_error(&ConfigError::MissingTrackerKind)
        }
        FetchError::Tracker(TrackerError::UnsupportedTrackerKind(kind)) => {
            log_config_error(&ConfigError::UnsupportedTrackerKind(kind.clone()))
        }
        other => format!("Failed to fetch from issue tracker: {other}"),
    };
    tracing::error!("{message}");
}
