//! GitHub Issues adapter (`SymphonyElixir.GitHub.{Adapter, Client, AgentTool}`).
//!
//! - Provider keys `repo` (`owner/name`, env `GITHUB_REPO`), `token` (env `GITHUB_TOKEN`), `api_url`
//!   (default `https://api.github.com`, https only); resolved on every call.
//! - Headers: `Accept: application/vnd.github+json`, `Authorization: Bearer <token>`,
//!   `X-GitHub-Api-Version: 2022-11-28`, `User-Agent: symphony`.
//! - States: only `open`/`closed` map to the API `state` filter (`all` for both); anything else reads
//!   nothing. Pages of 100 sorted by creation, stopping at the first short page.
//! - Pull requests come back from the Issues API and are kept with `dispatchable: false`.
//! - One agent tool, `github_api` (raw REST passthrough).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use symphony_core::config::tracker::validate_tracker;
use symphony_core::config::{
    GitHubSettings, TrackerSettings, resolve_github, secret_environment_names,
};
use symphony_core::issue::{normalize_labels, parse_datetime};
use symphony_core::{EnvSource, Issue, TrackerConfigError};

use crate::error::{Provider, TrackerError};
use crate::rest::{
    RestResponse, RestToolMessages, RestToolSpec, encode_unreserved, map_status,
    normalize_candidate_page, opt_string, parse_positive_id, present_str, query_pairs, state_set,
};
use crate::tool::{ToolContext, ToolResult};
use crate::transport::{HttpClient, HttpRequest, Method};
use crate::{MAX_PAGES, Tracker};

/// Page size for state reads.
pub const PAGE_SIZE: usize = 100;
/// `X-GitHub-Api-Version` header value.
pub const API_VERSION: &str = "2022-11-28";

/// The `github_api` tool.
pub static TOOL: RestToolSpec = RestToolSpec {
    provider: Provider::Github,
    name: "github_api",
    description: "Execute a GitHub REST API request using Symphony's configured auth.\n",
    methods: &["GET", "POST", "PATCH", "PUT", "DELETE"],
    method_description: "GitHub REST method.",
    path_description: "GitHub REST path such as /repos/owner/repo/issues/1/comments.",
    query_key: "params",
    path_prefix: "/",
    missing_auth: TrackerConfigError::MissingGithubToken,
    messages: RestToolMessages {
        invalid_arguments: "`github_api` expects an object with `method` and `path`.",
        invalid_method: "`github_api.method` must be GET, POST, PATCH, PUT, or DELETE.",
        invalid_path: "`github_api.path` must be a relative GitHub REST path.",
        invalid_query: "`github_api.params` must be a JSON object when provided.",
        missing_auth: "Symphony is missing GitHub auth. Set `tracker.provider.token` in `WORKFLOW.md` or export `GITHUB_TOKEN`.",
        request_failed: "GitHub API request failed before receiving a successful response.",
        execution_failed: "GitHub API tool execution failed.",
    },
};

/// The `github` adapter.
#[derive(Debug, Clone)]
pub struct GitHubTracker {
    http: HttpClient,
    env: Arc<dyn EnvSource>,
}

impl GitHubTracker {
    /// Adapter over `http`, resolving `$VAR`/default env vars through `env`.
    pub fn new(http: HttpClient, env: Arc<dyn EnvSource>) -> Self {
        Self { http, env }
    }

    fn settings(&self, settings: &TrackerSettings) -> Result<GitHubSettings, TrackerError> {
        resolve_github(settings, self.env.as_ref()).map_err(Into::into)
    }

    /// `Client.request/5`: resolves settings, then performs one REST call and returns its status and
    /// body without status mapping (used by `github_api`; the write path for agents).
    pub async fn request(
        &self,
        settings: &TrackerSettings,
        method: Method,
        path: &str,
        params: &Map<String, Value>,
        body: Option<Value>,
    ) -> Result<RestResponse, TrackerError> {
        let resolved = self.settings(settings)?;
        self.perform(&resolved, method, path, params, body).await
    }

    async fn perform(
        &self,
        settings: &GitHubSettings,
        method: Method,
        path: &str,
        params: &Map<String, Value>,
        body: Option<Value>,
    ) -> Result<RestResponse, TrackerError> {
        if method == Method::Head {
            return Err(TrackerError::InvalidMethod(Provider::Github));
        }
        let query = query_pairs(params).ok_or(TrackerError::UnknownPayload(Provider::Github))?;
        let request = HttpRequest::new(method, format!("{}{}", settings.api_url, path))
            .header("Accept", "application/vnd.github+json")
            .credential_header(
                "Authorization",
                format!("Bearer {}", settings.token),
                settings.token.clone(),
            )
            .header("X-GitHub-Api-Version", API_VERSION)
            .header("User-Agent", "symphony")
            .query(query)
            .json(body);
        let response = self
            .http
            .send(request)
            .await
            .map_err(|err| TrackerError::request(Provider::Github, err))?;
        Ok(RestResponse {
            status: response.status,
            body: response.body,
        })
    }

    async fn get(
        &self,
        settings: &GitHubSettings,
        path: &str,
        params: Map<String, Value>,
        allow_not_found: bool,
    ) -> Result<Option<Value>, TrackerError> {
        let response = self
            .perform(settings, Method::Get, path, &params, None)
            .await?;
        map_status(
            Provider::Github,
            Method::Get,
            path,
            response,
            allow_not_found,
        )
    }
}

fn issues_path(settings: &GitHubSettings) -> String {
    let encoded = match settings.repo.split_once('/') {
        Some((owner, name)) => format!("{}/{}", encode_unreserved(owner), encode_unreserved(name)),
        None => encode_unreserved(&settings.repo),
    };
    format!("/repos/{encoded}/issues")
}

fn state_query(requested: &std::collections::HashSet<String>) -> Option<&'static str> {
    match (requested.contains("open"), requested.contains("closed")) {
        (true, true) => Some("all"),
        (true, false) => Some("open"),
        (false, true) => Some("closed"),
        (false, false) => None,
    }
}

/// Normalizes a GitHub issue JSON object; `None` unless `number` is a positive integer and `title`
/// and `state` are non-blank strings.
pub fn normalize_issue(raw: &Value, repo: &str) -> Option<Issue> {
    let obj = raw.as_object()?;
    let number = obj.get("number")?.as_i64().filter(|n| *n > 0)?;
    let title = present_str(obj.get("title"))?;
    let state = present_str(obj.get("state"))?;
    let native_ref = crate::rest::native_ref(vec![
        ("id", obj.get("id").cloned()),
        ("node_id", obj.get("node_id").cloned()),
        ("number", obj.get("number").cloned()),
        ("repo", Some(Value::String(repo.to_owned()))),
    ]);
    Some(Issue {
        id: Some(number.to_string()),
        native_ref: (!native_ref.is_empty()).then_some(native_ref),
        identifier: Some(format!("GH-{number}")),
        title: Some(title.to_owned()),
        description: opt_string(obj.get("body")),
        priority: None,
        state: Some(state.to_owned()),
        branch_name: None,
        url: opt_string(obj.get("html_url")),
        assignee_id: raw
            .pointer("/assignee/login")
            .and_then(Value::as_str)
            .map(str::to_owned),
        blocked_by: Vec::new(),
        labels: extract_labels(obj.get("labels")),
        dispatchable: !obj.contains_key("pull_request"),
        created_at: obj
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_datetime),
        updated_at: obj
            .get("updated_at")
            .and_then(Value::as_str)
            .and_then(parse_datetime),
    })
}

fn extract_labels(labels: Option<&Value>) -> Vec<String> {
    let Some(labels) = labels.and_then(Value::as_array) else {
        return Vec::new();
    };
    normalize_labels(labels.iter().filter_map(|label| match label {
        Value::String(name) => Some(name.as_str()),
        Value::Object(map) => map.get("name").and_then(Value::as_str),
        _ => None,
    }))
}

#[async_trait]
impl Tracker for GitHubTracker {
    fn kind(&self) -> &'static str {
        "github"
    }

    /// States must be explicit lists of `open` (active) / `closed` (terminal); then client settings.
    fn validate_config(&self, settings: &TrackerSettings) -> Result<(), TrackerError> {
        validate_tracker(settings, self.env.as_ref()).map_err(Into::into)
    }

    async fn fetch_issues_by_states(
        &self,
        settings: &TrackerSettings,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let requested = state_set(states);
        let Some(query_state) = state_query(&requested) else {
            return Ok(Vec::new());
        };
        let resolved = self.settings(settings)?;
        let path = issues_path(&resolved);
        let mut issues = Vec::new();
        for page in 1..=MAX_PAGES {
            let params = Map::from_iter([
                ("state".to_owned(), json!(query_state)),
                ("per_page".to_owned(), json!(PAGE_SIZE)),
                ("page".to_owned(), json!(page)),
                ("sort".to_owned(), json!("created")),
                ("direction".to_owned(), json!("asc")),
            ]);
            let payload = self.get(&resolved, &path, params, false).await?;
            let Some(Value::Array(raw)) = payload else {
                return Err(TrackerError::UnknownPayload(Provider::Github));
            };
            issues.extend(normalize_candidate_page(
                &raw,
                "GitHub issue",
                &requested,
                |item| normalize_issue(item, &resolved.repo),
            ));
            if raw.len() < PAGE_SIZE {
                return Ok(issues);
            }
        }
        Err(TrackerError::PaginationLimitExceeded(Provider::Github))
    }

    async fn fetch_issues_by_ids(
        &self,
        settings: &TrackerSettings,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let ids = crate::rest::uniq(ids);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let resolved = self.settings(settings)?;
        let base = issues_path(&resolved);
        let mut issues = Vec::new();
        for id in &ids {
            let number =
                parse_positive_id(id).ok_or(TrackerError::InvalidIssueId(Provider::Github))?;
            let path = format!("{base}/{number}");
            match self.get(&resolved, &path, Map::new(), true).await? {
                None => {}
                Some(raw @ Value::Object(_)) => issues.push(
                    normalize_issue(&raw, &resolved.repo)
                        .ok_or(TrackerError::UnknownPayload(Provider::Github))?,
                ),
                Some(_) => return Err(TrackerError::UnknownPayload(Provider::Github)),
            }
        }
        Ok(issues)
    }

    /// `GITHUB_TOKEN`, `GH_TOKEN`, `GITHUB_ENTERPRISE_TOKEN`, `GH_ENTERPRISE_TOKEN` plus a `$VAR`
    /// used for `provider.token`.
    fn secret_environment_names(&self, settings: &TrackerSettings) -> Vec<String> {
        secret_environment_names(settings)
    }

    fn agent_tool_specs(&self) -> Vec<Value> {
        vec![TOOL.spec()]
    }

    async fn execute_agent_tool(
        &self,
        tool: Option<&str>,
        arguments: &Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        if tool != Some(TOOL.name) {
            return TOOL.unsupported(tool);
        }
        let call = match TOOL.normalize(arguments) {
            Ok(call) => call,
            Err(err) => return TOOL.argument_error(err),
        };
        match self
            .request(
                &ctx.settings,
                call.method,
                &call.path,
                &call.query,
                call.body,
            )
            .await
        {
            Ok(response) => TOOL.response(&response),
            Err(err) => TOOL.client_error(&err),
        }
    }
}
