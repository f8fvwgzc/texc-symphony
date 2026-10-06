//! The agent runner (`SymphonyElixir.AgentRunner`, B.8): one dispatch = workspace + hooks + one Codex
//! session running up to `agent.max_turns` turns on the same thread.
//!
//! Steps: create/reuse the workspace (running `after_create` when new) → report runtime info →
//! `before_run` → start the app-server (local `bash -lc` or SSH) with the tracker's dynamic tools →
//! turns (turn 1 renders the workflow prompt, later turns send the continuation guidance) with an
//! issue re-fetch between turns → stop the session → `after_run`.
//!
//! `after_run` runs whenever the workspace exists: after success, after a failure, and — unlike Elixir
//! (G4) — also when the orchestrator cancels the run (bounded by the cancellation grace period). It
//! does not run if the worker is aborted outright or panics.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use symphony_codex::{AppServerSession, DynamicToolHandler, StartOptions};
use symphony_core::issue::normalize_state;
use symphony_core::prompt::build_turn_prompt;
use symphony_core::{Issue, Settings, WorkflowStore};
use symphony_trackers::ToolBinding;
use tracing::Instrument;

use crate::agents;
use crate::process::EnvPolicy;
use crate::ssh::{SshConfig, SshLauncher};
use crate::tracker::{FetchError, IssueFetcher, LiveIssueFetcher, TrackerClient};
use crate::worker::{RunError, WorkerContext, WorkerFactory, WorkerResult};
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
}

impl Default for RunnerOptions {
    fn default() -> Self {
        Self {
            codex_env: Vec::new(),
            codex_stop_grace: symphony_codex::DEFAULT_STOP_GRACE,
            strip_hook_secrets: true,
            max_turns: None,
        }
    }
}

/// OS pid of a session's app-server process.
fn local_agent_pid(session: &AppServerSession) -> Option<u32> {
    session.codex_app_server_pid()?.parse().ok()
}

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
        let settings = self.workflow.settings();
        let worker_host = ctx.worker_host.as_deref();
        let host_log = worker_host.unwrap_or("local");
        let context = issue_log(&ctx.issue);
        tracing::info!("Starting agent run for {context} worker_host={host_log}");
        tracing::info!("Starting worker attempt for {context} worker_host={host_log}");

        let workspaces = self.workspace_manager(&settings);
        let issue_context = IssueContext::from(&ctx.issue);
        let workspace = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => return Err(RunError::Cancelled),
            created = workspaces.create_for_issue(&issue_context, worker_host) => created?,
        };
        ctx.reporter.runtime_info(worker_host, &workspace);

        let body = self.run_in_workspace(ctx, &settings, &workspaces, &workspace);
        let result = tokio::select! {
            biased;
            () = ctx.cancel.cancelled() => Err(RunError::Cancelled),
            result = body => result,
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
    ) -> WorkerResult {
        let worker_host = ctx.worker_host.as_deref();
        workspaces
            .run_before_run_hook(workspace, &IssueContext::from(&ctx.issue), worker_host)
            .await?;

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

        let mut session = AppServerSession::start(options).await?;
        // Recorded while it runs, so a Symphony restarted after a hard kill can stop it.
        let _registered = match (worker_host, local_agent_pid(&session)) {
            (None, Some(pid)) => {
                let root = settings.local_workspace_root(&self.workflow.workflow_file_path());
                let dir = agents::registry_dir(&root);
                agents::register(&dir, pid, ctx.issue.identifier.as_deref()).await
            }
            _ => None,
        };
        let result = self.run_turns(&mut session, ctx, workspace).await;
        session.stop().await;
        result
    }

    async fn run_turns(
        &self,
        session: &mut AppServerSession,
        ctx: &WorkerContext,
        workspace: &str,
    ) -> WorkerResult {
        let max_turns = self
            .options
            .max_turns
            .unwrap_or_else(|| self.workflow.settings().agent.max_turns)
            .max(1);
        let mut issue = ctx.issue.clone();
        let mut turn = 1;
        loop {
            let workflow = self.workflow.current();
            let prompt = build_turn_prompt(Ok(&workflow), &issue, ctx.attempt, turn, max_turns)?;
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
        settings.tracker.kind = Some("memory".into());
        assert!(hook_env_policy(&settings, true).removed().is_empty());
    }
}
