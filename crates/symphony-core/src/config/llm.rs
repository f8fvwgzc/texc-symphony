//! The platform's LLM connection contract v1: `LLM_*` environment variables that configure model
//! access the same way in every core.
//!
//! | Variable | Meaning |
//! |---|---|
//! | `LLM_PROVIDER` | `anthropic`, `openai` or `openai-compatible` (Ollama, vLLM, LM Studio, OpenRouter, Groq, Together, Gemini's OpenAI endpoint, Azure, ... by `LLM_BASE_URL`) |
//! | `LLM_BASE_URL` | provider API base URL (required for `openai-compatible`) |
//! | `LLM_API_KEY_FILE` / `LLM_API_KEY` | the key: a file (preferred, e.g. a mounted Kubernetes Secret) or the value |
//! | `LLM_MODEL` / `LLM_MODEL_SMALL` | the model, and an optional small/fast one |
//! | `LLM_TIMEOUT_SECS` / `LLM_MAX_RETRIES` | per-request timeout and retries |
//! | `LLM_GATEWAY_URL` + `LLM_GATEWAY_TOKEN_FILE` | gateway mode: every call goes to the platform's model gateway with the gateway token; no provider key is used |
//!
//! The contract applies once `LLM_PROVIDER` or `LLM_GATEWAY_URL` is set. It supplies the defaults
//! of the built-in `llm` backend (Symphony's API agent loop), of `api` backends that leave out
//! their endpoint, of Codex (`codex.provider` / `codex.model`) and of Claude Code; settings in
//! `WORKFLOW.md` win. Secret values are never stored here: [`SecretSource::read`] reads them when
//! a session starts, so a rotated Secret file applies to the next session.

use std::path::{Path, PathBuf};

use super::backend::{ANTHROPIC_BASE_URL, ApiProvider, OPENAI_BASE_URL};
use super::cast::Errors;
use super::tracker::is_https_url;
use crate::env::EnvSource;

/// Name of the built-in backend the contract configures.
pub const LLM_BACKEND: &str = "llm";
/// `LLM_PROVIDER`.
pub const LLM_PROVIDER: &str = "LLM_PROVIDER";
/// `LLM_BASE_URL`.
pub const LLM_BASE_URL: &str = "LLM_BASE_URL";
/// `LLM_API_KEY`.
pub const LLM_API_KEY: &str = "LLM_API_KEY";
/// `LLM_API_KEY_FILE`.
pub const LLM_API_KEY_FILE: &str = "LLM_API_KEY_FILE";
/// `LLM_MODEL`.
pub const LLM_MODEL: &str = "LLM_MODEL";
/// `LLM_MODEL_SMALL`.
pub const LLM_MODEL_SMALL: &str = "LLM_MODEL_SMALL";
/// `LLM_TIMEOUT_SECS`.
pub const LLM_TIMEOUT_SECS: &str = "LLM_TIMEOUT_SECS";
/// `LLM_MAX_RETRIES`.
pub const LLM_MAX_RETRIES: &str = "LLM_MAX_RETRIES";
/// `LLM_GATEWAY_URL`.
pub const LLM_GATEWAY_URL: &str = "LLM_GATEWAY_URL";
/// `LLM_GATEWAY_TOKEN_FILE`.
pub const LLM_GATEWAY_TOKEN_FILE: &str = "LLM_GATEWAY_TOKEN_FILE";
/// The contract's credential variables, removed from every agent child environment.
pub const LLM_SECRET_ENV_NAMES: [&str; 3] = [LLM_API_KEY, LLM_API_KEY_FILE, LLM_GATEWAY_TOKEN_FILE];
/// Codex `model_providers` key of the contract's provider.
pub const CODEX_LLM_PROVIDER: &str = "symphony-llm";
/// Variable that carries the contract's credential to Codex (Codex reads its key from the
/// environment; this name is not removed from its environment).
pub const CODEX_LLM_KEY_ENV: &str = "SYMPHONY_LLM_API_KEY";
/// Most retries `LLM_MAX_RETRIES` accepts.
pub const MAX_LLM_RETRIES: u32 = 10;

/// `LLM_PROVIDER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlmProvider {
    /// The Anthropic Messages API.
    Anthropic,
    /// The OpenAI API.
    OpenAi,
    /// Any OpenAI-compatible server, at `LLM_BASE_URL`.
    OpenAiCompatible,
}

impl LlmProvider {
    /// The contract spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }

    /// The wire format Symphony's API loop speaks to it.
    pub fn wire(self) -> ApiProvider {
        match self {
            Self::Anthropic => ApiProvider::Anthropic,
            Self::OpenAi | Self::OpenAiCompatible => ApiProvider::OpenAi,
        }
    }
}

/// Where a secret comes from. Holds names and paths only, never the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretSource {
    /// An environment variable.
    Env(String),
    /// A file (re-read on every [`SecretSource::read`]).
    File(PathBuf),
}

impl SecretSource {
    /// The secret's value now, with surrounding whitespace trimmed. The error names the variable
    /// or file, never the value.
    pub fn read(&self, env: &dyn EnvSource) -> Result<String, String> {
        let value = match self {
            Self::Env(name) => env
                .var(name)
                .ok_or_else(|| format!("the variable {name} is not set"))?,
            Self::File(path) => std::fs::read_to_string(path)
                .map_err(|err| format!("cannot read {}: {err}", path.display()))?,
        };
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Err(match self {
                Self::Env(name) => format!("the variable {name} is empty"),
                Self::File(path) => format!("{} is empty", path.display()),
            });
        }
        Ok(value)
    }

    /// The file, for a file source.
    pub fn file(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Env(_) => None,
        }
    }
}

/// Gateway mode (`LLM_GATEWAY_URL`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmGateway {
    /// Gateway root URL without a trailing `/` or `/v1`; calls go to `{url}/v1/messages`,
    /// `{url}/v1/chat/completions` and `{url}/v1/responses`.
    pub url: String,
    /// `LLM_GATEWAY_TOKEN_FILE`, sent as a bearer token.
    pub token: Option<SecretSource>,
}

/// The resolved contract.
#[derive(Clone, Debug, PartialEq)]
pub struct LlmConnection {
    /// `LLM_PROVIDER` (default `anthropic` in gateway mode).
    pub provider: LlmProvider,
    /// `LLM_BASE_URL`.
    pub base_url: Option<String>,
    /// `LLM_API_KEY_FILE`, else `LLM_API_KEY` (unused in gateway mode).
    pub api_key: Option<SecretSource>,
    /// `LLM_MODEL`.
    pub model: String,
    /// `LLM_MODEL_SMALL`.
    pub model_small: Option<String>,
    /// `LLM_TIMEOUT_SECS`.
    pub timeout_secs: Option<u64>,
    /// `LLM_MAX_RETRIES`.
    pub max_retries: Option<u32>,
    /// Gateway mode.
    pub gateway: Option<LlmGateway>,
}

fn trimmed_root(url: &str) -> String {
    let url = url.trim_end_matches('/');
    url.strip_suffix("/v1").unwrap_or(url).to_owned()
}

impl LlmConnection {
    /// The wire format of model calls.
    pub fn wire(&self) -> ApiProvider {
        self.provider.wire()
    }

    /// The credential model calls send: the gateway token in gateway mode, else the API key.
    pub fn credential(&self) -> Option<&SecretSource> {
        match &self.gateway {
            Some(gateway) => gateway.token.as_ref(),
            None => self.api_key.as_ref(),
        }
    }

    /// Base URL for an Anthropic Messages client (`{base}/v1/messages`).
    pub fn anthropic_base_url(&self) -> String {
        match (&self.gateway, &self.base_url) {
            (Some(gateway), _) => gateway.url.clone(),
            (None, Some(base)) => trimmed_root(base),
            (None, None) => ANTHROPIC_BASE_URL.to_owned(),
        }
    }

    /// Base URL for an OpenAI-compatible client (`{base}/chat/completions`, `{base}/responses`).
    pub fn openai_base_url(&self) -> String {
        match (&self.gateway, &self.base_url) {
            (Some(gateway), _) => format!("{}/v1", gateway.url),
            (None, Some(base)) => base.trim_end_matches('/').to_owned(),
            (None, None) => OPENAI_BASE_URL.to_owned(),
        }
    }

    /// The base URL for this contract's own wire format.
    pub fn api_base_url(&self) -> String {
        match self.wire() {
            ApiProvider::Anthropic => self.anthropic_base_url(),
            ApiProvider::OpenAi => self.openai_base_url(),
        }
    }

    /// `true` when Claude Code can use the contract (it speaks Anthropic Messages only).
    pub fn serves_anthropic(&self) -> bool {
        self.gateway.is_some() || self.wire() == ApiProvider::Anthropic
    }

    /// `true` when Codex can use the contract (it speaks the OpenAI Responses API only).
    pub fn serves_openai(&self) -> bool {
        self.gateway.is_some() || self.wire() == ApiProvider::OpenAi
    }

    /// Files holding the contract's secrets (masked from sandboxed commands).
    pub fn secret_files(&self) -> Vec<PathBuf> {
        let gateway = self.gateway.as_ref().and_then(|g| g.token.as_ref());
        [self.api_key.as_ref(), gateway]
            .into_iter()
            .flatten()
            .filter_map(|s| s.file().map(Path::to_path_buf))
            .collect()
    }
}

fn get(env: &dyn EnvSource, name: &str) -> Option<String> {
    env.var(name)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn url_ok(value: &str) -> bool {
    match value.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => {
            is_https_url(&format!("https://{rest}"), true)
        }
        _ => is_https_url(value, true),
    }
}

/// `LLM_*` variables that are set although the contract is inactive (no `LLM_PROVIDER` or
/// `LLM_GATEWAY_URL`): likely a typo, worth a warning.
pub fn stray_variables(env: &dyn EnvSource) -> Vec<&'static str> {
    if get(env, LLM_PROVIDER).is_some() || get(env, LLM_GATEWAY_URL).is_some() {
        return Vec::new();
    }
    [
        LLM_BASE_URL,
        LLM_API_KEY,
        LLM_API_KEY_FILE,
        LLM_MODEL,
        LLM_MODEL_SMALL,
    ]
    .into_iter()
    .filter(|name| get(env, name).is_some())
    .collect()
}

/// One line describing what the contract configures in `settings` (startup log).
pub fn describe(settings: &super::Settings) -> Option<String> {
    let llm = settings.llm.as_ref()?;
    let endpoint = match &llm.gateway {
        Some(gateway) => format!("gateway {}", gateway.url),
        None => format!("{} {}", llm.provider.as_str(), llm.api_base_url()),
    };
    let codex = match &settings.codex.provider {
        Some(p) if p.name == CODEX_LLM_PROVIDER => "codex uses it",
        Some(_) => "codex keeps codex.provider",
        None => "codex keeps its own login",
    };
    Some(format!(
        "LLM connection contract: {endpoint}, model {}; backend `llm` available; {codex}; \
         agent.backend is `{}`",
        llm.model, settings.agent.backend
    ))
}

/// Reads the contract from `env`; `None` when neither `LLM_PROVIDER` nor `LLM_GATEWAY_URL` is set.
/// Problems are reported under the variable's name (`LLM_PROVIDER is invalid`).
pub(crate) fn from_env(env: &dyn EnvSource, errors: &mut Errors) -> Option<LlmConnection> {
    let provider = get(env, LLM_PROVIDER);
    let gateway_url = get(env, LLM_GATEWAY_URL);
    if provider.is_none() && gateway_url.is_none() {
        // Inactive. Other `LLM_*` variables alone are ignored (startup logs a warning, see
        // [`stray_variables`]): they must not break a workflow that never used the contract.
        return None;
    }
    let before = errors.len();
    let provider = match provider.as_deref() {
        None | Some("anthropic") => LlmProvider::Anthropic,
        Some("openai") => LlmProvider::OpenAi,
        Some("openai-compatible") => LlmProvider::OpenAiCompatible,
        Some(_) => {
            errors.push(LLM_PROVIDER, "is invalid");
            LlmProvider::Anthropic
        }
    };
    let base_url = get(env, LLM_BASE_URL);
    if base_url.as_deref().is_some_and(|url| !url_ok(url)) {
        errors.push(LLM_BASE_URL, "is invalid");
    }
    if provider == LlmProvider::OpenAiCompatible && base_url.is_none() && gateway_url.is_none() {
        errors.push(LLM_BASE_URL, "can't be blank");
    }
    let gateway = match gateway_url {
        None => None,
        Some(url) if url_ok(&url) => Some(LlmGateway {
            url: trimmed_root(&url),
            token: get(env, LLM_GATEWAY_TOKEN_FILE).map(|p| SecretSource::File(p.into())),
        }),
        Some(_) => {
            errors.push(LLM_GATEWAY_URL, "is invalid");
            None
        }
    };
    let api_key = match get(env, LLM_API_KEY_FILE) {
        Some(path) => Some(SecretSource::File(path.into())),
        None => get(env, LLM_API_KEY).map(|_| SecretSource::Env(LLM_API_KEY.to_owned())),
    };
    let model = get(env, LLM_MODEL);
    if model.is_none() {
        errors.push(LLM_MODEL, "can't be blank");
    }
    let timeout_secs = match get(env, LLM_TIMEOUT_SECS) {
        None => None,
        Some(raw) => match raw.parse::<u64>() {
            Ok(secs) if secs > 0 => Some(secs),
            _ => {
                errors.push(LLM_TIMEOUT_SECS, "must be a positive integer");
                None
            }
        },
    };
    let max_retries = match get(env, LLM_MAX_RETRIES) {
        None => None,
        Some(raw) => match raw.parse::<u32>() {
            Ok(n) if n <= MAX_LLM_RETRIES => Some(n),
            _ => {
                errors.push(LLM_MAX_RETRIES, "must be an integer between 0 and 10");
                None
            }
        },
    };
    (errors.len() == before).then(|| LlmConnection {
        provider,
        base_url,
        api_key,
        model: model.unwrap_or_default(),
        model_small: get(env, LLM_MODEL_SMALL),
        timeout_secs,
        max_retries,
        gateway,
    })
}
