//! Per-tracker-kind settings resolution and preflight validation.
//!
//! These are the pure parts of the Elixir adapters' `validate_config/1`, `settings/1` and
//! `secret_environment_names/1`, kept here so `WorkflowStore` can refuse a bad workflow at boot and on
//! reload without depending on the network-facing tracker crate. Adapters reuse the resolved structs.

use std::fmt;

use serde_json::{Map, Value};

use super::schema::TrackerSettings;
use crate::env::{self, EnvSource, resolve_setting};
use crate::error::{ConfigError, TrackerConfigError as E};
use crate::issue::{normalize_state, present_string};

/// Supported `tracker.kind` values (exact, case-sensitive).
pub const SUPPORTED_TRACKER_KINDS: [&str; 6] =
    ["asana", "github", "gitlab", "jira", "linear", "memory"];
/// Default GitHub REST API base URL.
pub const GITHUB_DEFAULT_API_URL: &str = "https://api.github.com";
/// Default GitLab REST API base URL.
pub const GITLAB_DEFAULT_API_URL: &str = "https://gitlab.com/api/v4";
/// Default Asana REST API base URL.
pub const ASANA_DEFAULT_ENDPOINT: &str = "https://app.asana.com/api/1.0";

macro_rules! redacted_debug {
    ($ty:ident { $($plain:ident),* ; $($secret:ident),* }) => {
        impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($ty))
                    $(.field(stringify!($plain), &self.$plain))*
                    $(.field(stringify!($secret), &"<redacted>"))*
                    .finish()
            }
        }
    };
}

/// Resolved Linear settings (all four checks passed).
#[derive(Clone, PartialEq, Eq)]
pub struct LinearSettings {
    /// GraphQL endpoint.
    pub endpoint: String,
    /// API key (sent as the raw `Authorization` header value).
    pub api_key: String,
    /// Project slug.
    pub project_slug: String,
    /// Optional assignee routing filter: `me` (the API key's viewer) or a Linear user id.
    pub assignee: Option<String>,
}
redacted_debug!(LinearSettings { endpoint, project_slug, assignee ; api_key });

/// Resolved GitHub settings.
#[derive(Clone, PartialEq, Eq)]
pub struct GitHubSettings {
    /// API base URL with trailing `/` trimmed.
    pub api_url: String,
    /// `owner/name`.
    pub repo: String,
    /// Bearer token.
    pub token: String,
}
redacted_debug!(GitHubSettings { api_url, repo ; token });

/// Resolved GitLab settings.
#[derive(Clone, PartialEq, Eq)]
pub struct GitLabSettings {
    /// API base URL with trailing `/` trimmed.
    pub api_url: String,
    /// Personal access token.
    pub api_key: String,
    /// `group/project` path.
    pub project_path: String,
}
redacted_debug!(GitLabSettings { api_url, project_path ; api_key });

/// Resolved Jira Cloud settings.
#[derive(Clone, PartialEq, Eq)]
pub struct JiraSettings {
    /// Site base URL with trailing `/` trimmed.
    pub base_url: String,
    /// Account email (Basic auth user).
    pub email: String,
    /// API token (Basic auth password).
    pub api_token: String,
    /// Project key.
    pub project_key: String,
    /// Terminal states normalized (trim + lowercase, blanks dropped).
    pub terminal_states: Vec<String>,
}
redacted_debug!(JiraSettings { base_url, email, project_key, terminal_states ; api_token });

/// Resolved Asana settings.
#[derive(Clone, PartialEq, Eq)]
pub struct AsanaSettings {
    /// API base URL with trailing `/` trimmed.
    pub endpoint: String,
    /// Personal access token.
    pub api_key: String,
    /// Project gid.
    pub project_gid: String,
}
redacted_debug!(AsanaSettings { endpoint, project_gid ; api_key });

fn provider_str<'a>(provider: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    provider.get(key)
}

/// Elixir `provider["x"] || default`: `null`/`false`/absent fall back.
fn or_default<'a>(value: Option<&'a Value>, default: &'a str) -> Option<&'a str> {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Some(default),
        Some(Value::String(s)) => Some(s),
        Some(_) => None,
    }
}

/// Elixir `URI.parse/1`-based check: scheme `https` (case-insensitive) and a non-empty host;
/// optionally no query and no fragment.
pub fn is_https_url(value: &str, forbid_query_and_fragment: bool) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    if forbid_query_and_fragment && (rest.contains('?') || rest.contains('#')) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(stripped) = host_port.strip_prefix('[') {
        stripped.split(']').next().unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or_default()
    };
    !host.is_empty()
}

fn trim_trailing_slashes(value: &str) -> String {
    value.trim_end_matches('/').to_owned()
}

/// Linear `validate_config/1` + resolved settings.
pub fn resolve_linear(tracker: &TrackerSettings) -> Result<LinearSettings, E> {
    let endpoint = tracker
        .endpoint
        .as_deref()
        .filter(|e| present_string(Some(e)));
    let Some(endpoint) = endpoint else {
        return Err(E::InvalidLinearEndpoint);
    };
    let Some(api_key) = tracker
        .api_key
        .as_deref()
        .filter(|k| present_string(Some(k)))
    else {
        return Err(E::MissingLinearApiToken);
    };
    let Some(project_slug) = tracker
        .project_slug
        .as_deref()
        .filter(|s| present_string(Some(s)))
    else {
        return Err(E::MissingLinearProjectSlug);
    };
    // A non-string provider assignee is kept raw by Elixir and then rejected here.
    let raw_assignee_invalid = matches!(
        tracker.provider.get("assignee"),
        Some(v) if !v.is_null() && !v.is_string()
    );
    if raw_assignee_invalid
        || tracker
            .assignee
            .as_deref()
            .is_some_and(|a| !present_string(Some(a)))
    {
        return Err(E::InvalidLinearAssignee);
    }
    Ok(LinearSettings {
        endpoint: endpoint.to_owned(),
        api_key: api_key.to_owned(),
        project_slug: project_slug.to_owned(),
        assignee: tracker.assignee.clone(),
    })
}

fn validate_exact_states(
    states: Option<&Vec<String>>,
    allowed: &str,
    missing: E,
    invalid: E,
) -> Result<(), E> {
    let states = states.ok_or(missing)?;
    if states.iter().all(|s| normalize_state(s) == allowed) {
        Ok(())
    } else {
        Err(invalid)
    }
}

fn validate_present_states(states: Option<&Vec<String>>, missing: E, invalid: E) -> Result<(), E> {
    let states = states.ok_or(missing)?;
    if states.iter().all(|s| present_string(Some(s))) {
        Ok(())
    } else {
        Err(invalid)
    }
}

fn valid_github_repo(repo: &str) -> bool {
    match repo.split_once('/') {
        Some((owner, name)) => {
            let ok = |part: &str| {
                !part.is_empty() && !part.contains('/') && !part.chars().any(char::is_whitespace)
            };
            ok(owner) && ok(name)
        }
        None => false,
    }
}

/// GitHub `settings/1` (repo from `GITHUB_REPO`, token from `GITHUB_TOKEN` when not configured).
pub fn resolve_github(tracker: &TrackerSettings, env: &dyn EnvSource) -> Result<GitHubSettings, E> {
    let provider = &tracker.provider;
    let api_url = or_default(provider_str(provider, "api_url"), GITHUB_DEFAULT_API_URL);
    let repo = resolve_setting(provider.get("repo"), env.var("GITHUB_REPO"), env);
    let token = resolve_setting(provider.get("token"), env.var("GITHUB_TOKEN"), env);

    let Some(api_url) = api_url.filter(|u| is_https_url(u, false)) else {
        return Err(E::InvalidGithubApiUrl);
    };
    let Some(repo) = repo else {
        return Err(E::MissingGithubRepo);
    };
    if !valid_github_repo(&repo) {
        return Err(E::InvalidGithubRepo);
    }
    let Some(token) = token else {
        return Err(E::MissingGithubToken);
    };
    Ok(GitHubSettings {
        api_url: trim_trailing_slashes(api_url),
        repo,
        token,
    })
}

/// GitLab `settings/1` (project path from `GITLAB_PROJECT_PATH`, key from `GITLAB_PAT`).
pub fn resolve_gitlab(tracker: &TrackerSettings, env: &dyn EnvSource) -> Result<GitLabSettings, E> {
    let provider = &tracker.provider;
    let api_url = or_default(provider_str(provider, "api_url"), GITLAB_DEFAULT_API_URL);
    let project_path = resolve_setting(
        provider.get("project_path"),
        env.var("GITLAB_PROJECT_PATH"),
        env,
    );
    let api_key = resolve_setting(provider.get("api_key"), env.var("GITLAB_PAT"), env);

    let Some(api_url) = api_url.filter(|u| is_https_url(u, false)) else {
        return Err(E::InvalidGitlabApiUrl);
    };
    let Some(project_path) = project_path else {
        return Err(E::MissingGitlabProjectPath);
    };
    if project_path.contains([' ', '\t', '\n', '\r', '\0']) {
        return Err(E::InvalidGitlabProjectPath);
    }
    let Some(api_key) = api_key else {
        return Err(E::MissingGitlabApiKey);
    };
    Ok(GitLabSettings {
        api_url: trim_trailing_slashes(api_url),
        api_key,
        project_path,
    })
}

/// Jira `settings/1` (`JIRA_BASE_URL`, `JIRA_EMAIL`, `JIRA_API_TOKEN`; project key has no env fallback).
pub fn resolve_jira(tracker: &TrackerSettings, env: &dyn EnvSource) -> Result<JiraSettings, E> {
    let provider = &tracker.provider;
    let base_url = resolve_setting(provider.get("base_url"), env.var("JIRA_BASE_URL"), env);
    let email = resolve_setting(provider.get("email"), env.var("JIRA_EMAIL"), env);
    let api_token = resolve_setting(provider.get("api_token"), env.var("JIRA_API_TOKEN"), env);
    let project_key = resolve_setting(provider.get("project_key"), None, env);

    let Some(base_url) = base_url.filter(|u| is_https_url(u, true)) else {
        return Err(E::InvalidJiraBaseUrl);
    };
    let Some(email) = email else {
        return Err(E::MissingJiraEmail);
    };
    let Some(api_token) = api_token else {
        return Err(E::MissingJiraApiToken);
    };
    let Some(project_key) = project_key else {
        return Err(E::MissingJiraProjectKey);
    };
    let terminal_states = tracker
        .terminal_states
        .iter()
        .flatten()
        .map(|s| normalize_state(s))
        .filter(|s| !s.is_empty())
        .collect();
    Ok(JiraSettings {
        base_url: trim_trailing_slashes(&base_url),
        email,
        api_token,
        project_key,
        terminal_states,
    })
}

/// Asana `settings/1` (key from `ASANA_PAT`; project gid has no env fallback).
pub fn resolve_asana(tracker: &TrackerSettings, env: &dyn EnvSource) -> Result<AsanaSettings, E> {
    let provider = &tracker.provider;
    let endpoint = or_default(provider_str(provider, "endpoint"), ASANA_DEFAULT_ENDPOINT);
    let api_key = resolve_setting(provider.get("api_key"), env.var("ASANA_PAT"), env);
    let project_gid = resolve_setting(provider.get("project_gid"), None, env);

    let Some(endpoint) = endpoint.filter(|u| is_https_url(u, false)) else {
        return Err(E::InvalidAsanaEndpoint);
    };
    let Some(api_key) = api_key else {
        return Err(E::MissingAsanaApiKey);
    };
    let Some(project_gid) = project_gid else {
        return Err(E::MissingAsanaProjectGid);
    };
    Ok(AsanaSettings {
        endpoint: trim_trailing_slashes(endpoint),
        api_key,
        project_gid,
    })
}

/// `Tracker.validate_config/1` for the configured kind (assumes `kind` is present).
pub fn validate_tracker(tracker: &TrackerSettings, env: &dyn EnvSource) -> Result<(), ConfigError> {
    let kind = tracker
        .kind
        .as_deref()
        .ok_or(ConfigError::MissingTrackerKind)?;
    match kind {
        "memory" => Ok(()),
        "linear" => resolve_linear(tracker).map(drop).map_err(Into::into),
        "github" => {
            validate_exact_states(
                tracker.active_states.as_ref(),
                "open",
                E::MissingGithubActiveStates,
                E::InvalidGithubStates,
            )?;
            validate_exact_states(
                tracker.terminal_states.as_ref(),
                "closed",
                E::MissingGithubTerminalStates,
                E::InvalidGithubStates,
            )?;
            resolve_github(tracker, env).map(drop).map_err(Into::into)
        }
        "gitlab" => {
            validate_exact_states(
                tracker.active_states.as_ref(),
                "opened",
                E::MissingGitlabActiveStates,
                E::InvalidGitlabStates,
            )?;
            validate_exact_states(
                tracker.terminal_states.as_ref(),
                "closed",
                E::MissingGitlabTerminalStates,
                E::InvalidGitlabStates,
            )?;
            resolve_gitlab(tracker, env).map(drop).map_err(Into::into)
        }
        "jira" => {
            validate_present_states(
                tracker.active_states.as_ref(),
                E::MissingJiraActiveStates,
                E::InvalidJiraStates,
            )?;
            validate_present_states(
                tracker.terminal_states.as_ref(),
                E::MissingJiraTerminalStates,
                E::InvalidJiraStates,
            )?;
            resolve_jira(tracker, env).map(drop).map_err(Into::into)
        }
        "asana" => {
            validate_present_states(
                tracker.active_states.as_ref(),
                E::MissingAsanaActiveStates,
                E::InvalidAsanaStates,
            )?;
            validate_present_states(
                tracker.terminal_states.as_ref(),
                E::MissingAsanaTerminalStates,
                E::InvalidAsanaStates,
            )?;
            resolve_asana(tracker, env).map(drop).map_err(Into::into)
        }
        other => Err(ConfigError::UnsupportedTrackerKind(other.to_owned())),
    }
}

/// The adapters' `secret_environment_names/1`: env var names stripped from the Codex child process.
pub fn secret_environment_names(tracker: &TrackerSettings) -> Vec<String> {
    let provider = &tracker.provider;
    let (base, key): (&[&str], &str) = match tracker.kind.as_deref() {
        Some("linear") => return tracker.secret_environment_names.clone(),
        Some("github") => (
            &[
                "GITHUB_TOKEN",
                "GH_TOKEN",
                "GITHUB_ENTERPRISE_TOKEN",
                "GH_ENTERPRISE_TOKEN",
            ],
            "token",
        ),
        Some("gitlab") => (
            &[
                "GITLAB_PAT",
                "GITLAB_ACCESS_TOKEN",
                "GITLAB_TOKEN",
                "OAUTH_TOKEN",
            ],
            "api_key",
        ),
        Some("jira") => (&["JIRA_API_TOKEN"], "api_token"),
        Some("asana") => (&["ASANA_PAT"], "api_key"),
        _ => return Vec::new(),
    };
    let mut names: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    names.extend(env::env_reference_names([provider.get(key)]));
    env::uniq(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_url_checks_follow_uri_parse() {
        assert!(is_https_url("https://api.github.com", false));
        assert!(is_https_url("HTTPS://Host:1/x?", false));
        assert!(!is_https_url("HTTPS://Host:1/x?", true));
        assert!(!is_https_url("https://x.atlassian.net#frag", true));
        assert!(is_https_url("https://user@host/path", false));
        assert!(is_https_url("https://[::1]:8443/api", false));
        assert!(!is_https_url("http://api.github.com", false));
        assert!(!is_https_url("https://", false));
        assert!(!is_https_url("https:example.com", false));
        assert!(!is_https_url("api.github.com", false));
    }

    #[test]
    fn github_repo_shape() {
        assert!(valid_github_repo("openai/symphony"));
        assert!(!valid_github_repo("openai"));
        assert!(!valid_github_repo("openai/sym/phony"));
        assert!(!valid_github_repo("open ai/symphony"));
        assert!(!valid_github_repo("/symphony"));
    }
}
