//! Poll cycle: preflight, candidate fetch, revalidation and dispatch (B.2.7, B.4).

// Modified 2026-10-25: budget gating, redaction and pricing at dispatch (audit A1/A4).

use super::*;

impl Orchestrator<'_> {
    /// `maybe_dispatch/1`: reconcile, validate config, fetch candidates, dispatch in priority order.
    pub(crate) async fn maybe_dispatch(&mut self) {
        self.enforce_spend().await;
        self.reconcile_running_issues().await;
        self.reconcile_blocked_issues().await;
        if let Err(err) = self.ctx.workflow.force_reload() {
            log_preflight_error(&err);
            return;
        }
        self.dispatch_tasks().await;
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
        if let Some(stop) = self.state.spend.global_stop(&settings.budget, Utc::now()) {
            tracing::info!("Not dispatching new work: {}", stop.reason());
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
        if let Some(stop) = self.spend_stop_for(&issue).await {
            if let (SpendStop::IssueExhausted(reason), Some(id)) = (&stop, issue.id.as_deref()) {
                tracing::warn!("Blocking {}: {reason}", issue_context(&issue));
                self.block_for_budget(id, &issue, reason.clone());
            }
            return;
        }
        let Some(issue_id) = issue.id.clone() else {
            return;
        };
        if !self.take_lease(&issue_id).await {
            return;
        }
        if let Revalidation::Ok(refreshed) = self.refresh_issue_for_dispatch(&issue).await {
            self.do_dispatch_issue(refreshed, attempt, preferred);
        }
        // Lease held ⇔ claimed.
        if !self.state.claimed.contains(&issue_id) {
            self.drop_lease(&issue_id);
        }
    }

    /// What stops `issue` from being dispatched now, after loading its stored usage once.
    pub(crate) async fn spend_stop_for(&mut self, issue: &Issue) -> Option<SpendStop> {
        let issue_id = issue.id.as_deref()?;
        let budget = &self.settings().budget;
        let per_issue =
            budget.max_tokens_per_issue.is_some() || budget.max_cost_usd_per_issue.is_some();
        if per_issue && !self.state.spend.knows_issue(issue_id) {
            let usage = match self.ctx.store.issue_usage(issue_id).await {
                Ok(usage) => Usage {
                    tokens: usage.tokens.processed(),
                    cost_micro_usd: usage.cost_micro_usd,
                },
                Err(err) => {
                    tracing::warn!("Failed to load the usage of issue_id={issue_id}: {err}");
                    Usage::default()
                }
            };
            self.state.spend.seed_issue(issue_id, usage);
        }
        self.state.spend.stop_for(issue_id, budget, Utc::now())
    }

    /// Blocks a not-running issue whose budget is used up (it stays visible in `blocked`).
    pub(crate) fn block_for_budget(&mut self, issue_id: &str, issue: &Issue, error: String) {
        self.remove_retry_entry(issue_id);
        self.state.claimed.insert(issue_id.to_owned());
        self.state.blocked.insert(
            issue_id.to_owned(),
            BlockedEntry {
                identifier: issue
                    .identifier
                    .clone()
                    .unwrap_or_else(|| issue_id.to_owned()),
                issue: Some(issue.clone()),
                worker_host: None,
                workspace_path: None,
                session_id: None,
                error,
                blocked_at: Utc::now(),
                last_codex_message: None,
                last_codex_event: None,
                last_codex_timestamp: None,
            },
        );
        self.notify();
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
                self.spawn_issue_on_worker_host(issue, attempt, choice.host(), RunPlan::default());
                true
            }
        }
    }

    /// Starts queued tasks from the task API while slots are free (local runs, oldest first).
    pub(crate) async fn dispatch_tasks(&mut self) {
        if !self.ctx.store.is_enabled() {
            return;
        }
        let settings = self.settings();
        let slots = self.state.available_slots();
        if slots == 0
            || self
                .state
                .spend
                .global_stop(&settings.budget, Utc::now())
                .is_some()
        {
            return;
        }
        let queued = match self
            .ctx
            .store
            .queued_tasks(u32::try_from(slots).unwrap_or(u32::MAX))
            .await
        {
            Ok(queued) => queued,
            Err(err) => {
                tracing::warn!("Failed to read the task queue: {err}");
                return;
            }
        };
        for task in queued {
            if self.state.available_slots() == 0 {
                break;
            }
            let issue_id = task_issue_id(task.id);
            if self.state.running.contains_key(&issue_id) || self.state.claimed.contains(&issue_id)
            {
                continue;
            }
            let issue = task_issue(&task);
            if !self.take_lease(&issue_id).await {
                continue;
            }
            let Ok(Some(task)) = self.ctx.store.start_task(task.id).await else {
                self.drop_lease(&issue_id);
                continue;
            };
            let refusal = if settings.backend(task.backend.as_deref()).is_none() {
                Some(format!(
                    "unknown backend {:?}",
                    task.backend.as_deref().unwrap_or(&settings.agent.backend)
                ))
            } else {
                self.task_spend_stop(&issue, task.limits.max_tokens).await
            };
            if let Some(error) = refusal {
                tracing::warn!("Not running {}: {error}", issue_context(&issue));
                self.task_finish(task.id, TaskStatus::Failed, None, Some(error));
                self.drop_lease(&issue_id);
                continue;
            }
            let plan = RunPlan {
                backend: task.backend.clone(),
                task: Some((
                    TaskSpec {
                        id: task.id,
                        prompt: task.prompt.clone(),
                        repo: task.repo.clone(),
                        branch: task.branch.clone(),
                        timeout_ms: task.limits.timeout_ms,
                    },
                    task.limits.max_tokens,
                )),
            };
            self.spawn_issue_on_worker_host(issue, None, None, plan);
        }
    }

    /// Why a task may not start: its own token limit or the per-issue budget is used up (history
    /// is loaded first). The daily budget and the kill switch are checked by the caller.
    pub(crate) async fn task_spend_stop(
        &mut self,
        issue: &Issue,
        max_tokens: Option<u64>,
    ) -> Option<String> {
        let issue_id = issue.id.as_deref()?;
        if !self.state.spend.knows_issue(issue_id) {
            let usage = self
                .ctx
                .store
                .issue_usage(issue_id)
                .await
                .unwrap_or_default();
            self.state.spend.seed_issue(
                issue_id,
                Usage {
                    tokens: usage.tokens.processed(),
                    cost_micro_usd: usage.cost_micro_usd,
                },
            );
        }
        if let Some(limit) = max_tokens {
            let used = self.state.spend.issue(issue_id).tokens;
            if used >= limit {
                return Some(format!(
                    "task token limit exhausted: {used} of {limit} tokens used"
                ));
            }
        }
        self.state
            .spend
            .issue_stop(issue_id, &self.settings().budget)
            .map(|stop| stop.reason().to_owned())
    }

    pub(crate) fn spawn_issue_on_worker_host(
        &mut self,
        issue: Issue,
        attempt: Option<u32>,
        worker_host: Option<String>,
        plan: RunPlan,
    ) {
        let Some(issue_id) = issue.id.clone() else {
            return;
        };
        let identifier = issue.identifier.clone().unwrap_or_else(|| issue_id.clone());
        let run_id = self.tokens.next();
        let (codex_tx, codex_rx) = mpsc::unbounded_channel();
        let (done_tx, done_rx) = oneshot::channel::<()>();
        let cancel = self.shutdown.child_token();
        let cancel_for_watch = cancel.clone();
        let ctx = WorkerContext {
            issue: issue.clone(),
            attempt,
            worker_host: worker_host.clone(),
            run_id,
            events: EventSink::new(codex_tx),
            reporter: crate::worker::WorkerReporter::new(run_id, self.worker_tx.clone()),
            cancel: cancel.clone(),
            cancel_grace: self.ctx.worker_cancel_grace,
            backend: plan.backend.clone(),
            task: plan.task.as_ref().map(|(spec, _)| spec.clone()),
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
            Arc::new(Redactor::from_settings(&self.settings(), &ProcessEnv)),
        );
        let settings = self.settings();
        let pricing = Pricing::for_backend(
            settings.backend(plan.backend.as_deref()).as_ref(),
            &settings.budget,
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
                pricing,
                cost_micro_usd: 0,
                task: plan.task.map(|(spec, max_tokens)| TaskRun {
                    id: spec.id,
                    max_tokens,
                    result: None,
                }),
            },
        );
        self.state.claimed.insert(issue_id.clone());
        // Watch this run's lease so the watchdog can cancel it near expiry even if the actor loop
        // parks; the lease (shared stores only) was taken just before this spawn.
        self.watch_lease(&issue_id, &cancel_for_watch);
        self.remove_retry_entry(&issue_id);
    }
}

/// What a run executes besides its issue: another backend, or a task.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunPlan {
    pub backend: Option<String>,
    /// The task and its token limit.
    pub task: Option<(TaskSpec, Option<u64>)>,
}

/// The issue a task's runs are recorded under (`task:<id>` / `task-<id>`).
pub(crate) fn task_issue(task: &TaskRecord) -> Issue {
    let title = task.title.clone().unwrap_or_else(|| {
        let first = task.prompt.lines().next().unwrap_or_default();
        first.chars().take(80).collect()
    });
    Issue {
        id: Some(task_issue_id(task.id)),
        identifier: Some(task.identifier.clone()),
        title: Some(title),
        description: Some(task.prompt.clone()),
        state: Some("task".into()),
        dispatchable: true,
        created_at: Some(task.created_at),
        updated_at: Some(task.updated_at),
        ..Issue::default()
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
