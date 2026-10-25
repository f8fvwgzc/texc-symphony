//! The `codex` backend: `codex app-server` through [`symphony_codex::AppServerSession`].

use std::sync::Arc;

use async_trait::async_trait;
use symphony_codex::{AppServerSession, CodexError, EventSink, StartOptions, TurnOutcome};
use symphony_core::config::BackendSettings;
use symphony_core::{Issue, Settings};

use crate::{AgentBackend, AgentSession};

/// Runs `codex app-server` as configured by the `codex:` section (a backend `model` overrides
/// `codex.model`).
#[derive(Debug, Clone)]
pub struct CodexBackend {
    settings: BackendSettings,
}

impl CodexBackend {
    /// The backend for `settings` (kind `codex`).
    pub fn new(settings: BackendSettings) -> Self {
        Self { settings }
    }
}

#[async_trait]
impl AgentBackend for CodexBackend {
    fn name(&self) -> &str {
        &self.settings.name
    }

    async fn start(&self, mut options: StartOptions) -> Result<Box<dyn AgentSession>, CodexError> {
        if let Some(model) = &self.settings.model {
            let mut settings: Settings = (*options.settings).clone();
            settings.codex.model = Some(model.clone());
            options.settings = Arc::new(settings);
        }
        let session = AppServerSession::start(options).await?;
        Ok(Box::new(CodexSession { session }))
    }
}

struct CodexSession {
    session: AppServerSession,
}

#[async_trait]
impl AgentSession for CodexSession {
    async fn run_turn(
        &mut self,
        prompt: &str,
        issue: &Issue,
        events: &EventSink,
    ) -> Result<TurnOutcome, CodexError> {
        self.session.run_turn(prompt, issue, events).await
    }

    fn pid(&self) -> Option<u32> {
        self.session.codex_app_server_pid()?.parse().ok()
    }

    async fn stop(self: Box<Self>) {
        self.session.stop().await;
    }
}
