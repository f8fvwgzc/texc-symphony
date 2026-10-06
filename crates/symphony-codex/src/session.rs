//! `AppServerSession`: handshake, turns and shutdown (`Codex.AppServer.start_session/run_turn/stop_session`).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use symphony_core::config::CodexSettings;
use symphony_core::path_safety;
use symphony_core::{Issue, Settings, WorkflowStore};
use tokio::process::Command;
use tracing::Instrument;

use crate::dynamic_tool::{
    DynamicToolHandler, NoDynamicTools, normalize_tool_result, tool_call_arguments, tool_call_name,
    tool_response,
};
use crate::error::CodexError;
use crate::event::{CodexEvent, CodexEventData, EventSink, StreamMessage};
use crate::launch::{
    RemoteLauncher, codex_command, find_executable, local_launch_command, remote_launch_command,
    valid_secret_names, validate_local_workspace, validate_remote_workspace,
};
use crate::protocol::{
    self, DECISION_APPROVE_THIS_SESSION, INITIALIZE_ID, ResponseMatch, THREAD_START_ID,
    TURN_START_ID,
};
use crate::transport::{Incoming, StreamLabel, Transport, log_non_json_stream_line};

/// Default time [`AppServerSession::stop`] waits for the app-server to exit on stdin EOF before
/// killing its process group.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(2);

/// Server messages buffered while a response is awaited (beyond this they are dropped with a warning).
pub const MAX_PENDING_MESSAGES: usize = 1_024;

/// Inputs for [`AppServerSession::start`].
pub struct StartOptions {
    /// Issue workspace path (local: must be strictly inside the workspace root; remote: used verbatim).
    pub workspace: String,
    /// Remote worker (`host` or `host:port`); `None` runs Codex locally.
    pub worker_host: Option<String>,
    /// Settings snapshot used for the whole session (command, policies, timeouts).
    pub settings: Arc<Settings>,
    /// The `WORKFLOW.md` path: relative `workspace.root` values resolve against its directory.
    pub workflow_path: PathBuf,
    /// Dynamic tools advertised to and executed for Codex (snapshot for the session).
    pub tool_handler: Arc<dyn DynamicToolHandler>,
    /// Builds the SSH process for remote workers (required when `worker_host` is set).
    pub remote_launcher: Option<Arc<dyn RemoteLauncher>>,
    /// Extra environment for the child, applied before secret variables are removed.
    pub env: Vec<(String, String)>,
    /// How long [`AppServerSession::stop`] waits for a clean exit before killing the process group.
    pub stop_grace: Duration,
}

impl StartOptions {
    /// Options for a local session without dynamic tools.
    pub fn new(
        workspace: impl Into<String>,
        settings: Arc<Settings>,
        workflow_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            worker_host: None,
            settings,
            workflow_path: workflow_path.into(),
            tool_handler: Arc::new(NoDynamicTools),
            remote_launcher: None,
            env: Vec::new(),
            stop_grace: DEFAULT_STOP_GRACE,
        }
    }

    /// Options from the current snapshot of a [`WorkflowStore`].
    pub fn from_store(store: &WorkflowStore, workspace: impl Into<String>) -> Self {
        Self::new(workspace, store.settings(), store.workflow_file_path())
    }

    /// Runs Codex on a remote worker through `launcher`.
    pub fn with_worker_host(
        mut self,
        worker_host: Option<String>,
        launcher: Option<Arc<dyn RemoteLauncher>>,
    ) -> Self {
        self.worker_host = worker_host;
        self.remote_launcher = launcher;
        self
    }

    /// Sets the dynamic tool handler.
    pub fn with_tool_handler(mut self, handler: Arc<dyn DynamicToolHandler>) -> Self {
        self.tool_handler = handler;
        self
    }

    /// Adds one environment variable for the child.
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// Sets the shutdown grace period.
    pub fn with_stop_grace(mut self, grace: Duration) -> Self {
        self.stop_grace = grace;
        self
    }
}

/// A completed turn (`%{result: :turn_completed, session_id, thread_id, turn_id}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnOutcome {
    /// `"<thread_id>-<turn_id>"`.
    pub session_id: String,
    /// Thread id (stable across the session's turns).
    pub thread_id: String,
    /// Turn id.
    pub turn_id: String,
}

/// What a stream line means for the turn loop.
enum Step {
    Continue,
    Done(Result<(), CodexError>),
}

/// A running Codex app-server with a started thread.
pub struct AppServerSession {
    transport: Transport,
    pid: Option<String>,
    worker_host: Option<String>,
    approval_policy: Value,
    auto_approve_requests: bool,
    thread_sandbox: String,
    turn_sandbox_policy: Value,
    thread_id: String,
    workspace: String,
    codex: CodexSettings,
    tool_handler: Arc<dyn DynamicToolHandler>,
    stop_grace: Duration,
    /// Server messages that arrived while a response was awaited, replayed by the next turn loop.
    pending: VecDeque<StreamMessage>,
}

fn issue_context(issue: &Issue) -> String {
    format!(
        "issue_id={} issue_identifier={}",
        issue.id.as_deref().unwrap_or(""),
        issue.identifier.as_deref().unwrap_or("")
    )
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl AppServerSession {
    /// Validates the workspace, resolves policies, launches the app-server and performs the handshake
    /// (`initialize` → `initialized` → `thread/start`). On failure after launch the process is stopped.
    /// No events are emitted here (Elixir parity).
    pub async fn start(options: StartOptions) -> Result<Self, CodexError> {
        let StartOptions {
            workspace,
            worker_host,
            settings,
            workflow_path,
            tool_handler,
            remote_launcher,
            env,
            stop_grace,
        } = options;
        let span = tracing::info_span!("codex_session", worker_host = worker_host.as_deref());

        let workflow_file = path_safety::expand_path(&workflow_path, None);
        let base_dir = workflow_file.parent();
        let workspace = match &worker_host {
            None => {
                let root = settings.local_workspace_root(&workflow_path);
                lossy(
                    &validate_local_workspace(&workspace, &root)
                        .map_err(CodexError::InvalidWorkspaceCwd)?,
                )
            }
            Some(host) => validate_remote_workspace(&workspace, host)
                .map_err(CodexError::InvalidWorkspaceCwd)?,
        };
        // Resolved before launching (Elixir resolved it after `Port.open` and then closed the port).
        let runtime = settings
            .codex_runtime_settings(Some(&workspace), worker_host.is_some(), base_dir)
            .map_err(CodexError::SandboxPolicy)?;

        let codex = settings.codex.clone();
        let secret_names = valid_secret_names(
            settings
                .secret_environment_names()
                .into_iter()
                .chain(tool_handler.secret_environment_names()),
        );
        let command = match &worker_host {
            None => {
                let bash = find_executable("bash").ok_or(CodexError::BashNotFound)?;
                let mut command = Command::new(bash);
                command
                    .arg("-lc")
                    .arg(local_launch_command(&codex_command(&codex), &secret_names))
                    .current_dir(&workspace);
                command
            }
            Some(host) => {
                let launcher = remote_launcher
                    .as_ref()
                    .ok_or_else(|| CodexError::RemoteLauncherMissing(host.clone()))?;
                launcher.command(
                    host,
                    &remote_launch_command(&workspace, &codex_command(&codex), &secret_names),
                )?
            }
        };
        // Secrets are stripped from the local `ssh` process too (Elixir inherited the full env there).
        let transport = Transport::spawn(command, &secret_names, &env)?;
        let pid = transport.pid().map(|pid| pid.to_string());
        tracing::debug!(parent: &span, pid = ?pid, workspace = %workspace, "Codex app-server launched");

        let approval_policy = runtime.approval_policy.to_value();
        let mut session = Self {
            transport,
            pid,
            worker_host,
            auto_approve_requests: approval_policy == Value::String("never".into()),
            approval_policy,
            thread_sandbox: runtime.thread_sandbox,
            turn_sandbox_policy: Value::Object(runtime.turn_sandbox_policy),
            thread_id: String::new(),
            workspace,
            codex,
            tool_handler,
            stop_grace,
            pending: VecDeque::new(),
        };
        match session.handshake().instrument(span).await {
            Ok(thread_id) => {
                session.thread_id = thread_id;
                Ok(session)
            }
            Err(reason) => {
                session.stop().await;
                Err(reason)
            }
        }
    }

    async fn handshake(&mut self) -> Result<String, CodexError> {
        self.transport.send(&protocol::initialize_request()).await;
        self.await_response(INITIALIZE_ID).await?;
        self.transport
            .send(&protocol::initialized_notification())
            .await;
        let request = protocol::thread_start_request(
            &self.approval_policy,
            &self.thread_sandbox,
            &self.workspace,
            self.tool_handler.tool_specs(),
        );
        self.transport.send(&request).await;
        let result = self.await_response(THREAD_START_ID).await?;
        protocol::thread_id_from_result(&result).map_err(CodexError::InvalidThreadPayload)
    }

    /// Thread id from `thread/start`.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The cwd sent to Codex (canonical local path, or the remote path verbatim).
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// Remote worker host, if any.
    pub fn worker_host(&self) -> Option<&str> {
        self.worker_host.as_deref()
    }

    /// OS pid of the app-server (the local `ssh` pid for remote sessions), as a string.
    pub fn codex_app_server_pid(&self) -> Option<&str> {
        self.pid.as_deref()
    }

    /// The approval policy sent in `thread/start` and `turn/start`.
    pub fn approval_policy(&self) -> &Value {
        &self.approval_policy
    }

    /// `approval_policy == "never"`: approvals and MCP tool prompts are answered automatically.
    pub fn auto_approve_requests(&self) -> bool {
        self.auto_approve_requests
    }

    /// `thread/start.sandbox`.
    pub fn thread_sandbox(&self) -> &str {
        &self.thread_sandbox
    }

    /// `turn/start.sandboxPolicy`.
    pub fn turn_sandbox_policy(&self) -> &Value {
        &self.turn_sandbox_policy
    }

    /// The last stderr lines of the app-server (diagnostics).
    pub fn stderr_tail(&self) -> Vec<String> {
        self.transport.stderr_tail()
    }

    fn session_event(&self, data: CodexEventData) -> CodexEvent {
        CodexEvent::new(data, self.pid.clone(), self.worker_host.clone())
    }

    fn stream_event(&self, data: CodexEventData) -> CodexEvent {
        CodexEvent::new(data, self.pid.clone(), None)
    }

    /// `await_response/2`: waits for the response to `id`, with `read_timeout_ms` restarting on every
    /// line. Server messages (with a string `method`) arriving meanwhile are buffered and replayed by the
    /// next turn loop instead of being dropped; other lines are logged and ignored.
    async fn await_response(&mut self, id: i64) -> Result<Value, CodexError> {
        self.await_response_labelled(id, StreamLabel::Response)
            .await
    }

    /// Like [`Self::await_response`], but attributes out-of-band (stderr) output to `label` while waiting.
    async fn await_response_labelled(
        &mut self,
        id: i64,
        label: StreamLabel,
    ) -> Result<Value, CodexError> {
        self.transport.set_label(label);
        let wait = Duration::from_millis(self.codex.read_timeout_ms);
        loop {
            let line = match self.transport.next(wait).await {
                Incoming::Timeout => return Err(CodexError::ResponseTimeout),
                Incoming::Exit(status) => return Err(CodexError::PortExit(status)),
                Incoming::Line(line) => line,
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                log_non_json_stream_line(&line, "response stream");
                continue;
            };
            match protocol::match_response(&message, id) {
                ResponseMatch::Result(result) => return Ok(result),
                ResponseMatch::Error(error) => return Err(CodexError::ResponseError(error)),
                ResponseMatch::Other if message.get("method").is_some_and(Value::is_string) => {
                    if self.pending.len() < MAX_PENDING_MESSAGES {
                        tracing::debug!(
                            "Deferring server message while waiting for response: {line}"
                        );
                        self.pending.push_back(StreamMessage {
                            payload: message,
                            raw: line,
                        });
                    } else {
                        tracing::warn!(
                            "Dropping server message while waiting for response (buffer full): {line}"
                        );
                    }
                }
                ResponseMatch::Other => {
                    tracing::debug!("Ignoring message while waiting for response: {line}");
                }
            }
        }
    }

    /// Runs one turn: `turn/start` (id 3, reused by every turn), then streams until a terminal message.
    ///
    /// Emits `session_started` (or `startup_failed`), stream events, and `turn_ended_with_error` for
    /// non-blocker failures. Blocker failures ([`CodexError::blocker`]) end on their own
    /// `turn_input_required`/`approval_required` event so that event stays the session's last one.
    /// The session is not stopped on error; the caller decides.
    pub async fn run_turn(
        &mut self,
        prompt: &str,
        issue: &Issue,
        events: &EventSink,
    ) -> Result<TurnOutcome, CodexError> {
        let context = issue_context(issue);
        let span = tracing::info_span!(
            "codex_turn",
            issue_id = issue.id.as_deref(),
            issue_identifier = issue.identifier.as_deref()
        );
        let title = format!(
            "{}: {}",
            issue.identifier.as_deref().unwrap_or(""),
            issue.title.as_deref().unwrap_or("")
        );
        let request = protocol::turn_start_request(
            &self.thread_id,
            prompt,
            &self.workspace,
            &title,
            &self.approval_policy,
            &self.turn_sandbox_policy,
        );
        // stderr is a separate pipe, so the label must change before the request goes out: any output the
        // server writes while handling `turn/start` is causally part of the turn (Elixir got this ordering for
        // free because it merged stderr into stdout).
        self.transport.set_label(StreamLabel::Turn);
        self.transport.send(&request).await;
        let started = async {
            let result = self
                .await_response_labelled(TURN_START_ID, StreamLabel::Turn)
                .await?;
            protocol::turn_id_from_result(&result).map_err(CodexError::InvalidTurnPayload)
        }
        .instrument(span.clone())
        .await;
        let turn_id = match started {
            Ok(turn_id) => turn_id,
            Err(reason) => {
                tracing::error!(parent: &span, "Codex session failed for {context}: {reason}");
                events.emit(self.session_event(CodexEventData::StartupFailed {
                    reason: reason.clone(),
                }));
                return Err(reason);
            }
        };

        let session_id = format!("{}-{}", self.thread_id, turn_id);
        tracing::info!(parent: &span, "Codex session started for {context} session_id={session_id}");
        events.emit(self.session_event(CodexEventData::SessionStarted {
            session_id: session_id.clone(),
            thread_id: self.thread_id.clone(),
            turn_id: turn_id.clone(),
        }));

        match self
            .await_turn_completion(issue, events)
            .instrument(span.clone())
            .await
        {
            Ok(()) => {
                tracing::info!(parent: &span, "Codex session completed for {context} session_id={session_id}");
                Ok(TurnOutcome {
                    session_id,
                    thread_id: self.thread_id.clone(),
                    turn_id,
                })
            }
            Err(reason) => {
                tracing::warn!(parent: &span, "Codex session ended with error for {context} session_id={session_id}: {reason}");
                if reason.blocker().is_none() {
                    events.emit(self.session_event(CodexEventData::TurnEndedWithError {
                        session_id,
                        reason: reason.clone(),
                    }));
                }
                Err(reason)
            }
        }
    }

    async fn await_turn_completion(
        &mut self,
        issue: &Issue,
        events: &EventSink,
    ) -> Result<(), CodexError> {
        self.transport.set_label(StreamLabel::Turn);
        let wait = Duration::from_millis(self.codex.turn_timeout_ms);
        while let Some(message) = self.pending.pop_front() {
            if let Step::Done(result) = self.handle_message(message, issue, events).await {
                return result;
            }
        }
        loop {
            let line = match self.transport.next(wait).await {
                Incoming::Timeout => return Err(CodexError::TurnTimeout),
                Incoming::Exit(status) => return Err(CodexError::PortExit(status)),
                Incoming::Line(line) => line,
            };
            let payload = match serde_json::from_str::<Value>(&line) {
                Ok(payload) => payload,
                Err(_) => {
                    log_non_json_stream_line(&line, "turn stream");
                    if line.trim_start().starts_with('{') {
                        events.emit(self.stream_event(CodexEventData::Malformed { raw: line }));
                    }
                    continue;
                }
            };
            let message = StreamMessage { payload, raw: line };
            if let Step::Done(result) = self.handle_message(message, issue, events).await {
                return result;
            }
        }
    }

    /// `handle_incoming/6` for a decoded line.
    async fn handle_message(
        &mut self,
        message: StreamMessage,
        issue: &Issue,
        events: &EventSink,
    ) -> Step {
        let Some(method) = message
            .payload
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            events.emit(self.stream_event(CodexEventData::OtherMessage(message)));
            return Step::Continue;
        };
        let params = message.payload.get("params").cloned();
        match (method.as_str(), params) {
            // Current Codex versions report every turn end as `turn/completed` and put the outcome in
            // `turn.status`; only a missing or `completed` status is a success.
            ("turn/completed", Some(details))
                if protocol::turn_status(&details) == Some("failed") =>
            {
                let reason = CodexError::TurnFailed(details.clone());
                events.emit(self.stream_event(CodexEventData::TurnFailed { message, details }));
                Step::Done(Err(reason))
            }
            ("turn/completed", Some(details))
                if protocol::turn_status(&details) == Some("interrupted") =>
            {
                let reason = CodexError::TurnCancelled(details.clone());
                events.emit(self.stream_event(CodexEventData::TurnCancelled { message, details }));
                Step::Done(Err(reason))
            }
            ("turn/completed", _) => {
                events.emit(self.stream_event(CodexEventData::TurnCompleted(message)));
                Step::Done(Ok(()))
            }
            ("turn/failed", Some(details)) => {
                let reason = CodexError::TurnFailed(details.clone());
                events.emit(self.stream_event(CodexEventData::TurnFailed { message, details }));
                Step::Done(Err(reason))
            }
            ("turn/cancelled", Some(details)) => {
                let reason = CodexError::TurnCancelled(details.clone());
                events.emit(self.stream_event(CodexEventData::TurnCancelled { message, details }));
                Step::Done(Err(reason))
            }
            _ => self.handle_method(&method, message, issue, events).await,
        }
    }

    fn input_required(&self, message: StreamMessage, events: &EventSink) -> Step {
        let reason = CodexError::TurnInputRequired(message.payload.clone());
        events.emit(self.stream_event(CodexEventData::TurnInputRequired(message)));
        Step::Done(Err(reason))
    }

    /// `handle_turn_method` + `maybe_handle_approval_request`.
    async fn handle_method(
        &mut self,
        method: &str,
        message: StreamMessage,
        issue: &Issue,
        events: &EventSink,
    ) -> Step {
        let id = message.payload.get("id").cloned();
        let params = message.payload.get("params").cloned();

        if let (Some(decision), Some(id)) = (protocol::approval_decision(method), id.as_ref()) {
            if !self.auto_approve_requests {
                let reason = CodexError::ApprovalRequired(message.payload.clone());
                events.emit(self.stream_event(CodexEventData::ApprovalRequired(message)));
                return Step::Done(Err(reason));
            }
            self.transport
                .send(&protocol::response(id, json!({"decision": decision})))
                .await;
            events.emit(self.stream_event(CodexEventData::ApprovalAutoApproved {
                message,
                decision: decision.to_owned(),
            }));
            return Step::Continue;
        }

        match (method, id, params) {
            ("item/tool/call", Some(id), Some(params)) => {
                self.handle_tool_call(&id, &params, message, issue, events)
                    .await;
                Step::Continue
            }
            ("item/tool/requestUserInput", Some(id), Some(params)) => {
                let answers = self
                    .auto_approve_requests
                    .then(|| protocol::mcp_approval_answers(&params))
                    .flatten();
                let Some(answers) = answers else {
                    return self.input_required(message, events);
                };
                self.transport
                    .send(&protocol::response(&id, json!({"answers": answers})))
                    .await;
                events.emit(self.stream_event(CodexEventData::ApprovalAutoApproved {
                    message,
                    decision: DECISION_APPROVE_THIS_SESSION.to_owned(),
                }));
                Step::Continue
            }
            _ if protocol::needs_input(method, &message.payload) => {
                self.input_required(message, events)
            }
            _ => {
                tracing::debug!("Codex notification: {method:?}");
                events.emit(self.stream_event(CodexEventData::Notification(message)));
                Step::Continue
            }
        }
    }

    /// Dispatches `item/tool/call` to the handler (bounded by `turn_timeout_ms`), replies, then emits
    /// `tool_call_completed` / `unsupported_tool_call` / `tool_call_failed`.
    async fn handle_tool_call(
        &mut self,
        id: &Value,
        params: &Value,
        message: StreamMessage,
        issue: &Issue,
        events: &EventSink,
    ) {
        let tool = tool_call_name(params);
        let arguments = tool_call_arguments(params);
        let handler = Arc::clone(&self.tool_handler);
        let limit = Duration::from_millis(self.codex.turn_timeout_ms);
        let result = match tokio::time::timeout(
            limit,
            handler.execute(tool.as_deref(), arguments, issue),
        )
        .await
        {
            Ok(result) => normalize_tool_result(result),
            Err(_) => {
                tracing::warn!(tool = ?tool, "Dynamic tool call timed out after {}ms", limit.as_millis());
                tool_response(
                    false,
                    json!({"error": {"message": format!("Dynamic tool call timed out after {}ms.", limit.as_millis())}})
                        .to_string(),
                )
            }
        };
        let success = result.get("success") == Some(&Value::Bool(true));
        self.transport.send(&protocol::response(id, result)).await;
        let data = if success {
            CodexEventData::ToolCallCompleted(message)
        } else if tool.is_none() {
            CodexEventData::UnsupportedToolCall(message)
        } else {
            CodexEventData::ToolCallFailed(message)
        };
        events.emit(self.stream_event(data));
    }

    /// Stops the app-server: stdin EOF, up to `stop_grace` for a clean exit, then `SIGKILL` to the whole
    /// process group; the child is reaped before this returns.
    pub async fn stop(mut self) {
        self.transport.shutdown(self.stop_grace).await;
    }
}

/// `AppServer.run/4`: start a session, run one turn, always stop.
pub async fn run(
    options: StartOptions,
    prompt: &str,
    issue: &Issue,
    events: &EventSink,
) -> Result<TurnOutcome, CodexError> {
    let mut session = AppServerSession::start(options).await?;
    let result = session.run_turn(prompt, issue, events).await;
    session.stop().await;
    result
}
