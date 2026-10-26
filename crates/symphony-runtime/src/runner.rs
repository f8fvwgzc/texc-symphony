//! The agent runner (`SymphonyElixir.AgentRunner`, B.8): one dispatch = workspace + hooks + one agent
//! session running up to `agent.max_turns` turns on the same thread.
//!
//! Steps: create/reuse the workspace (running `after_create` when new) → report runtime info →
//! `before_run` → start the backend's session (`backends.<name>`, see `symphony_agents`; Codex
//! runs locally through `bash -lc` or over SSH) with the tracker's dynamic tools → turns (turn 1
//! renders the workflow prompt, wrapped by the backend's `prompt_template`; later turns send the
//! continuation guidance) with an issue re-fetch between turns → stop the session → `after_run`.
//!
//! A **task** (task API) differs: no workflow hooks, the workspace starts empty or as a clone of
//! the task's repository, the task prompt is sent as-is, there is exactly one turn, its final
//! agent message is reported as the result, and `timeout_ms` bounds the whole run.
//!
//! `after_run` runs whenever the workspace exists: after success, after a failure, and — unlike Elixir
//! (G4) — also when the orchestrator cancels the run (bounded by the cancellation grace period). It
//! does not run if the worker is aborted outright or panics.
//!
//! With `isolation:` a task run is isolated ([`crate::isolation`]): its own uid, a repository
//! token of its own and a default-deny network, set up before the clone and torn down with the run.

// Modified 2026-10-25: runs every backend through symphony_agents::AgentBackend; task runs (Part B).
// Modified 2026-10-26: task isolation (uid, repository token, egress proxy).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use symphony_agents::{AgentSession, backend_for};
use symphony_codex::{DynamicToolHandler, StartOptions};
use symphony_core::config::{BackendSettings, HooksSettings};
use symphony_core::issue::normalize_state;
use symphony_core::prompt::{build_turn_prompt, render_backend_prompt};
use symphony_core::{Issue, Settings, WorkflowStore};
use symphony_trackers::ToolBinding;
use tracing::Instrument;

use crate::agents;
use crate::isolation::{self, TaskIsolation};
use crate::process::{EnvPolicy, run_captured};
use crate::ssh::{SshConfig, SshLauncher};
use crate::tracker::{FetchError, IssueFetcher, LiveIssueFetcher, TrackerClient};
use crate::worker::{RunError, TaskSpec, WorkerContext, WorkerFactory, WorkerResult};
use crate::workspace::{IssueContext, WorkspaceManager};

/// Dynamic tools backed by a tracker [`ToolBinding`] (snapshot taken at session start).
#[derive(Debug, Clone)]
pub struct TrackerToolHandler {
    binding: ToolBinding,
}

impl TrackerToolHandler {
    /// Wraps `binding`.
    pub fn new(binding: ToolBinding) -> Self {
        Self { binding }
    }
}

#[async_trait]
impl DynamicToolHandler for TrackerToolHandler {
    fn tool_specs(&self) -> Vec<Value> {
        self.binding.tool_specs().to_vec()
    }

    fn secret_environment_names(&self) -> Vec<String> {
        self.binding.secret_environment_names().to_vec()
    }

    async fn execute(&self, tool: Option<&str>, arguments: Value, issue: &Issue) -> Value {
        self.binding
            .execute(tool, &arguments, Some(issue))
            .await
            .to_value()
    }
}

/// Outcome of the between-turns issue check (`continue_with_issue?/2`).
#[derive(Debug, Clone, PartialEq)]
pub enum Continuation {
    /// Still active and routable: run another turn with the refreshed issue.
    Continue(Issue),
    /// Stop (inactive, unroutable, or no longer visible).
    Done(Issue),
}

fn is_active(settings: &Settings, state: Option<&str>) -> bool {
    let Some(state) = state else {
        return false;
    };
    let wanted = normalize_state(state);
    settings
        .active_states()
        .iter()
        .any(|s| normalize_state(s) == wanted)
}

/// `continue_with_issue?/2`: re-fetches the issue; continue only while its state is active **and** it
/// is still routable (`dispatchable` + required labels). A missing issue or id ends the run.
pub async fn continue_with_issue(
    issue: &Issue,
    fetcher: &dyn IssueFetcher,
    settings: &Settings,
) -> Result<Continuation, FetchError> {
    let Some(id) = issue.id.clone() else {
        return Ok(Continuation::Done(issue.clone()));
    };
    let issues = fetcher.fetch_issues_by_ids(&[id]).await?;
    Ok(match issues.into_iter().next() {
        Some(refreshed)
            if is_active(settings, refreshed.state.as_deref())
                && refreshed.routable(&settings.tracker.required_labels) =>
        {
            Continuation::Continue(refreshed)
        }
        Some(refreshed) => Continuation::Done(refreshed),
        None => Continuation::Done(issue.clone()),
    })
}

/// Options for [`AgentRunner`].
#[derive(Debug, Clone)]
pub struct RunnerOptions {
    /// Extra environment for the Codex child (applied before secrets are removed).
    pub codex_env: Vec<(String, String)>,
    /// How long `stop()` waits for the app-server to exit before killing its process group.
    pub codex_stop_grace: Duration,
    /// Whether hooks get the tracker secrets stripped from their environment (default `true`).
    pub strip_hook_secrets: bool,
    /// Override of `agent.max_turns` (tests).
    pub max_turns: Option<u32>,
    /// The `symphony` executable agents use for its helper subcommands (`agent-tools`,
    /// `egress-shim`); default: the running executable.
    pub symphony_executable: Option<PathBuf>,
}

impl Default for RunnerOptions {
    fn default() -> Self {
        Self {
            codex_env: Vec::new(),
            codex_stop_grace: symphony_codex::DEFAULT_STOP_GRACE,
            strip_hook_secrets: true,
            max_turns: None,
            symphony_executable: std::env::current_exe().ok(),
        }
    }
}

/// Bound on cloning a task's repository.
pub const TASK_CLONE_TIMEOUT: Duration = Duration::from_secs(600);

/// Runs one issue end to end (see the module docs).
#[derive(Clone)]
pub struct AgentRunner {
    workflow: Arc<WorkflowStore>,
    tracker: TrackerClient,
    ssh: SshConfig,
    fetcher: Arc<dyn IssueFetcher>,
    options: RunnerOptions,
}

impl std::fmt::Debug for AgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRunner")
            .field("tracker", &self.tracker)
            .field("ssh", &self.ssh)
            .field("options", &self.options)
            .finish()
    }
}

/// The environment policy for hooks under `settings`.
pub fn hook_env_policy(settings: &Settings, strip_secrets: bool) -> EnvPolicy {
    if strip_secrets {
        EnvPolicy::strip(settings.secret_environment_names())
    } else {
        EnvPolicy::inherit()
    }
}

impl AgentRunner {
    /// A runner reading settings from `workflow`, tracker tools/reads through `tracker`, SSH via `ssh`.
    pub fn new(workflow: Arc<WorkflowStore>, tracker: TrackerClient, ssh: SshConfig) -> Self {
        let fetcher = Arc::new(LiveIssueFetcher::new(
            Arc::clone(&workflow),
            tracker.clone(),
        ));
        Self {
            workflow,
            tracker,
            ssh,
            fetcher,
            options: RunnerOptions::default(),
        }
    }

    /// Replaces the between-turns issue fetcher (`:issue_state_fetcher`).
    pub fn with_fetcher(mut self, fetcher: Arc<dyn IssueFetcher>) -> Self {
        self.fetcher = fetcher;
        self
    }

    /// Sets the runner options.
    pub fn with_options(mut self, options: RunnerOptions) -> Self {
        self.options = options;
        self
    }

    fn workspace_manager(&self, settings: &Arc<Settings>) -> WorkspaceManager {
        WorkspaceManager::new(
            Arc::clone(settings),
            self.workflow.workflow_file_path(),
            self.ssh.clone(),
        )
        .with_hook_env(hook_env_policy(settings, self.options.strip_hook_secrets))
    }

    /// `AgentRunner.run/3`.
    pub async fn run(&self, ctx: WorkerContext) -> WorkerResult {
        let span = tracing::info_span!(
            "agent_run",
            issue_id = ctx.issue.id.as_deref(),
            issue_identifier = ctx.issue.identifier.as_deref(),
            run_id = ctx.run_id,
            attempt = ctx.attempt,
            worker_host = ctx.worker_host.as_deref()
        );
        let context = issue_log(&ctx.issue);
        let result = self.run_inner(&ctx).instrument(span.clone()).await;
        match &result {
            Ok(()) => {}
            Err(RunError::Cancelled) => {
                tracing::info!(parent: &span, "Agent run cancelled for {context}");
            }
            Err(reason) => {
                tracing::error!(parent: &span, "Agent run failed for {context}: {reason}");
            }
        }
        result
    }

    async fn run_inner(&self, ctx: &WorkerContext) -> WorkerResult {
        let mut settings = self.workflow.settings();
        if ctx.task.is_some() {
            // Tasks bring their own repository (or none): the workflow's hooks are for issues.
            settings = Arc::new(Settings {
                hooks: HooksSettings::default(),
                ..(*settings).clone()
            });
        }
        let worker_host = ctx.worker_host.as_deref();
        let host_log = worker_host.unwrap_or("local");
        let context = issue_log(&ctx.issue);
        tracing::info!("Starting agent run for {context} worker_host={host_log}");
        tracing::info!("Starting worker attempt for {context} worker_host={host_log}");
        // Isolation applies to task runs only, and a run that cannot be isolated never starts.
        let isolation_settings = ctx.task.as_ref().and(settings.isolation.as_ref());
        if let Some(isolation_settings) = isolation_settings {
            isolation::preflight(isolation_settings, worker_host)?;
        }

        let workspaces = self.workspace_manager(&settings);
        let issue_context = IssueContext::from(&ctx.issue);
        let workspace = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => return Err(RunError::Cancelled),
            created = workspaces.create_for_issue(&issue_context, worker_host) => created?,
        };
        ctx.reporter.runtime_info(worker_host, &workspace);

        let body = async {
            let isolation = match (&ctx.task, isolation_settings) {
                (Some(task), Some(isolation_settings)) => Some(
                    TaskIsolation::prepare(
                        &settings,
                        isolation_settings,
                        task,
                        std::path::Path::new(&workspace),
                    )
                    .await?,
                ),
                _ => None,
            };
            if let Some(task) = &ctx.task {
                let cloned =
                    prepare_task_workspace(&settings, &workspace, task, isolation.as_ref()).await?;
                if let Some(isolation) = &isolation {
                    isolation.finish_workspace(std::path::Path::new(&workspace), cloned)?;
                }
            }
            self.run_in_workspace(ctx, &settings, &workspaces, &workspace, isolation.as_ref())
                .await
        };
        let limit = ctx
            .task
            .as_ref()
            .and_then(|task| task.timeout_ms)
            .map(Duration::from_millis);
        let bounded = async {
            match limit {
                Some(limit) => tokio::time::timeout(limit, body).await.unwrap_or_else(|_| {
                    Err(RunError::TaskTimeout(
                        u64::try_from(limit.as_millis()).unwrap_or(u64::MAX),
                    ))
                }),
                None => body.await,
            }
        };
        let result = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => Err(RunError::Cancelled),
            result = bounded => result,
        };

        // `try ... after Workspace.run_after_run_hook/3` (+ cancellation, see module docs).
        if matches!(result, Err(RunError::Cancelled)) {
            workspaces
                .run_after_run_hook_within(
                    &workspace,
                    &issue_context,
                    worker_host,
                    ctx.cancel_grace,
                )
                .await;
        } else {
            workspaces
                .run_after_run_hook(&workspace, &issue_context, worker_host)
                .await;
        }
        result
    }

    async fn run_in_workspace(
        &self,
        ctx: &WorkerContext,
        settings: &Arc<Settings>,
        workspaces: &WorkspaceManager,
        workspace: &str,
        isolation: Option<&TaskIsolation>,
    ) -> WorkerResult {
        let worker_host = ctx.worker_host.as_deref();
        workspaces
            .run_before_run_hook(workspace, &IssueContext::from(&ctx.issue), worker_host)
            .await?;

        let name = ctx
            .backend
            .clone()
            .unwrap_or_else(|| settings.agent.backend.clone());
        let backend_settings = settings
            .backend(Some(&name))
            .ok_or(RunError::UnknownBackend(name))?;
        let backend = backend_for(&backend_settings, self.options.symphony_executable.clone());
        let binding = self
            .tracker
            .bind_agent_tools(settings)
            .map_err(RunError::ToolBinding)?;
        let mut options = StartOptions::new(
            workspace,
            Arc::clone(settings),
            self.workflow.workflow_file_path(),
        )
        .with_tool_handler(Arc::new(TrackerToolHandler::new(binding)))
        .with_stop_grace(self.options.codex_stop_grace);
        if let Some(host) = worker_host {
            options = options.with_worker_host(
                Some(host.to_owned()),
                Some(SshLauncher::shared(self.ssh.clone())),
            );
        }
        for (name, value) in &self.options.codex_env {
            options = options.with_env(name.clone(), value.clone());
        }
        if let Some(isolation) = isolation {
            options = isolation.apply(options);
        }

        let mut session = backend.start(options).await?;
        // Recorded while it runs, so a Symphony restarted after a hard kill can stop it.
        let _registered = match (worker_host, session.pid()) {
            (None, Some(pid)) => {
                let root = settings.local_workspace_root(&self.workflow.workflow_file_path());
                let dir = agents::registry_dir(&root);
                agents::register(&dir, pid, ctx.issue.identifier.as_deref()).await
            }
            _ => None,
        };
        let result = self
            .run_turns(
                session.as_mut(),
                ctx,
                workspace,
                &backend_settings,
                isolation,
            )
            .await;
        session.stop().await;
        result
    }

    async fn run_turns(
        &self,
        session: &mut dyn AgentSession,
        ctx: &WorkerContext,
        workspace: &str,
        backend: &BackendSettings,
        isolation: Option<&TaskIsolation>,
    ) -> WorkerResult {
        if let Some(task) = &ctx.task {
            let prompt = wrap_prompt(backend, task.prompt.clone(), &ctx.issue, ctx.attempt)?;
            // The task's own token never leaves the run in an event or its result.
            let events = match isolation {
                Some(isolation) => isolation.redacted_events(&ctx.events),
                None => ctx.events.clone(),
            };
            let outcome = match session.run_turn(&prompt, &ctx.issue, &events).await {
                Ok(outcome) => outcome,
                // The run's error becomes the task's `error`: redacted like its events.
                Err(mut err) => {
                    if let Some(redactor) = isolation.and_then(TaskIsolation::redactor) {
                        redactor.redact_error(&mut err);
                    }
                    return Err(err.into());
                }
            };
            tracing::info!(
                "Completed task run for {} session_id={} workspace={workspace}",
                issue_log(&ctx.issue),
                outcome.session_id
            );
            let result = outcome.final_message.as_deref().unwrap_or_default();
            match isolation.and_then(TaskIsolation::redactor) {
                Some(redactor) => ctx.reporter.task_result(&redactor.redact_str(result)),
                None => ctx.reporter.task_result(result),
            }
            return Ok(());
        }
        let max_turns = self
            .options
            .max_turns
            .unwrap_or_else(|| self.workflow.settings().agent.max_turns)
            .max(1);
        let mut issue = ctx.issue.clone();
        let mut turn = 1;
        loop {
            let workflow = self.workflow.current();
            let mut prompt =
                build_turn_prompt(Ok(&workflow), &issue, ctx.attempt, turn, max_turns)?;
            if turn == 1 {
                prompt = wrap_prompt(backend, prompt, &issue, ctx.attempt)?;
            }
            let outcome = session.run_turn(&prompt, &issue, &ctx.events).await?;
            tracing::info!(
                "Completed agent run for {} session_id={} workspace={workspace} turn={turn}/{max_turns}",
                issue_log(&issue),
                outcome.session_id
            );
            let settings = self.workflow.settings();
            match continue_with_issue(&issue, self.fetcher.as_ref(), &settings).await {
                Ok(Continuation::Continue(refreshed)) if turn < max_turns => {
                    tracing::info!(
                        "Continuing agent run for {} after normal turn completion turn={turn}/{max_turns}",
                        issue_log(&refreshed)
                    );
                    issue = refreshed;
                    turn += 1;
                }
                Ok(Continuation::Continue(refreshed)) => {
                    tracing::info!(
                        "Reached agent.max_turns for {} with issue still active; returning control to orchestrator",
                        issue_log(&refreshed)
                    );
                    return Ok(());
                }
                Ok(Continuation::Done(_)) => return Ok(()),
                Err(reason) => return Err(RunError::IssueStateRefresh(reason)),
            }
        }
    }
}

/// Applies the backend's `prompt_template` (when it has one) around `prompt`.
fn wrap_prompt(
    backend: &BackendSettings,
    prompt: String,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, RunError> {
    match &backend.prompt_template {
        Some(template) => Ok(render_backend_prompt(template, &prompt, issue, attempt)?),
        None => Ok(prompt),
    }
}

/// Clones the task's repository into its fresh workspace (nothing to do without a repository or
/// when the workspace already holds files from an earlier run of the task); `true` when it cloned.
///
/// Under isolation the clone runs as the task uid with `HOME=<workspace>/.home`, and a repository
/// on the GitHub App's host is cloned over https with the task's token (from the environment).
async fn prepare_task_workspace(
    settings: &Settings,
    workspace: &str,
    task: &TaskSpec,
    isolation: Option<&TaskIsolation>,
) -> Result<bool, RunError> {
    let Some(repo) = &task.repo else {
        return Ok(false);
    };
    if !symphony_core::task::valid_repo_url(repo)
        || task
            .branch
            .as_deref()
            .is_some_and(|b| !symphony_core::task::valid_branch(b))
    {
        return Err(RunError::TaskClone("repository or branch refused".into()));
    }
    let mut entries = tokio::fs::read_dir(workspace)
        .await
        .map_err(|err| RunError::TaskClone(err.to_string()))?;
    if entries
        .next_entry()
        .await
        .map_err(|err| RunError::TaskClone(err.to_string()))?
        .is_some()
    {
        return Ok(false);
    }
    let credentials = isolation.and_then(TaskIsolation::credentials);
    let url = credentials.map_or_else(|| repo.clone(), |c| c.clone_url());
    let mut command = tokio::process::Command::new("git");
    // Only network transports: `ext::`, `file://` and friends stay disabled.
    command
        .args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "clone",
            "--depth",
            "1",
        ])
        .current_dir(workspace)
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Some(branch) = &task.branch {
        command.args(["--branch", branch]);
    }
    command.args(["--", url.as_str(), "."]);
    EnvPolicy::strip(settings.secret_environment_names()).apply(&mut command);
    if let Some(isolation) = isolation {
        isolation.apply_to_command(&mut command);
    }
    let output = run_captured(command, TASK_CLONE_TIMEOUT)
        .await
        .map_err(|err| RunError::TaskClone(err.to_string()))?;
    if output.status != 0 {
        let tail: String = output
            .output
            .chars()
            .rev()
            .take(500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let tail = match isolation.and_then(TaskIsolation::redactor) {
            Some(redactor) => redactor.redact_str(tail.trim()).into_owned(),
            None => tail.trim().to_owned(),
        };
        return Err(RunError::TaskClone(format!(
            "git clone exited {}: {tail}",
            output.status
        )));
    }
    Ok(true)
}

fn issue_log(issue: &Issue) -> String {
    format!(
        "issue_id={} issue_identifier={}",
        issue.id.as_deref().unwrap_or(""),
        issue.identifier.as_deref().unwrap_or("")
    )
}

/// The production [`WorkerFactory`]: every dispatch runs an [`AgentRunner`].
#[derive(Debug, Clone)]
pub struct CodexWorkerFactory {
    runner: AgentRunner,
}

impl CodexWorkerFactory {
    /// Wraps `runner`.
    pub fn new(runner: AgentRunner) -> Self {
        Self { runner }
    }
}

impl WorkerFactory for CodexWorkerFactory {
    fn start(&self, ctx: WorkerContext) -> BoxFuture<'static, WorkerResult> {
        let runner = self.runner.clone();
        Box::pin(async move { runner.run(ctx).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_env_strips_the_tracker_secrets_unless_disabled() {
        let mut settings = Settings::default();
        settings.tracker.kind = Some("github".into());
        let policy = hook_env_policy(&settings, true);
        assert!(policy.removed().iter().any(|n| n == "GITHUB_TOKEN"));
        assert!(policy.removed().iter().any(|n| n == "GH_TOKEN"));
        assert!(hook_env_policy(&settings, false).removed().is_empty());
        // No tracker secrets; the LLM contract's are always stripped.
        settings.tracker.kind = Some("memory".into());
        assert_eq!(
            hook_env_policy(&settings, true).removed(),
            symphony_core::config::llm::LLM_SECRET_ENV_NAMES.map(str::to_owned)
        );
    }
}
