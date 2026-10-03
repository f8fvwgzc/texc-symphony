//! GitLab Issues adapter (`SymphonyElixir.GitLab.{Adapter, Client, AgentTool}`).
//!
//! - Provider keys `project_path` (`group/project` or numeric id, env `GITLAB_PROJECT_PATH`),
//!   `api_key` (env `GITLAB_PAT`), `api_url` (default `https://gitlab.com/api/v4`, https only).
//! - Headers: `Accept: application/json`, `Authorization: Bearer <api_key>`; never `PRIVATE-TOKEN`.
//! - States: `opened`/`closed` (`all` for both); pages of 100 ordered by `created_at`.
//! - Every issue is dispatchable (closed, confidential and incident issues included).
//! - One agent tool, `gitlab_api` (raw REST passthrough; no `PATCH`).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use symphony_core::config::tracker::validate_tracker;
use symphony_core::config::{
    GitLabSettings, TrackerSettings, resolve_gitlab, secret_environment_names,
};
use symphony_core::issue::{normalize_labels, parse_datetime};
use symphony_core::{EnvSource, Issue, TrackerConfigError};

use crate::error::{Provider, TrackerError};
use crate::rest::{
    RestResponse, RestToolMessages, RestToolSpec, encode_unreserved, map_status,
    normalize_candidate_page, opt_string, parse_positive_id, present_str, query_pairs, state_set,
    trimmed_string, uniq,
};
use crate::tool::{ToolContext, ToolResult};
use crate::transport::{HttpClient, HttpRequest, Method};
use crate::{MAX_PAGES, Tracker};

/// Page size for state reads.
pub const PAGE_SIZE: usize = 100;

/// The `gitlab_api` tool.
pub static TOOL: RestToolSpec = RestToolSpec {
    provider: Provider::Gitlab,
    name: "gitlab_api",
    description: "Execute a GitLab REST API request using Symphony's configured auth.\n",
    methods: &["GET", "POST", "PUT", "DELETE"],
    method_description: "GitLab REST method.",
    path_description: "GitLab REST path such as /projects/group%2Frepo/issues/1/notes.",
    query_key: "query",
    path_prefix: "/",
    missing_auth: TrackerConfigError::MissingGitlabApiKey,
    messages: RestToolMessages {
        invalid_arguments: "gitlab_api expects an object with method and path.",
        invalid_method: "gitlab_api.method must be GET, POST, PUT, or DELETE.",
        invalid_path: "gitlab_api.path must be a relative GitLab REST path.",
        invalid_query: "gitlab_api.query must be a JSON object when provided.",
        missing_auth: "Symphony is missing GitLab auth. Set tracker.provider.api_key or export GITLAB_PAT.",
        request_failed: "GitLab API request failed before receiving a successful response.",
        execution_failed: "GitLab REST tool execution failed.",
    },
};

/// The `gitlab` adapter.
#[derive(Debug, Clone)]
pub struct GitLabTracker {
    http: HttpClient,
    env: Arc<dyn EnvSource>,
}

impl GitLabTracker {
    /// Adapter over `http`, resolving `$VAR`/default env vars through `env`.
    pub fn new(http: HttpClient, env: Arc<dyn EnvSource>) -> Self {
        Self { http, env }
    }

    fn settings(&self, settings: &TrackerSettings) -> Result<GitLabSettings, TrackerError> {
        resolve_gitlab(settings, self.env.as_ref()).map_err(Into::into)
    }

    /// `Client.request/5`: resolves settings, then performs one REST call and returns its status and
    /// body without status mapping (used by `gitlab_api`; the write path for agents).
    pub async fn request(
        &self,
        settings: &TrackerSettings,
        method: Method,
        path: &str,
        query: &Map<String, Value>,
        body: Option<Value>,
    ) -> Result<RestResponse, TrackerError> {
        let resolved = self.settings(settings)?;
        self.perform(&resolved, method, path, query, body).await
    }

    async fn perform(
        &self,
        settings: &GitLabSettings,
        method: Method,
        path: &str,
        query: &Map<String, Value>,
        body: Option<Value>,
    ) -> Result<RestResponse, TrackerError> {
        if matches!(method, Method::Head | Method::Patch) {
            return Err(TrackerError::InvalidMethod(Provider::Gitlab));
        }
        let query = query_pairs(query).ok_or(TrackerError::UnknownPayload(Provider::Gitlab))?;
        let request = HttpRequest::new(method, format!("{}{}", settings.api_url, path))
            .header("Accept", "application/json")
            .credential_header(
                "Authorization",
                format!("Bearer {}", settings.api_key),
                settings.api_key.clone(),
            )
            .query(query)
            .json(body);
        let response = self
            .http
            .send(request)
            .await
            .map_err(|err| TrackerError::request(Provider::Gitlab, err))?;
        Ok(RestResponse {
            status: response.status,
            body: response.body,
        })
    }

    async fn get(
        &self,
        settings: &GitLabSettings,
        path: &str,
        query: Map<String, Value>,
        allow_not_found: bool,
    ) -> Result<Option<Value>, TrackerError> {
        let response = self
            .perform(settings, Method::Get, path, &query, None)
            .await?;
        map_status(
            Provider::Gitlab,
            Method::Get,
            path,
            response,
            allow_not_found,
        )
    }
}

fn issues_path(settings: &GitLabSettings) -> String {
    format!(
        "/projects/{}/issues",
        encode_unreserved(&settings.project_path)
    )
}

fn state_query(requested: &HashSet<String>) -> Option<&'static str> {
    match (requested.contains("opened"), requested.contains("closed")) {
        (true, true) => Some("all"),
        (true, false) => Some("opened"),
        (false, true) => Some("closed"),
        (false, false) => None,
    }
}

/// Normalizes a GitLab issue JSON object; `None` unless `iid` is a positive integer and `title` and
/// `state` are non-blank strings.
pub fn normalize_issue(raw: &Value, project_path: &str) -> Option<Issue> {
    let obj = raw.as_object()?;
    let iid = obj.get("iid")?.as_i64().filter(|n| *n > 0)?;
    let title = present_str(obj.get("title"))?;
    let state = present_str(obj.get("state"))?;
    let native_ref = crate::rest::native_ref(vec![
        ("id", obj.get("id").cloned()),
        ("iid", obj.get("iid").cloned()),
        ("project_id", obj.get("project_id").cloned()),
        ("project_path", Some(Value::String(project_path.to_owned()))),
        ("references", obj.get("references").cloned()),
    ]);
    Some(Issue {
        id: Some(iid.to_string()),
        native_ref: Some(native_ref),
        identifier: Some(format!("GL-{iid}")),
        title: Some(title.to_owned()),
        description: trimmed_string(obj.get("description")),
        priority: None,
        state: Some(state.to_owned()),
        branch_name: None,
        url: opt_string(obj.get("web_url")),
        assignee_id: assignee_id(obj),
        blocked_by: Vec::new(),
        labels: obj
            .get("labels")
            .and_then(Value::as_array)
            .map(|labels| normalize_labels(labels.iter().filter_map(Value::as_str)))
            .unwrap_or_default(),
        dispatchable: true,
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

/// First `assignees[]` entry, else `assignee`; its integer `id` (as a string) or its `username`.
///
/// Elixir applied the id/username clauses to the *issue* when nobody was assigned and returned the
/// issue's own id; here an unassigned issue yields `None`.
fn assignee_id(issue: &Map<String, Value>) -> Option<String> {
    let person = match issue.get("assignees") {
        Some(Value::Array(list)) if list.first().is_some_and(Value::is_object) => list.first(),
        _ => issue.get("assignee").filter(|a| a.is_object()),
    }?;
    if let Some(id) = person.get("id").and_then(Value::as_i64) {
        return Some(id.to_string());
    }
    person
        .get("username")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[async_trait]
impl Tracker for GitLabTracker {
    fn kind(&self) -> &'static str {
        "gitlab"
    }

    /// States must be explicit lists of `opened` (active) / `closed` (terminal); then client settings.
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
            let query = Map::from_iter([
                ("state".to_owned(), json!(query_state)),
                ("per_page".to_owned(), json!(PAGE_SIZE)),
                ("page".to_owned(), json!(page)),
                ("order_by".to_owned(), json!("created_at")),
                ("sort".to_owned(), json!("asc")),
            ]);
            let payload = self.get(&resolved, &path, query, false).await?;
            let Some(Value::Array(raw)) = payload else {
                return Err(TrackerError::UnknownPayload(Provider::Gitlab));
            };
            issues.extend(normalize_candidate_page(
                &raw,
                "GitLab issue",
                &requested,
                |item| normalize_issue(item, &resolved.project_path),
            ));
            if raw.len() < PAGE_SIZE {
                return Ok(issues);
            }
        }
        Err(TrackerError::PaginationLimitExceeded(Provider::Gitlab))
    }

    async fn fetch_issues_by_ids(
        &self,
        settings: &TrackerSettings,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let ids = uniq(ids);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let resolved = self.settings(settings)?;
        let base = issues_path(&resolved);
        let mut issues = Vec::new();
        for id in &ids {
            let iid =
                parse_positive_id(id).ok_or(TrackerError::InvalidIssueId(Provider::Gitlab))?;
            let path = format!("{base}/{iid}");
            match self.get(&resolved, &path, Map::new(), true).await? {
                None => {}
                Some(raw @ Value::Object(_)) => issues.push(
                    normalize_issue(&raw, &resolved.project_path)
                        .ok_or(TrackerError::UnknownPayload(Provider::Gitlab))?,
                ),
                Some(_) => return Err(TrackerError::UnknownPayload(Provider::Gitlab)),
            }
        }
        Ok(issues)
    }

    /// `GITLAB_PAT`, `GITLAB_ACCESS_TOKEN`, `GITLAB_TOKEN`, `OAUTH_TOKEN` plus a `$VAR` used for
    /// `provider.api_key`.
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
