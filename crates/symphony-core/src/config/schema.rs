//! Typed settings and the front-matter parser (`SymphonyElixir.Config.Schema`).

use std::collections::BTreeMap;
use std::fmt;

use serde::Serialize;
use serde_json::{Map, Value};

use super::cast::{Errors, Section};
use super::tracker::is_https_url;
use super::value::drop_nil_values;
use crate::env::{self, EnvSource};
use crate::error::ConfigError;
use crate::issue::normalize_state;

/// Default Linear GraphQL endpoint.
pub const LINEAR_ENDPOINT: &str = "https://api.linear.app/graphql";
/// Default active states for `linear` and `memory` trackers.
pub const LINEAR_ACTIVE_STATES: [&str; 2] = ["Todo", "In Progress"];
/// Default terminal states for `linear` and `memory` trackers.
pub const LINEAR_TERMINAL_STATES: [&str; 5] =
    ["Closed", "Cancelled", "Canceled", "Duplicate", "Done"];
/// Default `codex.command`.
pub const DEFAULT_CODEX_COMMAND: &str = "codex app-server";
/// Default `codex.thread_sandbox`.
pub const DEFAULT_THREAD_SANDBOX: &str = "workspace-write";
/// Default `server.host`.
pub const DEFAULT_SERVER_HOST: &str = "127.0.0.1";
/// Directory name of the default workspace root under the temp dir.
pub const DEFAULT_WORKSPACE_DIR_NAME: &str = "symphony_workspaces";

/// A value that is either a string or a map (Elixir `Schema.StringOrMap`), used by
/// `codex.approval_policy`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum StringOrMap {
    /// A plain string (any value, including `""`, is accepted and passed through).
    String(String),
    /// A map, passed through with string keys.
    Map(Map<String, Value>),
}

impl StringOrMap {
    /// `StringOrMap.cast/1`: strings and maps are accepted, anything else is `None` (`is invalid`).
    pub fn cast(value: &Value) -> Option<Self> {
        match value {
            Value::String(s) => Some(Self::String(s.clone())),
            Value::Object(m) => Some(Self::Map(m.clone())),
            _ => None,
        }
    }

    /// The value as JSON.
    pub fn to_value(&self) -> Value {
        match self {
            Self::String(s) => Value::String(s.clone()),
            Self::Map(m) => Value::Object(m.clone()),
        }
    }

    /// The string form, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            Self::Map(_) => None,
        }
    }
}

/// Default `codex.approval_policy`: Codex auto-rejects sandbox approvals, rule prompts and MCP
/// elicitations instead of asking (in `granular`, `false` means "do not prompt, reject").
///
/// Elixir sent `{reject: {...: true}}`; Codex renamed that variant to `granular` and inverted the
/// booleans, and current versions refuse `reject` as an unknown variant.
pub fn default_approval_policy() -> StringOrMap {
    let mut granular = Map::new();
    granular.insert("sandbox_approval".into(), Value::Bool(false));
    granular.insert("rules".into(), Value::Bool(false));
    granular.insert("mcp_elicitations".into(), Value::Bool(false));
    let mut policy = Map::new();
    policy.insert("granular".into(), Value::Object(granular));
    StringOrMap::Map(policy)
}

/// `<tmp>/symphony_workspaces` as a string.
pub fn default_workspace_root(env: &dyn EnvSource) -> String {
    env.tmp_dir()
        .join(DEFAULT_WORKSPACE_DIR_NAME)
        .to_string_lossy()
        .into_owned()
}

/// `tracker:` section after finalization.
#[derive(Clone, PartialEq, Default)]
pub struct TrackerSettings {
    /// Adapter kind (`linear`, `memory`, `github`, `gitlab`, `jira`, `asana`); case-sensitive.
    pub kind: Option<String>,
    /// Effective endpoint (`provider.endpoint`, else the legacy flat key; Linear defaults it).
    pub endpoint: Option<String>,
    /// Linear: resolved API key (`$VAR`/`LINEAR_API_KEY`). Other kinds: the raw flat value.
    pub api_key: Option<String>,
    /// Effective project slug (`provider.project_slug`, else the legacy flat key).
    pub project_slug: Option<String>,
    /// Linear: resolved assignee (`$VAR`/`LINEAR_ASSIGNEE`). Other kinds: the raw flat value.
    pub assignee: Option<String>,
    /// Adapter-owned provider map, preserved verbatim (raw `$VAR` strings kept). For Linear it always
    /// contains `endpoint`, `api_key`, `project_slug` and `assignee` (possibly `null`).
    pub provider: Map<String, Value>,
    /// Env var names to strip from the Codex child (computed; Linear only at parse time).
    pub secret_environment_names: Vec<String>,
    /// Required labels, trimmed + lowercased + deduplicated (blank labels kept as `""`).
    pub required_labels: Vec<String>,
    /// Active states (Linear/memory default when absent; `None` for other kinds when absent).
    pub active_states: Option<Vec<String>>,
    /// Terminal states (Linear/memory default when absent; `None` for other kinds when absent).
    pub terminal_states: Option<Vec<String>>,
}

impl fmt::Debug for TrackerSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted_provider: BTreeMap<&str, Value> = self
            .provider
            .iter()
            .map(|(k, v)| {
                let secret = is_secret_key(k) && !v.is_null();
                (
                    k.as_str(),
                    if secret {
                        Value::String("<redacted>".into())
                    } else {
                        v.clone()
                    },
                )
            })
            .collect();
        f.debug_struct("TrackerSettings")
            .field("kind", &self.kind)
            .field("endpoint", &self.endpoint)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("project_slug", &self.project_slug)
            .field("assignee", &self.assignee)
            .field("provider", &redacted_provider)
            .field("secret_environment_names", &self.secret_environment_names)
            .field("required_labels", &self.required_labels)
            .field("active_states", &self.active_states)
            .field("terminal_states", &self.terminal_states)
            .finish()
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["token", "key", "secret", "password"]
        .iter()
        .any(|needle| key.contains(needle))
}

/// `polling:` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollingSettings {
    /// Poll interval in ms (`> 0`, default 30 000).
    pub interval_ms: u64,
}

impl Default for PollingSettings {
    fn default() -> Self {
        Self {
            interval_ms: 30_000,
        }
    }
}

/// `workspace:` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSettings {
    /// Workspace root, kept **raw** (no `~`/relative resolution; see
    /// [`super::Settings::local_workspace_root`]). `$VAR` resolved; unset/empty falls back to the default.
    pub root: String,
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        Self {
            root: default_workspace_root(&crate::env::ProcessEnv),
        }
    }
}

/// `worker:` section (SSH workers).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WorkerSettings {
    /// `host` or `host:port` entries.
    pub ssh_hosts: Vec<String>,
    /// Per-host concurrency cap (`> 0`).
    pub max_concurrent_agents_per_host: Option<u32>,
}

/// `agent:` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSettings {
    /// Global concurrency (`> 0`, default 10).
    pub max_concurrent_agents: u32,
    /// Max turns per run (`> 0`, default 20).
    pub max_turns: u32,
    /// Retry backoff cap in ms (`> 0`, default 300 000).
    pub max_retry_backoff_ms: u64,
    /// Delay in ms before re-checking an issue whose run ended normally (`> 0`, default 1 000).
    pub continuation_delay_ms: u64,
    /// Per-state concurrency caps keyed by normalized (trim + lowercase) state name.
    pub max_concurrent_agents_by_state: BTreeMap<String, u32>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_concurrent_agents: 10,
            max_turns: 20,
            max_retry_backoff_ms: 300_000,
            continuation_delay_ms: 1_000,
            max_concurrent_agents_by_state: BTreeMap::new(),
        }
    }
}

/// `codex.provider`: the model provider Codex uses instead of its configured default.
///
/// Codex talks to it with the OpenAI Responses API, the only wire format current Codex versions
/// support for custom providers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexProvider {
    /// Provider id (`[A-Za-z0-9_-]+`), used as the Codex `model_providers` key.
    pub name: String,
    /// Base URL of the provider's OpenAI-compatible API (`http://` or `https://` with a host).
    pub base_url: String,
    /// Name of the env var holding the API key; Codex reads the value itself, Symphony never does.
    pub api_key_env: Option<String>,
}

/// `codex:` section.
#[derive(Clone, Debug, PartialEq)]
pub struct CodexSettings {
    /// Launch command, passed verbatim to `bash -lc` (no `~`/`$VAR` expansion here).
    pub command: String,
    /// Model override, passed to Codex as `--config model=...` when set.
    pub model: Option<String>,
    /// Model provider override, passed to Codex as `--config model_providers...` when set.
    pub provider: Option<CodexProvider>,
    /// Approval policy (string or map; no enum check, `""` accepted).
    pub approval_policy: StringOrMap,
    /// Thread sandbox (no enum check, `""` accepted).
    pub thread_sandbox: String,
    /// Explicit turn sandbox policy, passed through unchanged when set.
    pub turn_sandbox_policy: Option<Map<String, Value>>,
    /// Turn inactivity timeout in ms (`> 0`, default 3 600 000).
    pub turn_timeout_ms: u64,
    /// Read timeout in ms (`> 0`, default 5 000).
    pub read_timeout_ms: u64,
    /// Stall timeout in ms (`>= 0`, default 300 000; `0` disables stall detection).
    pub stall_timeout_ms: u64,
}

impl Default for CodexSettings {
    fn default() -> Self {
        Self {
            command: DEFAULT_CODEX_COMMAND.into(),
            model: None,
            provider: None,
            approval_policy: default_approval_policy(),
            thread_sandbox: DEFAULT_THREAD_SANDBOX.into(),
            turn_sandbox_policy: None,
            turn_timeout_ms: 3_600_000,
            read_timeout_ms: 5_000,
            stall_timeout_ms: 300_000,
        }
    }
}

/// `hooks:` section (shell scripts run with `sh -lc`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HooksSettings {
    /// Runs once when a workspace directory is newly created.
    pub after_create: Option<String>,
    /// Runs before every agent run.
    pub before_run: Option<String>,
    /// Runs after every agent run.
    pub after_run: Option<String>,
    /// Runs before a workspace is removed.
    pub before_remove: Option<String>,
    /// Hook timeout in ms (`> 0`, default 60 000).
    pub timeout_ms: u64,
}

impl Default for HooksSettings {
    fn default() -> Self {
        Self {
            after_create: None,
            before_run: None,
            after_run: None,
            before_remove: None,
            timeout_ms: 60_000,
        }
    }
}

/// `observability:` section (terminal dashboard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservabilitySettings {
    /// Dashboard enabled (default `true`).
    pub dashboard_enabled: bool,
    /// Refresh interval in ms (`> 0`, default 1 000).
    pub refresh_ms: u64,
    /// Minimum render interval in ms (`> 0`, default 16).
    pub render_interval_ms: u64,
}

impl Default for ObservabilitySettings {
    fn default() -> Self {
        Self {
            dashboard_enabled: true,
            refresh_ms: 1_000,
            render_interval_ms: 16,
        }
    }
}

/// `server:` section (HTTP API / dashboard).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSettings {
    /// Port (`None` = disabled, `0` = ephemeral).
    pub port: Option<u16>,
    /// Bind host (IP literal or DNS name; default `127.0.0.1`).
    pub host: String,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            port: None,
            host: DEFAULT_SERVER_HOST.into(),
        }
    }
}

/// The typed `WORKFLOW.md` front matter.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Settings {
    /// `tracker:`
    pub tracker: TrackerSettings,
    /// `polling:`
    pub polling: PollingSettings,
    /// `workspace:`
    pub workspace: WorkspaceSettings,
    /// `worker:`
    pub worker: WorkerSettings,
    /// `agent:`
    pub agent: AgentSettings,
    /// `codex:`
    pub codex: CodexSettings,
    /// `hooks:`
    pub hooks: HooksSettings,
    /// `observability:`
    pub observability: ObservabilitySettings,
    /// `server:`
    pub server: ServerSettings,
}

/// `Schema.normalize_state_limits/1`: keys become `trim + downcase`; values are untouched.
pub fn normalize_state_limits(limits: &Map<String, Value>) -> BTreeMap<String, Value> {
    limits
        .iter()
        .map(|(state, limit)| (normalize_state(state), limit.clone()))
        .collect()
}

/// `Schema.validate_state_limits/2`: one message per bad entry, in key order.
pub fn validate_state_limits(limits: &BTreeMap<String, Value>) -> Vec<&'static str> {
    limits
        .iter()
        .filter_map(|(state, limit)| {
            if state.trim().is_empty() {
                Some("state names must not be blank")
            } else if limit.as_i64().is_none_or(|n| n <= 0) {
                Some("limits must be positive integers")
            } else {
                None
            }
        })
        .collect()
}

/// `Schema.parse/1`: cast, validate and finalize the decoded front matter.
///
/// Unknown keys are ignored; `null` values behave as absent; errors are reported as
/// [`ConfigError::InvalidWorkflowConfig`] with `"dotted.path message"` entries joined by `", "`.
pub fn parse(config: &Map<String, Value>, env: &dyn EnvSource) -> Result<Settings, ConfigError> {
    let cleaned = drop_nil_values(Value::Object(config.clone()));
    let root = match &cleaned {
        Value::Object(map) => map,
        // `drop_nil_values` maps an object to an object.
        _ => {
            return Err(ConfigError::InvalidWorkflowConfig(
                "config is invalid".into(),
            ));
        }
    };

    let mut errors = Errors::default();
    let tracker = cast_tracker(&Section::new(root, "tracker", &mut errors), &mut errors);
    let polling = cast_polling(&Section::new(root, "polling", &mut errors), &mut errors);
    let workspace = cast_workspace(
        &Section::new(root, "workspace", &mut errors),
        &mut errors,
        env,
    );
    let worker = cast_worker(&Section::new(root, "worker", &mut errors), &mut errors);
    let agent = cast_agent(&Section::new(root, "agent", &mut errors), &mut errors);
    let codex = cast_codex(&Section::new(root, "codex", &mut errors), &mut errors);
    let hooks = cast_hooks(&Section::new(root, "hooks", &mut errors), &mut errors);
    let observability = cast_observability(
        &Section::new(root, "observability", &mut errors),
        &mut errors,
    );
    let server = cast_server(&Section::new(root, "server", &mut errors), &mut errors);

    if !errors.is_empty() {
        return Err(ConfigError::InvalidWorkflowConfig(errors.join()));
    }

    let settings = Settings {
        tracker,
        polling,
        workspace,
        worker,
        agent,
        codex,
        hooks,
        observability,
        server,
    };
    Ok(finalize(settings, env))
}

fn cast_tracker(s: &Section<'_>, errors: &mut Errors) -> TrackerSettings {
    let required_labels = s
        .string_list("required_labels", errors)
        .map(|labels| env::uniq(labels.iter().map(|l| normalize_state(l))))
        .unwrap_or_default();
    TrackerSettings {
        kind: s.string("kind", errors),
        endpoint: s.string("endpoint", errors),
        api_key: s.string("api_key", errors),
        project_slug: s.string("project_slug", errors),
        assignee: s.string("assignee", errors),
        provider: s.map("provider", errors).unwrap_or_default(),
        secret_environment_names: Vec::new(),
        required_labels,
        active_states: s.string_list("active_states", errors),
        terminal_states: s.string_list("terminal_states", errors),
    }
}

fn cast_polling(s: &Section<'_>, errors: &mut Errors) -> PollingSettings {
    let mut out = PollingSettings::default();
    let v = s.integer("interval_ms", errors);
    let v = s.positive("interval_ms", v, errors);
    if let Some(v) = s.fit("interval_ms", v, errors) {
        out.interval_ms = v;
    }
    out
}

fn cast_workspace(s: &Section<'_>, errors: &mut Errors, env: &dyn EnvSource) -> WorkspaceSettings {
    WorkspaceSettings {
        root: s
            .string("root", errors)
            .unwrap_or_else(|| default_workspace_root(env)),
    }
}

fn cast_worker(s: &Section<'_>, errors: &mut Errors) -> WorkerSettings {
    let ssh_hosts = s.string_list("ssh_hosts", errors).unwrap_or_default();
    let per_host = s.integer("max_concurrent_agents_per_host", errors);
    let per_host = s.positive("max_concurrent_agents_per_host", per_host, errors);
    WorkerSettings {
        ssh_hosts,
        max_concurrent_agents_per_host: s.fit("max_concurrent_agents_per_host", per_host, errors),
    }
}

fn cast_agent(s: &Section<'_>, errors: &mut Errors) -> AgentSettings {
    let mut out = AgentSettings::default();

    let v = s.integer("max_concurrent_agents", errors);
    let v = s.positive("max_concurrent_agents", v, errors);
    if let Some(v) = s.fit("max_concurrent_agents", v, errors) {
        out.max_concurrent_agents = v;
    }
    let v = s.integer("max_turns", errors);
    let v = s.positive("max_turns", v, errors);
    if let Some(v) = s.fit("max_turns", v, errors) {
        out.max_turns = v;
    }
    let v = s.integer("max_retry_backoff_ms", errors);
    let v = s.positive("max_retry_backoff_ms", v, errors);
    if let Some(v) = s.fit("max_retry_backoff_ms", v, errors) {
        out.max_retry_backoff_ms = v;
    }

    let v = s.integer("continuation_delay_ms", errors);
    let v = s.positive("continuation_delay_ms", v, errors);
    if let Some(v) = s.fit("continuation_delay_ms", v, errors) {
        out.continuation_delay_ms = v;
    }

    let field = "max_concurrent_agents_by_state";
    if let Some(raw) = s.map(field, errors) {
        let limits = normalize_state_limits(&raw);
        let messages = validate_state_limits(&limits);
        if messages.is_empty() {
            out.max_concurrent_agents_by_state = limits
                .into_iter()
                .filter_map(|(state, limit)| {
                    let limit = limit.as_i64().and_then(|n| u32::try_from(n).ok())?;
                    Some((state, limit))
                })
                .collect();
        } else {
            for message in messages {
                errors.push(&s.path(field), message);
            }
        }
    }
    out
}

fn cast_codex(s: &Section<'_>, errors: &mut Errors) -> CodexSettings {
    let mut out = CodexSettings::default();

    if let Some(command) = s.string("command", errors) {
        if command.trim().is_empty() {
            errors.push(&s.path("command"), "can't be blank");
        } else {
            out.command = command;
        }
    }
    if let Some(model) = s.string("model", errors) {
        if has_blank_or_control(&model) {
            errors.push(&s.path("model"), "is invalid");
        } else {
            out.model = Some(model);
        }
    }
    if let Some(provider) = s.map("provider", errors) {
        out.provider = cast_codex_provider(&provider, &s.path("provider"), errors);
    }
    if let Some(policy) = s.string_or_map("approval_policy", errors) {
        out.approval_policy = policy;
    }
    if let Some(sandbox) = s.string("thread_sandbox", errors) {
        out.thread_sandbox = sandbox;
    }
    out.turn_sandbox_policy = s.map("turn_sandbox_policy", errors);

    let v = s.integer("turn_timeout_ms", errors);
    let v = s.positive("turn_timeout_ms", v, errors);
    if let Some(v) = s.fit("turn_timeout_ms", v, errors) {
        out.turn_timeout_ms = v;
    }
    let v = s.integer("read_timeout_ms", errors);
    let v = s.positive("read_timeout_ms", v, errors);
    if let Some(v) = s.fit("read_timeout_ms", v, errors) {
        out.read_timeout_ms = v;
    }
    let v = s.integer("stall_timeout_ms", errors);
    let v = s.non_negative("stall_timeout_ms", v, errors);
    if let Some(v) = s.fit("stall_timeout_ms", v, errors) {
        out.stall_timeout_ms = v;
    }
    out
}

/// `true` for an empty value or one with whitespace or control characters.
fn has_blank_or_control(value: &str) -> bool {
    value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn cast_codex_provider(
    provider: &Map<String, Value>,
    path: &str,
    errors: &mut Errors,
) -> Option<CodexProvider> {
    let before = errors.len();
    let mut field =
        |key: &str, required: bool, valid: &dyn Fn(&str) -> bool| match provider.get(key) {
            None | Some(Value::Null) => {
                if required {
                    errors.push(&format!("{path}.{key}"), "can't be blank");
                }
                None
            }
            Some(Value::String(value)) if valid(value) => Some(value.clone()),
            Some(_) => {
                errors.push(&format!("{path}.{key}"), "is invalid");
                None
            }
        };
    let name = field("name", true, &|v| {
        !v.is_empty()
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    });
    let base_url = field("base_url", true, &|v| {
        !has_blank_or_control(v) && is_http_url(v)
    });
    let api_key_env = field("api_key_env", false, &env::valid_env_name);
    if errors.len() != before {
        return None;
    }
    Some(CodexProvider {
        name: name?,
        base_url: base_url?,
        api_key_env,
    })
}

/// `http://` or `https://` with a host (plain HTTP is allowed for local servers such as Ollama).
fn is_http_url(value: &str) -> bool {
    match value.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => {
            is_https_url(&format!("https://{rest}"), false)
        }
        _ => is_https_url(value, false),
    }
}

fn cast_hooks(s: &Section<'_>, errors: &mut Errors) -> HooksSettings {
    let mut out = HooksSettings {
        after_create: s.string("after_create", errors),
        before_run: s.string("before_run", errors),
        after_run: s.string("after_run", errors),
        before_remove: s.string("before_remove", errors),
        ..HooksSettings::default()
    };
    let v = s.integer("timeout_ms", errors);
    let v = s.positive("timeout_ms", v, errors);
    if let Some(v) = s.fit("timeout_ms", v, errors) {
        out.timeout_ms = v;
    }
    out
}

fn cast_observability(s: &Section<'_>, errors: &mut Errors) -> ObservabilitySettings {
    let mut out = ObservabilitySettings::default();
    if let Some(enabled) = s.boolean("dashboard_enabled", errors) {
        out.dashboard_enabled = enabled;
    }
    let v = s.integer("refresh_ms", errors);
    let v = s.positive("refresh_ms", v, errors);
    if let Some(v) = s.fit("refresh_ms", v, errors) {
        out.refresh_ms = v;
    }
    let v = s.integer("render_interval_ms", errors);
    let v = s.positive("render_interval_ms", v, errors);
    if let Some(v) = s.fit("render_interval_ms", v, errors) {
        out.render_interval_ms = v;
    }
    out
}

fn cast_server(s: &Section<'_>, errors: &mut Errors) -> ServerSettings {
    let mut out = ServerSettings::default();
    let port = s.integer("port", errors);
    if let Some(port) = s.non_negative("port", port, errors) {
        match u16::try_from(port) {
            Ok(port) => out.port = Some(port),
            Err(_) => errors.push(&s.path("port"), "must be less than or equal to 65535"),
        }
    }
    if let Some(host) = s.string("host", errors) {
        out.host = host;
    }
    out
}

/// `finalize_settings/1` (order matters; see blueprint A.3.2).
fn finalize(mut settings: Settings, env: &dyn EnvSource) -> Settings {
    let tracker = &mut settings.tracker;
    let mut provider = std::mem::take(&mut tracker.provider);

    match tracker.kind.as_deref() {
        Some("linear") => {
            let flat_endpoint = tracker
                .endpoint
                .clone()
                .unwrap_or_else(|| LINEAR_ENDPOINT.to_owned());
            put_new(&mut provider, "endpoint", Value::String(flat_endpoint));
            put_new(&mut provider, "api_key", opt_string(&tracker.api_key));
            put_new(
                &mut provider,
                "project_slug",
                opt_string(&tracker.project_slug),
            );
            put_new(&mut provider, "assignee", opt_string(&tracker.assignee));

            tracker.api_key = env::resolve_secret_setting(
                provider.get("api_key"),
                env.var("LINEAR_API_KEY"),
                env,
            );
            tracker.assignee = env::resolve_secret_setting(
                provider.get("assignee"),
                env.var("LINEAR_ASSIGNEE"),
                env,
            );
            let mut names = vec!["LINEAR_API_KEY".to_owned()];
            names.extend(env::env_reference_names([provider.get("api_key")]));
            tracker.secret_environment_names = env::uniq(names);
        }
        _ => {
            tracker.secret_environment_names = Vec::new();
        }
    }

    if matches!(tracker.kind.as_deref(), Some("linear" | "memory")) {
        tracker
            .active_states
            .get_or_insert_with(|| LINEAR_ACTIVE_STATES.iter().map(|s| s.to_string()).collect());
        tracker.terminal_states.get_or_insert_with(|| {
            LINEAR_TERMINAL_STATES
                .iter()
                .map(|s| s.to_string())
                .collect()
        });
    }

    if let Some(endpoint) = provider.get("endpoint") {
        tracker.endpoint = endpoint.as_str().map(str::to_owned);
    }
    if let Some(slug) = provider.get("project_slug") {
        tracker.project_slug = slug.as_str().map(str::to_owned);
    }
    tracker.provider = provider;

    let default_root = default_workspace_root(env);
    settings.workspace.root = env::resolve_path_value(&settings.workspace.root, &default_root, env);
    settings
}

fn put_new(map: &mut Map<String, Value>, key: &str, value: Value) {
    if !map.contains_key(key) {
        map.insert(key.to_owned(), value);
    }
}

fn opt_string(value: &Option<String>) -> Value {
    value.clone().map_or(Value::Null, Value::String)
}
