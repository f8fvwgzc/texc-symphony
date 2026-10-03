//! Asana adapter (`SymphonyElixir.Asana.{Adapter, Client, AgentTool}`).
//!
//! - Provider keys `endpoint` (default `https://app.asana.com/api/1.0`, https only, no `$VAR`),
//!   `api_key` (env `ASANA_PAT`), `project_gid`.
//! - Auth: `Authorization: Bearer <api_key>`, `Accept: application/json`.
//! - State = the name of the task's section in the configured project. Candidate reads page through
//!   every task of the project (`offset` paging, 100 per page) and filter sections client-side;
//!   completed tasks are returned with `dispatchable: false`.
//! - Id refresh: one `GET /tasks/{gid}` per id; 404 and out-of-project tasks are omitted.
//! - One agent tool, `asana_api` (raw REST passthrough).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use symphony_core::config::tracker::validate_tracker;
use symphony_core::config::{
    AsanaSettings, TrackerSettings, resolve_asana, secret_environment_names,
};
use symphony_core::issue::{normalize_labels, parse_datetime};
use symphony_core::{EnvSource, Issue, TrackerConfigError};

use crate::error::{Provider, TrackerError};
use crate::rest::{
    RestResponse, RestToolMessages, RestToolSpec, encode_unreserved, map_status,
    normalize_candidate_page, opt_string, present_str, query_pairs, state_set, trimmed_string,
    uniq,
};
use crate::tool::{ToolContext, ToolResult};
use crate::transport::{HttpClient, HttpRequest, Method};
use crate::{MAX_PAGES, Tracker};

/// Page size (`limit`).
pub const PAGE_SIZE: usize = 100;

/// `opt_fields` sent with every task read.
pub const TASK_FIELDS: &str = "gid,name,notes,completed,resource_subtype,assignee.gid,tags.name,memberships.project.gid,memberships.section.gid,memberships.section.name,permalink_url,created_at,modified_at";

/// The `asana_api` tool.
pub static TOOL: RestToolSpec = RestToolSpec {
    provider: Provider::Asana,
    name: "asana_api",
    description: "Execute an Asana REST API request using Symphony's configured auth.\n",
    methods: &["GET", "POST", "PUT", "DELETE"],
    method_description: "Asana REST method.",
    path_description: "Asana REST path such as /tasks/{task_gid}/stories.",
    query_key: "query",
    path_prefix: "/",
    missing_auth: TrackerConfigError::MissingAsanaApiKey,
    messages: RestToolMessages {
        invalid_arguments: "asana_api expects an object with method and path.",
        invalid_method: "asana_api.method must be GET, POST, PUT, or DELETE.",
        invalid_path: "asana_api.path must be a relative Asana REST path.",
        invalid_query: "asana_api.query must be a JSON object when provided.",
        missing_auth: "Symphony is missing Asana auth. Set tracker.provider.api_key or export ASANA_PAT.",
        request_failed: "Asana API request failed before receiving a successful response.",
        execution_failed: "Asana REST tool execution failed.",
    },
};

/// The `asana` adapter.
#[derive(Debug, Clone)]
pub struct AsanaTracker {
    http: HttpClient,
    env: Arc<dyn EnvSource>,
}

impl AsanaTracker {
    /// Adapter over `http`, resolving `$VAR`/default env vars through `env`.
    pub fn new(http: HttpClient, env: Arc<dyn EnvSource>) -> Self {
        Self { http, env }
    }

    fn settings(&self, settings: &TrackerSettings) -> Result<AsanaSettings, TrackerError> {
        resolve_asana(settings, self.env.as_ref()).map_err(Into::into)
    }

    /// `Client.request/5`: resolves settings, then performs one REST call and returns its status and
    /// body without status mapping (used by `asana_api`; the write path for agents).
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
        settings: &AsanaSettings,
        method: Method,
        path: &str,
        query: &Map<String, Value>,
        body: Option<Value>,
    ) -> Result<RestResponse, TrackerError> {
        if matches!(method, Method::Head | Method::Patch) {
            return Err(TrackerError::InvalidMethod(Provider::Asana));
        }
        let query = query_pairs(query).ok_or(TrackerError::UnknownPayload(Provider::Asana))?;
        let request = HttpRequest::new(method, format!("{}{}", settings.endpoint, path))
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
            .map_err(|err| TrackerError::request(Provider::Asana, err))?;
        Ok(RestResponse {
            status: response.status,
            body: response.body,
        })
    }

    async fn get(
        &self,
        settings: &AsanaSettings,
        path: &str,
        query: Map<String, Value>,
        allow_not_found: bool,
    ) -> Result<Option<Value>, TrackerError> {
        let response = self
            .perform(settings, Method::Get, path, &query, None)
            .await?;
        map_status(
            Provider::Asana,
            Method::Get,
            path,
            response,
            allow_not_found,
        )
    }
}

fn project_membership<'a>(task: &'a Value, project_gid: &str) -> Option<&'a Value> {
    task.get("memberships")?
        .as_array()?
        .iter()
        .find(|m| m.pointer("/project/gid").and_then(Value::as_str) == Some(project_gid))
}

/// `true` only for a well-formed task (`gid`, `name` present, `memberships` a list) with no membership
/// in the configured project.
fn task_outside_project(task: &Value, project_gid: &str) -> bool {
    present_str(task.get("gid")).is_some()
        && present_str(task.get("name")).is_some()
        && task.get("memberships").is_some_and(Value::is_array)
        && project_membership(task, project_gid).is_none()
}

/// Normalizes an Asana task; `None` unless `gid`, `name` and the section name of its membership in
/// the configured project are non-blank strings.
pub fn normalize_issue(task: &Value, settings: &AsanaSettings) -> Option<Issue> {
    let obj = task.as_object()?;
    let gid = present_str(obj.get("gid"))?;
    let name = present_str(obj.get("name"))?;
    let membership = project_membership(task, &settings.project_gid);
    let state = present_str(membership.and_then(|m| m.pointer("/section/name")))?;
    let native_ref = crate::rest::native_ref(vec![
        ("task_gid", Some(Value::String(gid.to_owned()))),
        (
            "project_gid",
            Some(Value::String(settings.project_gid.clone())),
        ),
        (
            "section_gid",
            membership.and_then(|m| m.pointer("/section/gid")).cloned(),
        ),
    ]);
    let tags = obj
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            normalize_labels(
                tags.iter()
                    .filter_map(|tag| tag.get("name").and_then(Value::as_str)),
            )
        })
        .unwrap_or_default();
    Some(Issue {
        id: Some(gid.to_owned()),
        native_ref: Some(native_ref),
        identifier: Some(format!("ASANA-{gid}")),
        title: Some(name.to_owned()),
        description: trimmed_string(obj.get("notes")),
        priority: None,
        state: Some(state.to_owned()),
        branch_name: None,
        url: opt_string(obj.get("permalink_url")),
        assignee_id: task
            .pointer("/assignee/gid")
            .and_then(Value::as_str)
            .map(str::to_owned),
        labels: tags,
        blocked_by: Vec::new(),
        dispatchable: obj.get("completed") == Some(&Value::Bool(false))
            && obj.get("resource_subtype").and_then(Value::as_str) != Some("section"),
        created_at: obj
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(parse_datetime),
        updated_at: obj
            .get("modified_at")
            .and_then(Value::as_str)
            .and_then(parse_datetime),
    })
}

enum PageNext {
    Done,
    Offset(String),
}

fn task_page(payload: &Value) -> Result<(&Vec<Value>, PageNext), TrackerError> {
    let tasks = payload
        .get("data")
        .and_then(Value::as_array)
        .ok_or(TrackerError::UnknownPayload(Provider::Asana))?;
    match payload.get("next_page") {
        Some(Value::Null) => Ok((tasks, PageNext::Done)),
        Some(next @ Value::Object(_)) => match next.get("offset").and_then(Value::as_str) {
            Some(offset) if !offset.is_empty() => Ok((tasks, PageNext::Offset(offset.to_owned()))),
            _ => Err(TrackerError::MissingPageCursor(Provider::Asana)),
        },
        _ => Err(TrackerError::UnknownPayload(Provider::Asana)),
    }
}

#[async_trait]
impl Tracker for AsanaTracker {
    fn kind(&self) -> &'static str {
        "asana"
    }

    /// State lists must be present (empty allowed) with non-blank entries; then client settings.
    fn validate_config(&self, settings: &TrackerSettings) -> Result<(), TrackerError> {
        validate_tracker(settings, self.env.as_ref()).map_err(Into::into)
    }

    async fn fetch_issues_by_states(
        &self,
        settings: &TrackerSettings,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let resolved = self.settings(settings)?;
        let requested = state_set(states);
        let path = format!(
            "/projects/{}/tasks",
            encode_unreserved(&resolved.project_gid)
        );
        let mut issues = Vec::new();
        let mut offset: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..MAX_PAGES {
            let mut query = Map::new();
            query.insert("limit".into(), json!(PAGE_SIZE));
            query.insert("opt_fields".into(), json!(TASK_FIELDS));
            if let Some(o) = &offset {
                query.insert("offset".into(), json!(o));
            }
            let payload = self
                .get(&resolved, &path, query, false)
                .await?
                .unwrap_or(Value::Null);
            let (raw, next) = task_page(&payload)?;
            issues.extend(normalize_candidate_page(
                raw,
                "Asana task",
                &requested,
                |task| normalize_issue(task, &resolved),
            ));
            match next {
                PageNext::Done => return Ok(issues),
                PageNext::Offset(next_offset) => {
                    if !seen.insert(next_offset.clone()) {
                        return Err(TrackerError::PaginationRepeatedCursor(Provider::Asana));
                    }
                    offset = Some(next_offset);
                }
            }
        }
        Err(TrackerError::PaginationLimitExceeded(Provider::Asana))
    }

    async fn fetch_issues_by_ids(
        &self,
        settings: &TrackerSettings,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let resolved = self.settings(settings)?;
        let mut issues = Vec::new();
        for id in uniq(ids) {
            let path = format!("/tasks/{}", encode_unreserved(&id));
            let mut query = Map::new();
            query.insert("opt_fields".into(), json!(TASK_FIELDS));
            let Some(payload) = self.get(&resolved, &path, query, true).await? else {
                continue;
            };
            let task = payload
                .get("data")
                .filter(|t| t.is_object())
                .ok_or(TrackerError::UnknownPayload(Provider::Asana))?;
            if task_outside_project(task, &resolved.project_gid) {
                continue;
            }
            issues.push(
                normalize_issue(task, &resolved)
                    .ok_or(TrackerError::UnknownPayload(Provider::Asana))?,
            );
        }
        Ok(issues)
    }

    /// `ASANA_PAT` plus a `$VAR` used for `provider.api_key`.
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
