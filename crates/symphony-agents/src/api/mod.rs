//! The `api` backend: Symphony's own agent loop over a model API.
//!
//! One session keeps one conversation for all its turns. A turn adds the prompt, then calls the
//! model up to `max_steps` times: every response's text is streamed as
//! `item/agentMessage/delta` and recorded as an `agentMessage` item, its tool calls run in order
//! ([`tools`]), and their results go back to the model, until it answers without tool calls.
//!
//! Events of one turn, in order: `session_started`; per model call: deltas, the `agentMessage`
//! item (when it wrote text), cumulative `thread/tokenUsage/updated`, then per tool call a
//! `commandExecution` item pair (`shell`), a `fileChange` item (`write_file`, `edit_file`) or a
//! `tool_call_completed` / `tool_call_failed` event (the rest); finally `turn_completed`, or
//! `turn_ended_with_error` when the turn fails (the conversation is then rolled back to before
//! the turn, so the next turn starts from a consistent history).
//!
//! Requests are retried on HTTP 429 and 5xx and on connection failures, up to three times with
//! 1 s / 2 s / 4 s backoff (`retry-after` seconds win, capped at 30 s). Wire formats:
//! [`anthropic`] and [`openai`].

pub mod anthropic;
pub mod openai;
pub mod sandbox;
pub mod sse;
pub mod tools;
pub mod wire;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{CONTENT_TYPE, RETRY_AFTER};
use serde_json::Value;
use symphony_codex::launch::{valid_secret_names, validate_local_workspace};
use symphony_codex::{CodexError, EventSink, StartOptions, TurnOutcome};
use symphony_core::config::{ApiBackendSettings, BackendKind, BackendSettings, SecretSource};
use symphony_core::{EnvSource, Issue, ProcessEnv};

use self::sse::SseParser;
use self::tools::{ShellPolicy, Toolbox};
use self::wire::{Message, Reply, Request, ToolDef, Wire};
use crate::events::{Emitter, Usage};
use crate::{AgentBackend, AgentSession};

/// Longest wait between retries.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Bytes of an error response quoted in the error.
pub const ERROR_EXCERPT_CHARS: usize = 500;

/// A process-unique id with `prefix` (thread, turn and item ids).
pub(crate) fn new_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = chrono::Utc::now().timestamp_millis();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{millis:x}-{n:x}")
}

/// Runs turns with Symphony's agent loop (see the module docs).
#[derive(Clone)]
pub struct ApiBackend {
    settings: BackendSettings,
    env: Arc<dyn EnvSource>,
    retry_base: Duration,
}

impl std::fmt::Debug for ApiBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiBackend")
            .field("name", &self.settings.name)
            .field("retry_base", &self.retry_base)
            .finish_non_exhaustive()
    }
}

impl ApiBackend {
    /// The backend for `settings` (kind `api`); the API key is read from the process
    /// environment.
    pub fn new(settings: BackendSettings) -> Self {
        Self {
            settings,
            env: Arc::new(ProcessEnv),
            retry_base: Duration::from_secs(1),
        }
    }

    /// Reads `api_key_env` from `env` instead of the process environment.
    pub fn with_env(mut self, env: Arc<dyn EnvSource>) -> Self {
        self.env = env;
        self
    }

    /// First retry delay (doubled per retry; 1 s by default). Tests shorten it.
    pub fn with_retry_base(mut self, base: Duration) -> Self {
        self.retry_base = base;
        self
    }

    fn api(&self) -> Result<&ApiBackendSettings, CodexError> {
        match &self.settings.kind {
            BackendKind::Api(api) => Ok(api),
            other => Err(CodexError::Unsupported(format!(
                "backend `{}` has kind {}, not api",
                self.settings.name,
                other.as_str()
            ))),
        }
    }
}

fn default_system_prompt(workspace: &std::path::Path) -> String {
    format!(
        "You are an autonomous agent working in the directory {}. Use the tools to inspect and \
         change files and to run commands there; paths are relative to that directory. Keep \
         working until the task is done, without asking for confirmation. Then reply with a \
         short summary of what you did and of anything that blocked you.",
        workspace.display()
    )
}

#[async_trait]
impl AgentBackend for ApiBackend {
    fn name(&self) -> &str {
        &self.settings.name
    }

    async fn start(&self, options: StartOptions) -> Result<Box<dyn AgentSession>, CodexError> {
        let api = self.api()?.clone();
        if let Some(host) = &options.worker_host {
            return Err(CodexError::Unsupported(format!(
                "the api backend runs its tools on the Symphony host; worker_host={host} is not supported"
            )));
        }
        let model = self.settings.model.clone().ok_or_else(|| {
            CodexError::Unsupported(format!("backend `{}` has no model", self.settings.name))
        })?;
        let root = options
            .settings
            .local_workspace_root(&options.workflow_path);
        let workspace = validate_local_workspace(&options.workspace, &root)
            .map_err(CodexError::InvalidWorkspaceCwd)?;
        let source = match (&api.api_key_file, &api.api_key_env) {
            (Some(path), _) => Some(SecretSource::File(path.clone())),
            (None, Some(name)) => Some(SecretSource::Env(name.clone())),
            (None, None) => None,
        };
        let key = source
            .map(|source| source.read(self.env.as_ref()))
            .transpose()
            .map_err(|err| CodexError::Backend(format!("no API key: {err}")))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(api.request_timeout_ms))
            .build()
            .map_err(|err| CodexError::Backend(format!("cannot build the HTTP client: {err}")))?;
        let secret_names = valid_secret_names(
            options
                .settings
                .secret_environment_names()
                .into_iter()
                .chain(options.tool_handler.secret_environment_names()),
        );
        let toolbox = Toolbox::new(
            workspace.clone(),
            api.tools.clone(),
            Arc::clone(&options.tool_handler),
            ShellPolicy {
                sandbox: api.sandbox,
                network: api.network,
                timeout: Duration::from_millis(api.shell_timeout_ms),
                secret_names,
                extra_env: options.env.clone(),
                masked_files: options
                    .settings
                    .llm
                    .as_ref()
                    .map(|llm| llm.secret_files())
                    .unwrap_or_default()
                    .into_iter()
                    .chain(api.api_key_file.clone())
                    .collect(),
            },
        );
        let system = self
            .settings
            .system_prompt
            .clone()
            .unwrap_or_else(|| default_system_prompt(&workspace));
        Ok(Box::new(ApiSession {
            wire: Wire::new(api.provider, &api.base_url),
            tools: toolbox.definitions(),
            toolbox,
            api,
            model,
            key,
            client,
            system,
            thread_id: new_id("thread"),
            messages: Vec::new(),
            usage: Usage::default(),
            turn_timeout: Duration::from_millis(self.settings.turn_timeout_ms),
            retry_base: self.retry_base,
            _workspace: workspace,
        }))
    }
}

struct ApiSession {
    api: ApiBackendSettings,
    wire: Wire,
    model: String,
    key: Option<String>,
    client: reqwest::Client,
    system: String,
    toolbox: Toolbox,
    tools: Vec<ToolDef>,
    thread_id: String,
    messages: Vec<Message>,
    usage: Usage,
    turn_timeout: Duration,
    retry_base: Duration,
    _workspace: PathBuf,
}

/// `text` cut to `max` chars, with `secret` removed.
fn excerpt(text: &str, secret: Option<&str>, max: usize) -> String {
    let text = match secret.filter(|s| !s.is_empty()) {
        Some(secret) => text.replace(secret, "[REDACTED]"),
        None => text.to_owned(),
    };
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

impl ApiSession {
    fn backend_error(&self, message: impl std::fmt::Display) -> CodexError {
        CodexError::Backend(format!("{} API: {message}", self.wire.label()))
    }

    fn retry_delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        retry_after
            .unwrap_or(self.retry_base * 2u32.saturating_pow(attempt))
            .min(MAX_RETRY_DELAY)
    }

    /// One model call (with retries), streaming text deltas to `emitter`.
    async fn call_model(&self, emitter: &Emitter<'_>) -> Result<Reply, CodexError> {
        let request = Request {
            model: &self.model,
            system: &self.system,
            messages: &self.messages,
            tools: &self.tools,
            max_tokens: self.api.max_output_tokens,
            temperature: self.api.temperature,
            stream: self.api.stream,
        };
        let body = self.wire.body(&request);
        let url = self.wire.url(&self.api.base_url);
        let mut attempt = 0;
        loop {
            let mut builder = self.client.post(&url).json(&body);
            for (name, value) in self
                .wire
                .headers(self.key.as_deref())
                .into_iter()
                .chain(self.api.headers.clone())
            {
                builder = builder.header(name, value);
            }
            let response = match builder.send().await {
                Ok(response) => response,
                Err(err)
                    if attempt < self.api.max_retries && (err.is_connect() || err.is_timeout()) =>
                {
                    tracing::warn!("Model request failed, retrying: {err}");
                    tokio::time::sleep(self.retry_delay(attempt, None)).await;
                    attempt += 1;
                    continue;
                }
                Err(err) => {
                    return Err(self.backend_error(format!(
                        "request failed: {}",
                        excerpt(&err.to_string(), self.key.as_deref(), ERROR_EXCERPT_CHARS)
                    )));
                }
            };
            let status = response.status();
            if status.is_success() {
                return self.read_reply(response, emitter).await;
            }
            let retryable = status.as_u16() == 429 || status.is_server_error();
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let text = response.text().await.unwrap_or_default();
            if retryable && attempt < self.api.max_retries {
                tracing::warn!(status = status.as_u16(), "Model API busy, retrying");
                tokio::time::sleep(self.retry_delay(attempt, retry_after)).await;
                attempt += 1;
                continue;
            }
            return Err(self.backend_error(format!(
                "HTTP {}: {}",
                status.as_u16(),
                excerpt(&text, self.key.as_deref(), ERROR_EXCERPT_CHARS)
            )));
        }
    }

    async fn read_reply(
        &self,
        mut response: reqwest::Response,
        emitter: &Emitter<'_>,
    ) -> Result<Reply, CodexError> {
        let event_stream = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        if !event_stream {
            // Not streamed (by request, or a server that ignored `stream: true`).
            let body: Value = response
                .json()
                .await
                .map_err(|err| self.backend_error(format!("undecodable response: {err}")))?;
            return self.wire.parse(&body).map_err(|e| self.backend_error(e));
        }
        let mut parser = SseParser::new();
        let mut state = self.wire.stream();
        let mut on_text = |text: &str| emitter.agent_delta(text);
        'read: loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|err| self.backend_error(format!("stream interrupted: {err}")))?;
            let Some(chunk) = chunk else {
                if let Some(event) = parser.finish() {
                    state
                        .apply(&event, &mut on_text)
                        .map_err(|e| self.backend_error(e))?;
                }
                break;
            };
            for event in parser.feed(&chunk) {
                if state
                    .apply(&event, &mut on_text)
                    .map_err(|e| self.backend_error(e))?
                {
                    break 'read;
                }
            }
        }
        state.finish().map_err(|e| self.backend_error(e))
    }

    async fn turn_loop(
        &mut self,
        issue: &Issue,
        emitter: &Emitter<'_>,
        turn_id: &str,
    ) -> Result<Option<String>, CodexError> {
        let mut final_message = None;
        for _ in 0..self.api.max_steps {
            let reply = self.call_model(emitter).await?;
            self.usage.add(reply.usage);
            emitter.usage(&self.thread_id, turn_id, self.usage);
            if !reply.text.is_empty() {
                emitter.agent_message(&new_id("msg"), &reply.text);
                final_message = Some(reply.text.clone());
            }
            let calls = reply.tool_calls.clone();
            self.messages.push(Message::Assistant {
                text: reply.text,
                tool_calls: reply.tool_calls,
            });
            if calls.is_empty() {
                return Ok(final_message);
            }
            let mut results = Vec::with_capacity(calls.len());
            for call in &calls {
                results.push(self.toolbox.run(call, issue, emitter).await);
            }
            self.messages.push(Message::ToolResults(results));
        }
        Err(CodexError::LimitExceeded(format!(
            "max_steps ({}) model calls without a final answer",
            self.api.max_steps
        )))
    }
}

#[async_trait]
impl AgentSession for ApiSession {
    async fn run_turn(
        &mut self,
        prompt: &str,
        issue: &Issue,
        events: &EventSink,
    ) -> Result<TurnOutcome, CodexError> {
        let emitter = Emitter::new(events, None);
        let turn_id = new_id("turn");
        let session_id = emitter.session_started(&self.thread_id, &turn_id);
        let mark = self.messages.len();
        self.messages.push(Message::User(prompt.to_owned()));
        let limit = self.turn_timeout;
        let result = tokio::time::timeout(limit, self.turn_loop(issue, &emitter, &turn_id))
            .await
            .unwrap_or(Err(CodexError::TurnTimeout));
        match result {
            Ok(final_message) => {
                emitter.turn_completed(&self.thread_id, &turn_id);
                Ok(TurnOutcome {
                    session_id,
                    thread_id: self.thread_id.clone(),
                    turn_id,
                    final_message,
                })
            }
            Err(reason) => {
                // A half-finished exchange (tool calls without results) would poison later turns.
                self.messages.truncate(mark);
                tracing::warn!("API agent turn failed session_id={session_id}: {reason}");
                emitter.turn_failed(&session_id, &reason);
                Err(reason)
            }
        }
    }

    fn pid(&self) -> Option<u32> {
        None
    }

    async fn stop(self: Box<Self>) {
        // Shell commands only run inside `run_turn` and are killed with their process group
        // when it returns or is dropped; nothing outlives the session.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_are_short_and_never_carry_the_key() {
        assert_eq!(
            excerpt("abc sk-1 def", Some("sk-1"), 100),
            "abc [REDACTED] def"
        );
        assert_eq!(excerpt("abcdef", None, 3), "abc…");
        let a = new_id("t");
        let b = new_id("t");
        assert_ne!(a, b);
        assert!(a.starts_with("t-"));
    }
}
