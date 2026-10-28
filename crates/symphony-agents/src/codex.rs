//! The `codex` backend: `codex app-server` through [`symphony_codex::AppServerSession`].
//!
//! With the LLM contract, `codex.provider` points at the contract's endpoint (see
//! `symphony_core::config::llm`); Codex reads its key from the environment, so the session's
//! child gets the contract's credential as `SYMPHONY_LLM_API_KEY` (local runs only).
//!
//! A task with a default-deny network runs the app-server in the network jail
//! ([`crate::egress::network_jail`]): its only route out is the task's egress proxy.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use symphony_codex::{AppServerSession, CodexError, EventSink, StartOptions, TurnOutcome};
use symphony_core::config::BackendSettings;
use symphony_core::config::llm::{CODEX_LLM_KEY_ENV, CODEX_LLM_PROVIDER};
use symphony_core::{EnvSource, Issue, ProcessEnv, Settings};

use crate::{AgentBackend, AgentSession};

/// Runs `codex app-server` as configured by the `codex:` section (a backend `model` overrides
/// `codex.model`).
#[derive(Clone)]
pub struct CodexBackend {
    settings: BackendSettings,
    env: Arc<dyn EnvSource>,
    /// The `symphony` executable whose `egress-shim` subcommand relays a jailed app-server's
    /// traffic to a task's egress proxy.
    egress_shim: Option<PathBuf>,
}

impl std::fmt::Debug for CodexBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexBackend")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl CodexBackend {
    /// The backend for `settings` (kind `codex`).
    pub fn new(settings: BackendSettings) -> Self {
        Self {
            settings,
            env: Arc::new(ProcessEnv),
            egress_shim: std::env::current_exe().ok(),
        }
    }

    /// Reads the contract's `LLM_API_KEY` from `env` instead of the process environment.
    pub fn with_env(mut self, env: Arc<dyn EnvSource>) -> Self {
        self.env = env;
        self
    }

    /// The executable whose `egress-shim` subcommand relays a jailed app-server's traffic
    /// (default: the running executable, `symphony` in production; `None` refuses tasks with a
    /// default-deny network).
    pub fn with_egress_shim(mut self, program: Option<PathBuf>) -> Self {
        self.egress_shim = program;
        self
    }
}

/// The contract credential for Codex, when `codex.provider` is the contract's provider.
fn contract_key(settings: &Settings, env: &dyn EnvSource) -> Result<Option<String>, CodexError> {
    let (Some(llm), Some(provider)) = (&settings.llm, &settings.codex.provider) else {
        return Ok(None);
    };
    if provider.name != CODEX_LLM_PROVIDER
        || provider.api_key_env.as_deref() != Some(CODEX_LLM_KEY_ENV)
    {
        return Ok(None);
    }
    llm.credential()
        .map(|source| source.read(env))
        .transpose()
        .map_err(|err| CodexError::Backend(format!("no LLM credential for Codex: {err}")))
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
        if options.worker_host.is_none()
            && let Some(key) = contract_key(&options.settings, self.env.as_ref())?
        {
            options = options.with_env(CODEX_LLM_KEY_ENV, key);
        }
        // A default-deny network confines the whole app-server (and the commands it runs) to the
        // task's egress proxy; the shim inside sets the proxy variables.
        if let Some(route) = options.egress.clone() {
            let jail = crate::egress::network_jail(
                &route,
                self.egress_shim.as_deref(),
                Path::new(&options.workspace),
                &options.settings.host_secret_files(),
            )
            .map_err(CodexError::Unsupported)?;
            options = options.with_launcher(jail);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use symphony_core::MapEnv;

    fn parse(env: &MapEnv, config: serde_json::Value) -> Settings {
        let serde_json::Value::Object(map) = config else {
            unreachable!("an object")
        };
        symphony_core::config::parse(&map, env).unwrap()
    }

    #[test]
    fn codex_gets_the_contract_key_only_for_the_contract_provider() {
        let env = MapEnv::new()
            .with("LLM_PROVIDER", "openai-compatible")
            .with("LLM_BASE_URL", "http://vllm.internal:8000/v1")
            .with("LLM_API_KEY", "sk-contract-0123456789")
            .with("LLM_MODEL", "qwen3-coder");
        let settings = parse(&env, json!({}));
        assert_eq!(
            contract_key(&settings, &env).unwrap().as_deref(),
            Some("sk-contract-0123456789")
        );
        // A provider of the workflow's own keeps its own key handling.
        let own = parse(
            &env,
            json!({"codex": {"provider": {"name": "ollama", "base_url": "http://127.0.0.1:11434/v1"}}}),
        );
        assert_eq!(contract_key(&own, &env).unwrap(), None);
        // A missing key file is an error, not a silent keyless run.
        let env = MapEnv::new()
            .with("LLM_GATEWAY_URL", "http://gateway:8080")
            .with("LLM_GATEWAY_TOKEN_FILE", "/nonexistent/symphony-test-token")
            .with("LLM_MODEL", "gpt-5");
        let settings = parse(&env, json!({}));
        assert!(contract_key(&settings, &env).is_err());
    }
}
