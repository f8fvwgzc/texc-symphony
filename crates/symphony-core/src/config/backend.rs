//! `backends:` — the agent backends a workflow can run, and `agent.backend`, the default one.
//!
//! ```yaml
//! agent:
//!   backend: local            # default for tracker issues and tasks (built-in: codex)
//! backends:
//!   claude:
//!     kind: claude_code       # the Claude Code CLI
//!     model: sonnet
//!     allowed_tools: [Read, Edit, Write, Glob, Grep, "Bash(cargo:*)"]
//!   local:
//!     kind: api               # Symphony's own agent loop over an HTTP model API
//!     provider: openai        # wire format: openai (Chat Completions) or anthropic (Messages)
//!     base_url: http://localhost:11434/v1
//!     model: qwen2.5-coder:32b
//!     tools: [read_file, write_file, edit_file, list_files, shell, tracker]
//! ```
//!
//! `codex` always exists (kind `codex`, configured by the `codex:` section) unless the workflow
//! defines its own `codex` entry. Any OpenAI-compatible server (OpenAI, Ollama, vLLM, LM Studio,
//! OpenRouter, Groq, Together, Gemini's OpenAI endpoint) is a `kind: api, provider: openai`
//! entry with its `base_url`; no code is needed.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::cast::{Errors, cast_boolean, cast_integer, cast_number, cast_string_list};
use super::tracker::is_https_url;
use crate::env;

/// Name of the built-in backend that drives `codex app-server`.
pub const CODEX_BACKEND: &str = "codex";
/// Default Anthropic Messages API base URL.
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// Default OpenAI API base URL.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Tools the API agent loop can offer.
pub const API_TOOLS: [&str; 6] = [
    "read_file",
    "write_file",
    "edit_file",
    "list_files",
    "shell",
    "tracker",
];

/// Wire format of an `api` backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiProvider {
    /// Anthropic Messages API (`POST {base_url}/v1/messages`).
    Anthropic,
    /// OpenAI-compatible Chat Completions (`POST {base_url}/chat/completions`).
    OpenAi,
}

impl ApiProvider {
    /// Config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }
}

/// How the API loop's `shell` tool is confined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxMode {
    /// `bubblewrap` on Linux or `sandbox-exec` on macOS when installed; otherwise `shell` refuses
    /// to run (set `none` to run unconfined, e.g. inside a container that is the sandbox).
    Auto,
    /// Linux `bwrap`: read-only root, writable workspace and `/tmp`, no network unless allowed.
    Bubblewrap,
    /// macOS `sandbox-exec` profile: writes only to the workspace and temp dirs, no network
    /// unless allowed.
    Seatbelt,
    /// No confinement beyond the workspace cwd and a filtered environment.
    None,
}

/// Token prices of one backend (override `budget` prices for its runs).
#[derive(Clone, Debug, PartialEq)]
pub struct BackendPricing {
    /// USD per million input tokens.
    pub input_usd_per_million_tokens: f64,
    /// USD per million output tokens.
    pub output_usd_per_million_tokens: f64,
}

/// `kind: claude_code` settings.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeCodeSettings {
    /// Executable (default `claude`), run with `bash -lc` in the workspace.
    pub command: String,
    /// `--allowedTools` entries (Claude Code permission rules).
    pub allowed_tools: Vec<String>,
    /// `--disallowedTools` entries.
    pub disallowed_tools: Vec<String>,
    /// `--permission-mode` (default `acceptEdits`).
    pub permission_mode: String,
    /// `--max-turns` per Symphony turn, if set.
    pub max_turns: Option<u32>,
    /// Name of an env var whose value is passed to Claude Code as `ANTHROPIC_API_KEY`. Unset:
    /// Claude Code uses its own login.
    pub api_key_env: Option<String>,
    /// Extra arguments appended verbatim.
    pub extra_args: Vec<String>,
}

impl Default for ClaudeCodeSettings {
    fn default() -> Self {
        Self {
            command: "claude".into(),
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            permission_mode: "acceptEdits".into(),
            max_turns: None,
            api_key_env: None,
            extra_args: Vec::new(),
        }
    }
}

/// `kind: api` settings.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiBackendSettings {
    /// Wire format.
    pub provider: ApiProvider,
    /// API base URL (`http://` allowed for local servers).
    pub base_url: String,
    /// Name of the env var holding the API key (sent as `x-api-key` / `Authorization: Bearer`).
    /// Read host-side and removed from every agent child. Unset: no key is sent.
    pub api_key_env: Option<String>,
    /// Extra request headers (non-secret, e.g. OpenRouter's `HTTP-Referer`).
    pub headers: BTreeMap<String, String>,
    /// Tools offered to the model (subset of [`API_TOOLS`]).
    pub tools: Vec<String>,
    /// Model calls per turn at most (`> 0`, default 50).
    pub max_steps: u32,
    /// `max_tokens` of each model response (`> 0`, default 8192).
    pub max_output_tokens: u32,
    /// Sampling temperature, if set.
    pub temperature: Option<f64>,
    /// Stream responses (default `true`); `false` for servers with broken streaming.
    pub stream: bool,
    /// Bound on one model request in ms (`> 0`, default 600 000).
    pub request_timeout_ms: u64,
    /// `shell` confinement.
    pub sandbox: SandboxMode,
    /// Network access for `shell` commands (default `false`).
    pub network: bool,
    /// Bound on one `shell` command in ms (`> 0`, default 600 000).
    pub shell_timeout_ms: u64,
}

/// What a backend runs.
#[derive(Clone, Debug, PartialEq)]
pub enum BackendKind {
    /// `codex app-server`, configured by the `codex:` section.
    Codex,
    /// The Claude Code CLI.
    ClaudeCode(ClaudeCodeSettings),
    /// Symphony's built-in agent loop over a model API.
    Api(ApiBackendSettings),
}

impl BackendKind {
    /// Config spelling (`codex`, `claude_code`, `api`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode(_) => "claude_code",
            Self::Api(_) => "api",
        }
    }
}

/// One `backends.<name>` entry.
#[derive(Clone, Debug, PartialEq)]
pub struct BackendSettings {
    /// The entry's key.
    pub name: String,
    /// What it runs.
    pub kind: BackendKind,
    /// Model id or alias (`codex` kind: overrides `codex.model`).
    pub model: Option<String>,
    /// Liquid template wrapping each first-turn prompt: `{{ prompt }}` (the workflow or task
    /// prompt), `{{ issue }}` and `{{ attempt }}`.
    pub prompt_template: Option<String>,
    /// System prompt (`api`: the system message; `claude_code`: `--append-system-prompt`).
    pub system_prompt: Option<String>,
    /// Prices for this backend's runs (else `budget` prices).
    pub pricing: Option<BackendPricing>,
    /// Silence timeout of one turn in ms (`> 0`, default `codex.turn_timeout_ms`'s default).
    pub turn_timeout_ms: u64,
}

impl BackendSettings {
    /// The built-in `codex` entry.
    pub fn codex() -> Self {
        Self {
            name: CODEX_BACKEND.into(),
            kind: BackendKind::Codex,
            model: None,
            prompt_template: None,
            system_prompt: None,
            pricing: None,
            turn_timeout_ms: 3_600_000,
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn http_url(value: &str) -> bool {
    match value.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => {
            is_https_url(&format!("https://{rest}"), false)
        }
        _ => is_https_url(value, false),
    }
}

struct Entry<'a> {
    path: String,
    map: &'a Map<String, Value>,
}

impl Entry<'_> {
    fn key(&self, key: &str) -> String {
        format!("{}.{key}", self.path)
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    fn string(&self, key: &str, errors: &mut Errors) -> Option<String> {
        match self.get(key)? {
            Value::String(s) => Some(s.clone()),
            _ => {
                errors.push(&self.key(key), "is invalid");
                None
            }
        }
    }

    fn token(&self, key: &str, errors: &mut Errors) -> Option<String> {
        let value = self.string(key, errors)?;
        if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            errors.push(&self.key(key), "is invalid");
            return None;
        }
        Some(value)
    }

    fn list(&self, key: &str, errors: &mut Errors) -> Option<Vec<String>> {
        let value = self.get(key)?;
        let list = cast_string_list(value);
        if list.is_none() {
            errors.push(&self.key(key), "is invalid");
        }
        list
    }

    fn positive(&self, key: &str, errors: &mut Errors) -> Option<u64> {
        let value = self.get(key)?;
        match cast_integer(value) {
            Some(n) if n > 0 => Some(n as u64),
            Some(_) => {
                errors.push(&self.key(key), "must be greater than 0");
                None
            }
            None => {
                errors.push(&self.key(key), "is invalid");
                None
            }
        }
    }

    fn positive_u32(&self, key: &str, errors: &mut Errors) -> Option<u32> {
        let value = self.positive(key, errors)?;
        match u32::try_from(value) {
            Ok(v) => Some(v),
            Err(_) => {
                errors.push(&self.key(key), "is invalid");
                None
            }
        }
    }

    fn boolean(&self, key: &str, errors: &mut Errors) -> Option<bool> {
        let value = self.get(key)?;
        let cast = cast_boolean(value);
        if cast.is_none() {
            errors.push(&self.key(key), "is invalid");
        }
        cast
    }

    fn env_name(&self, key: &str, errors: &mut Errors) -> Option<String> {
        let value = self.string(key, errors)?;
        // `$NAME` is accepted too, for symmetry with other `$VAR` settings.
        let name = value.strip_prefix('$').unwrap_or(&value).to_owned();
        if env::valid_env_name(&name) {
            Some(name)
        } else {
            errors.push(&self.key(key), "is invalid");
            None
        }
    }

    fn price(&self, key: &str, errors: &mut Errors) -> Option<f64> {
        let value = self.get(key)?;
        match cast_number(value) {
            Some(n) if n >= 0.0 => Some(n),
            Some(_) => {
                errors.push(&self.key(key), "must be greater than or equal to 0");
                None
            }
            None => {
                errors.push(&self.key(key), "is invalid");
                None
            }
        }
    }
}

fn cast_pricing(entry: &Entry<'_>, errors: &mut Errors) -> Option<BackendPricing> {
    let value = entry.get("pricing")?;
    let Value::Object(map) = value else {
        errors.push(&entry.key("pricing"), "is invalid");
        return None;
    };
    let pricing = Entry {
        path: entry.key("pricing"),
        map,
    };
    Some(BackendPricing {
        input_usd_per_million_tokens: pricing
            .price("input_usd_per_million_tokens", errors)
            .unwrap_or(0.0),
        output_usd_per_million_tokens: pricing
            .price("output_usd_per_million_tokens", errors)
            .unwrap_or(0.0),
    })
}

fn cast_claude(entry: &Entry<'_>, errors: &mut Errors) -> ClaudeCodeSettings {
    let mut out = ClaudeCodeSettings::default();
    if let Some(command) = entry.string("command", errors) {
        if command.trim().is_empty() {
            errors.push(&entry.key("command"), "can't be blank");
        } else {
            out.command = command;
        }
    }
    out.allowed_tools = entry.list("allowed_tools", errors).unwrap_or_default();
    out.disallowed_tools = entry.list("disallowed_tools", errors).unwrap_or_default();
    if let Some(mode) = entry.string("permission_mode", errors) {
        match mode.as_str() {
            "default" | "acceptEdits" | "plan" | "bypassPermissions" | "dontAsk" => {
                out.permission_mode = mode
            }
            _ => errors.push(&entry.key("permission_mode"), "is invalid"),
        }
    }
    out.max_turns = entry.positive_u32("max_turns", errors);
    out.api_key_env = entry.env_name("api_key_env", errors);
    out.extra_args = entry.list("extra_args", errors).unwrap_or_default();
    out
}

fn cast_api(entry: &Entry<'_>, errors: &mut Errors) -> Option<ApiBackendSettings> {
    let before = errors.len();
    let provider = match entry.string("provider", errors).as_deref() {
        Some("anthropic") => Some(ApiProvider::Anthropic),
        Some("openai") => Some(ApiProvider::OpenAi),
        Some(_) => {
            errors.push(&entry.key("provider"), "is invalid");
            None
        }
        None => {
            errors.push(&entry.key("provider"), "can't be blank");
            None
        }
    };
    let base_url = entry.token("base_url", errors);
    if let Some(url) = &base_url
        && !http_url(url)
    {
        errors.push(&entry.key("base_url"), "is invalid");
    }
    let headers = match entry.get("headers") {
        None => BTreeMap::new(),
        Some(Value::Object(map)) => {
            let mut headers = BTreeMap::new();
            for (name, value) in map {
                match value.as_str() {
                    Some(v)
                        if !name.is_empty()
                            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                            && !v.contains(['\r', '\n']) =>
                    {
                        headers.insert(name.clone(), v.to_owned());
                    }
                    _ => errors.push(&entry.key("headers"), "is invalid"),
                }
            }
            headers
        }
        Some(_) => {
            errors.push(&entry.key("headers"), "is invalid");
            BTreeMap::new()
        }
    };
    let tools = match entry.list("tools", errors) {
        Some(tools) => {
            if tools.iter().all(|t| API_TOOLS.contains(&t.as_str())) {
                env::uniq(tools)
            } else {
                errors.push(&entry.key("tools"), "is invalid");
                Vec::new()
            }
        }
        None => API_TOOLS.iter().map(|t| (*t).to_owned()).collect(),
    };
    let sandbox = match entry.string("sandbox", errors).as_deref() {
        None | Some("auto") => SandboxMode::Auto,
        Some("bubblewrap") => SandboxMode::Bubblewrap,
        Some("seatbelt") => SandboxMode::Seatbelt,
        Some("none") => SandboxMode::None,
        Some(_) => {
            errors.push(&entry.key("sandbox"), "is invalid");
            SandboxMode::Auto
        }
    };
    let temperature = match entry.get("temperature") {
        None => None,
        Some(value) => match cast_number(value) {
            Some(t) if (0.0..=2.0).contains(&t) => Some(t),
            _ => {
                errors.push(&entry.key("temperature"), "is invalid");
                None
            }
        },
    };
    let api = ApiBackendSettings {
        provider: provider.unwrap_or(ApiProvider::OpenAi),
        base_url: base_url.unwrap_or_else(|| match provider {
            Some(ApiProvider::Anthropic) => ANTHROPIC_BASE_URL.to_owned(),
            _ => OPENAI_BASE_URL.to_owned(),
        }),
        api_key_env: entry.env_name("api_key_env", errors),
        headers,
        tools,
        max_steps: entry.positive_u32("max_steps", errors).unwrap_or(50),
        max_output_tokens: entry
            .positive_u32("max_output_tokens", errors)
            .unwrap_or(8192),
        temperature,
        stream: entry.boolean("stream", errors).unwrap_or(true),
        request_timeout_ms: entry
            .positive("request_timeout_ms", errors)
            .unwrap_or(600_000),
        sandbox,
        network: entry.boolean("network", errors).unwrap_or(false),
        shell_timeout_ms: entry
            .positive("shell_timeout_ms", errors)
            .unwrap_or(600_000),
    };
    (errors.len() == before).then_some(api)
}

fn cast_entry(name: &str, value: &Value, errors: &mut Errors) -> Option<BackendSettings> {
    let path = format!("backends.{name}");
    if !valid_name(name) {
        errors.push(&path, "is invalid");
        return None;
    }
    let Value::Object(map) = value else {
        errors.push(&path, "is invalid");
        return None;
    };
    let entry = Entry { path, map };
    let before = errors.len();
    let kind = match entry.string("kind", errors).as_deref() {
        Some("codex") => Some(BackendKind::Codex),
        Some("claude_code") => Some(BackendKind::ClaudeCode(cast_claude(&entry, errors))),
        Some("api") => cast_api(&entry, errors).map(BackendKind::Api),
        Some(_) => {
            errors.push(&entry.key("kind"), "is invalid");
            None
        }
        None => {
            errors.push(&entry.key("kind"), "can't be blank");
            None
        }
    };
    let model = entry.token("model", errors);
    if matches!(kind, Some(BackendKind::Api(_))) && model.is_none() && errors.len() == before {
        errors.push(&entry.key("model"), "can't be blank");
    }
    let settings = BackendSettings {
        name: name.to_owned(),
        kind: kind?,
        model,
        prompt_template: entry.string("prompt_template", errors),
        system_prompt: entry.string("system_prompt", errors),
        pricing: cast_pricing(&entry, errors),
        turn_timeout_ms: entry
            .positive("turn_timeout_ms", errors)
            .unwrap_or(3_600_000),
    };
    (errors.len() == before).then_some(settings)
}

/// Casts `backends:` (a map of name to entry) and adds the built-in `codex` entry.
pub(crate) fn cast_backends(
    root: &Map<String, Value>,
    errors: &mut Errors,
) -> BTreeMap<String, BackendSettings> {
    let mut out = BTreeMap::new();
    match root.get("backends") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (name, value) in map {
                if let Some(entry) = cast_entry(name, value, errors) {
                    out.insert(name.clone(), entry);
                }
            }
        }
        Some(_) => errors.push("backends", "is invalid"),
    }
    out.entry(CODEX_BACKEND.to_owned())
        .or_insert_with(BackendSettings::codex);
    out
}

/// Env var names that hold host-side API keys of `api` backends (removed from every child).
pub fn backend_secret_environment_names(
    backends: &BTreeMap<String, BackendSettings>,
) -> Vec<String> {
    env::uniq(backends.values().filter_map(|b| match &b.kind {
        BackendKind::Api(api) => api.api_key_env.clone(),
        _ => None,
    }))
}
