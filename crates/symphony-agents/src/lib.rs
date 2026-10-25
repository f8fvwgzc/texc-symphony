//! symphony-agents: the agent backends Symphony can run, behind one [`AgentBackend`] trait.
//!
//! The runner (`symphony-runtime`) drives every backend the same way: [`AgentBackend::start`] a
//! session in the issue workspace, [`AgentSession::run_turn`] once per turn while streaming
//! events to an [`EventSink`], then [`AgentSession::stop`]. Usage is reported as token events.
//!
//! Backends ([`backend_for`] picks one from a `backends.<name>` entry of WORKFLOW.md):
//!
//! * [`codex`]: `codex app-server` (the original backend; [`symphony_codex`] does the work).
//! * [`claude`]: the Claude Code CLI (`claude -p --output-format stream-json`), one process per
//!   turn, resumed by session id.
//! * [`api`]: Symphony's own agent loop over an HTTP model API, speaking Anthropic Messages or
//!   OpenAI-compatible Chat Completions (OpenAI, Ollama, vLLM, LM Studio, OpenRouter, Groq,
//!   Together, Gemini's OpenAI endpoint), with workspace tools (files, a sandboxed shell) and the
//!   tracker's scoped tools.
//!
//! **Events.** The orchestrator, run history, dashboard and humanizer understand Codex-shaped
//! events ([`symphony_codex::CodexEvent`]), so every backend reports in that vocabulary
//! ([`events::Emitter`]): `session_started`, `item/started` / `item/completed` items
//! (`agentMessage`, `commandExecution`), `item/agentMessage/delta`, `item/tool/call` results,
//! `thread/tokenUsage/updated` with cumulative totals, `turn/completed`, and
//! `turn_ended_with_error` with a [`CodexError`].

#![warn(missing_docs)]

pub mod api;
pub mod claude;
pub mod codex;
pub mod egress;
pub mod events;
#[cfg(unix)]
pub mod mcp;
mod process;

use std::sync::Arc;

use async_trait::async_trait;
use symphony_codex::{CodexError, EventSink, StartOptions, TurnOutcome};
use symphony_core::Issue;
use symphony_core::config::{BackendKind, BackendSettings};

pub use symphony_codex::StartOptions as SessionOptions;

/// Starts agent sessions of one configured backend.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// The `backends.<name>` key.
    fn name(&self) -> &str;

    /// Validates the workspace and starts a session there. `options.tool_handler` carries the
    /// tracker's scoped tools; `options.settings` is the settings snapshot for the whole session.
    async fn start(&self, options: StartOptions) -> Result<Box<dyn AgentSession>, CodexError>;
}

/// One running agent session (one dispatch of one issue or task).
#[async_trait]
pub trait AgentSession: Send {
    /// Runs one turn with `prompt`, streaming events to `events`. Failures are also reported as a
    /// `turn_ended_with_error` (or blocker) event, like the Codex session does.
    async fn run_turn(
        &mut self,
        prompt: &str,
        issue: &Issue,
        events: &EventSink,
    ) -> Result<TurnOutcome, CodexError>;

    /// OS pid of a long-lived agent process (recorded so a restarted Symphony can stop it), if
    /// the backend keeps one between turns.
    fn pid(&self) -> Option<u32>;

    /// Stops the session and every process it started; returns once they are gone.
    async fn stop(self: Box<Self>);
}

/// The backend for one `backends.<name>` entry.
pub fn backend_for(settings: &BackendSettings) -> Arc<dyn AgentBackend> {
    match &settings.kind {
        BackendKind::Codex => Arc::new(codex::CodexBackend::new(settings.clone())),
        BackendKind::ClaudeCode(_) => Arc::new(claude::ClaudeCodeBackend::new(settings.clone())),
        BackendKind::Api(_) => Arc::new(api::ApiBackend::new(settings.clone())),
    }
}
