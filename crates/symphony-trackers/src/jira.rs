//! Jira Cloud adapter (`SymphonyElixir.Jira.{Adapter, Client, AgentTool}`).
//!
//! - Provider keys `base_url` (env `JIRA_BASE_URL`), `email` (env `JIRA_EMAIL`), `api_token`
//!   (env `JIRA_API_TOKEN`), `project_key`; all accept `$VAR`.
//! - Auth: `Authorization: Basic base64(email:api_token)`, `Accept: application/json`.
//! - Candidate reads: `POST /rest/api/3/search/jql` (enhanced search, `nextPageToken` paging, 100 per
//!   page) with `project = "KEY" AND status IN ("A", "B")`, plus a client-side state filter.
//! - Id refresh: `POST /rest/api/3/issue/bulkfetch` in batches of 100; out-of-project ids are omitted,
//!   malformed requested records fail the call.
//! - Blockers: inward `Blocks` links; Atlassian Document Format descriptions are flattened to text.
//! - One agent tool, `jira_rest` (raw REST v3 passthrough limited to `/rest/api/3/`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use symphony_core::config::tracker::validate_tracker;
use symphony_core::config::{
    JiraSettings, TrackerSettings, resolve_jira, secret_environment_names,
};
use symphony_core::issue::{normalize_labels, normalize_state, parse_datetime_compact_offset};
use symphony_core::{BlockerRef, EnvSource, Issue, TrackerConfigError};

use crate::error::{Provider, TrackerError};
use crate::rest::{
    RestResponse, RestToolMessages, RestToolSpec, encode_unreserved, map_status,
    normalize_candidate_page, present_str, query_pairs, state_set, uniq,
};
use crate::tool::{ToolContext, ToolResult};
use crate::transport::{HttpClient, HttpRequest, Method};
use crate::{MAX_PAGES, Tracker};

/// Page size for searches and bulk-fetch batches.
pub const PAGE_SIZE: usize = 100;

/// Fields requested for every issue (exact order).
pub const ISSUE_FIELDS: [&str; 9] = [
    "summary",
    "description",
    "status",
    "labels",
    "assignee",
    "created",
    "updated",
    "project",
    "issuelinks",
];

const SEARCH_PATH: &str = "/rest/api/3/search/jql";
const BULKFETCH_PATH: &str = "/rest/api/3/issue/bulkfetch";

/// The `jira_rest` tool.
pub static TOOL: RestToolSpec = RestToolSpec {
    provider: Provider::Jira,
    name: "jira_rest",
    description: "Execute a Jira Cloud REST v3 request using Symphony's configured auth.\n",
    methods: &["GET", "POST", "PUT", "DELETE"],
    method_description: "Jira REST method.",
    path_description: "Jira REST v3 path beginning with /rest/api/3/.",
    query_key: "query",
    path_prefix: "/rest/api/3/",
    missing_auth: TrackerConfigError::MissingJiraApiToken,
    messages: RestToolMessages {
        invalid_arguments: "jira_rest expects an object with method and path.",
        invalid_method: "jira_rest.method must be GET, POST, PUT, or DELETE.",
        invalid_path: "jira_rest.path must begin with /rest/api/3/.",
        invalid_query: "jira_rest.query must be a JSON object when provided.",
        missing_auth: "Symphony is missing Jira auth. Set tracker.provider.api_token or export JIRA_API_TOKEN.",
        request_failed: "Jira API request failed before receiving a successful response.",
        execution_failed: "Jira REST tool execution failed.",
    },
};

/// The `jira` adapter.
#[derive(Debug, Clone)]
pub struct JiraTracker {
    http: HttpClient,
    env: Arc<dyn EnvSource>,
}

impl JiraTracker {
    /// Adapter over `http`, resolving `$VAR`/default env vars through `env`.
    pub fn new(http: HttpClient, env: Arc<dyn EnvSource>) -> Self {
        Self { http, env }
    }

    fn settings(&self, settings: &TrackerSettings) -> Result<JiraSettings, TrackerError> {
        resolve_jira(settings, self.env.as_ref()).map_err(Into::into)
    }

    /// `Client.request/5`: resolves settings, then performs one REST call and returns its status and
    /// body without status mapping (used by `jira_rest`; the write path for agents).
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
        settings: &JiraSettings,
        method: Method,
        path: &str,
        query: &Map<String, Value>,
        body: Option<Value>,
    ) -> Result<RestResponse, TrackerError> {
        if matches!(method, Method::Head | Method::Patch) {
            return Err(TrackerError::InvalidMethod(Provider::Jira));
        }
        let query = query_pairs(query).ok_or(TrackerError::UnknownPayload(Provider::Jira))?;
        let credentials = STANDARD.encode(format!("{}:{}", settings.email, settings.api_token));
        let request = HttpRequest::new(method, format!("{}{}", settings.base_url, path))
            .header("Accept", "application/json")
            .credential_header(
                "Authorization",
                format!("Basic {credentials}"),
                settings.api_token.clone(),
            )
            .query(query)
            .json(body);
        let response = self
            .http
            .send(request)
            .await
            .map_err(|err| TrackerError::request(Provider::Jira, err))?;
        Ok(RestResponse {
            status: response.status,
            body: response.body,
        })
    }

    async fn post(
        &self,
        settings: &JiraSettings,
        path: &str,
        body: Value,
    ) -> Result<Value, TrackerError> {
        let response = self
            .perform(settings, Method::Post, path, &Map::new(), Some(body))
            .await?;
        map_status(Provider::Jira, Method::Post, path, response, false)?
            .ok_or(TrackerError::UnknownPayload(Provider::Jira))
    }

    async fn fetch_id_batch(
        &self,
        settings: &JiraSettings,
        batch: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let body = json!({"issueIdsOrKeys": batch, "fields": ISSUE_FIELDS});
        let payload = self.post(settings, BULKFETCH_PATH, body).await?;
        let raw_issues = payload
            .get("issues")
            .and_then(Value::as_array)
            .ok_or(TrackerError::UnknownPayload(Provider::Jira))?;
        let requested: HashSet<&str> = batch.iter().map(String::as_str).collect();
        let mut by_id: HashMap<String, Issue> = HashMap::new();
        for raw in raw_issues {
            let id = raw
                .get("id")
                .and_then(Value::as_str)
                .ok_or(TrackerError::UnknownPayload(Provider::Jira))?;
            if !requested.contains(id) {
                continue;
            }
            let project_key = raw
                .pointer("/fields/project/key")
                .filter(|v| !v.is_null())
                .ok_or(TrackerError::UnknownPayload(Provider::Jira))?;
            if !same_project_key(project_key.as_str(), &settings.project_key) {
                continue;
            }
            let issue = normalize_issue(raw, settings)
                .ok_or(TrackerError::UnknownPayload(Provider::Jira))?;
            by_id.insert(id.to_owned(), issue);
        }
        Ok(batch.iter().filter_map(|id| by_id.remove(id)).collect())
    }
}

/// `"` + value with `\` and `"` escaped + `"`.
fn jql_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `project = "KEY" AND status IN ("A", "B")` (states passed raw).
pub fn state_jql(project_key: &str, states: &[String]) -> String {
    let quoted: Vec<String> = states.iter().map(|s| jql_quote(s)).collect();
    format!(
        "project = {} AND status IN ({})",
        jql_quote(project_key),
        quoted.join(", ")
    )
}

fn same_project_key(left: Option<&str>, right: &str) -> bool {
    left.is_some_and(|l| l.trim().to_lowercase() == right.trim().to_lowercase())
}

/// Normalizes a Jira issue; `None` unless `id`, `key`, `fields.summary`, `fields.status.name` are
/// non-blank strings and `fields.project.key` matches the configured project (case-insensitive).
pub fn normalize_issue(raw: &Value, settings: &JiraSettings) -> Option<Issue> {
    let obj = raw.as_object()?;
    let id = obj.get("id")?.as_str()?;
    let key = obj.get("key")?.as_str()?;
    let fields = obj.get("fields")?.as_object()?;
    let field = |name: &str| fields.get(name);
    let state = present_str(raw.pointer("/fields/status/name"))?;
    let category = raw
        .pointer("/fields/status/statusCategory/key")
        .and_then(Value::as_str);
    if !same_project_key(
        raw.pointer("/fields/project/key").and_then(Value::as_str),
        &settings.project_key,
    ) || id.trim().is_empty()
        || key.trim().is_empty()
    {
        return None;
    }
    let title = present_str(field("summary"))?;
    let blocked_by = extract_blockers(field("issuelinks"));
    let dispatchable = dispatchable(state, category, &blocked_by, &settings.terminal_states);
    Some(Issue {
        id: Some(id.to_owned()),
        native_ref: None,
        identifier: Some(key.to_owned()),
        title: Some(title.to_owned()),
        description: description_text(field("description")),
        priority: None,
        state: Some(state.to_owned()),
        branch_name: None,
        url: Some(format!(
            "{}/browse/{}",
            settings.base_url,
            encode_unreserved(key)
        )),
        assignee_id: raw
            .pointer("/fields/assignee/accountId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        labels: field("labels")
            .and_then(Value::as_array)
            .map(|labels| normalize_labels(labels.iter().filter_map(Value::as_str)))
            .unwrap_or_default(),
        blocked_by,
        dispatchable,
        created_at: field("created")
            .and_then(Value::as_str)
            .and_then(parse_datetime_compact_offset),
        updated_at: field("updated")
            .and_then(Value::as_str)
            .and_then(parse_datetime_compact_offset),
    })
}

/// Plain-text description: strings are trimmed; ADF documents are flattened with [`adf_text`];
/// blank results become `None`.
pub fn description_text(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::String(s) => s.clone(),
        node @ Value::Object(_) => adf_text(node),
        _ => return None,
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Atlassian Document Format to text (ordered clauses; first match wins):
/// `hardBreak` -> newline; `text` -> the text; paragraph/heading/blockquote -> children + newline;
/// any other node with `content` -> children; `attrs.text`/`shortName`/`url` (strings only) -> that.
pub fn adf_text(node: &Value) -> String {
    let Some(obj) = node.as_object() else {
        return String::new();
    };
    let kind = obj.get("type").and_then(Value::as_str);
    if kind == Some("hardBreak") {
        return "\n".into();
    }
    if let Some(text) = obj.get("text").and_then(Value::as_str) {
        return text.to_owned();
    }
    if let Some(content) = obj.get("content").and_then(Value::as_array) {
        let children: String = content.iter().map(adf_text).collect();
        return if matches!(kind, Some("paragraph" | "heading" | "blockquote")) {
            children + "\n"
        } else {
            children
        };
    }
    if let Some(attrs) = obj.get("attrs").and_then(Value::as_object) {
        return ["text", "shortName", "url"]
            .iter()
            .find_map(|k| attrs.get(*k).and_then(Value::as_str))
            .unwrap_or_default()
            .to_owned();
    }
    String::new()
}

/// Inward `Blocks` links ("the inward issue blocks this one"), in order.
fn extract_blockers(links: Option<&Value>) -> Vec<BlockerRef> {
    let Some(links) = links.and_then(Value::as_array) else {
        return Vec::new();
    };
    links
        .iter()
        .filter_map(|link| {
            let type_name = link.pointer("/type/name")?.as_str()?;
            let inward = link.get("inwardIssue").filter(|i| i.is_object())?;
            (normalize_state(type_name) == "blocks").then(|| BlockerRef {
                id: present_str(inward.get("id")).map(str::to_owned),
                identifier: present_str(inward.get("key")).map(str::to_owned),
                state: present_str(inward.pointer("/fields/status/name")).map(str::to_owned),
            })
        })
        .collect()
}

/// Ready-state gate: with a status category, only `new` is gated; without one, `todo`/`to do` are.
/// A gated issue is dispatchable only when every blocker's state is terminal.
fn dispatchable(
    state: &str,
    category: Option<&str>,
    blockers: &[BlockerRef],
    terminal_states: &[String],
) -> bool {
    let gated = match category.filter(|c| !c.is_empty()) {
        Some(category) => normalize_state(category) == "new",
        None => matches!(normalize_state(state).as_str(), "todo" | "to do"),
    };
    !gated
        || blockers.iter().all(|blocker| {
            blocker
                .state
                .as_deref()
                .is_some_and(|s| terminal_states.contains(&normalize_state(s)))
        })
}

enum PageNext {
    Done,
    Token(String),
}

fn state_page(payload: &Value) -> Result<(&Vec<Value>, PageNext), TrackerError> {
    let issues = payload
        .get("issues")
        .and_then(Value::as_array)
        .ok_or(TrackerError::UnknownPayload(Provider::Jira))?;
    match payload.get("isLast") {
        Some(Value::Bool(true)) => Ok((issues, PageNext::Done)),
        Some(Value::Bool(false)) => match payload.get("nextPageToken").and_then(Value::as_str) {
            Some(token) if !token.is_empty() => Ok((issues, PageNext::Token(token.to_owned()))),
            _ => Err(TrackerError::MissingPageCursor(Provider::Jira)),
        },
        _ => Err(TrackerError::UnknownPayload(Provider::Jira)),
    }
}

#[async_trait]
impl Tracker for JiraTracker {
    fn kind(&self) -> &'static str {
        "jira"
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
        let jql = state_jql(&resolved.project_key, states);
        let mut issues = Vec::new();
        let mut token: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..MAX_PAGES {
            let mut body = json!({"jql": jql, "fields": ISSUE_FIELDS, "maxResults": PAGE_SIZE});
            if let (Some(t), Value::Object(map)) = (&token, &mut body) {
                map.insert("nextPageToken".into(), Value::String(t.clone()));
            }
            let payload = self.post(&resolved, SEARCH_PATH, body).await?;
            let (raw, next) = state_page(&payload)?;
            issues.extend(normalize_candidate_page(
                raw,
                "Jira issue",
                &requested,
                |item| normalize_issue(item, &resolved),
            ));
            match next {
                PageNext::Done => return Ok(issues),
                PageNext::Token(next_token) => {
                    if !seen.insert(next_token.clone()) {
                        return Err(TrackerError::PaginationRepeatedCursor(Provider::Jira));
                    }
                    token = Some(next_token);
                }
            }
        }
        Err(TrackerError::PaginationLimitExceeded(Provider::Jira))
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
        let ids = uniq(ids);
        let mut issues = Vec::new();
        for batch in ids.chunks(PAGE_SIZE) {
            issues.extend(self.fetch_id_batch(&resolved, batch).await?);
        }
        Ok(issues)
    }

    /// `JIRA_API_TOKEN` plus a `$VAR` used for `provider.api_token`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jql_quotes_and_escapes() {
        assert_eq!(
            state_jql("SYM", &["To Do".into()]),
            r#"project = "SYM" AND status IN ("To Do")"#
        );
        assert_eq!(
            state_jql("A\"B", &["x\\y".into(), "In Progress".into()]),
            r#"project = "A\"B" AND status IN ("x\\y", "In Progress")"#
        );
    }

    #[test]
    fn adf_flattening() {
        let doc = json!({"type": "doc", "content": [
            {"type": "paragraph", "content": [
                {"type": "text", "text": "First line"},
                {"type": "hardBreak"},
                {"type": "text", "text": "Second line"}
            ]}
        ]});
        assert_eq!(
            description_text(Some(&doc)).as_deref(),
            Some("First line\nSecond line")
        );
        let inline = json!({"type": "doc", "content": [{"type": "paragraph", "content": [
            {"type": "mention", "attrs": {"text": "@Alex"}},
            {"type": "emoji", "attrs": {"shortName": ":wave:"}},
            {"type": "status", "attrs": {"text": "Ready"}},
            {"type": "inlineCard", "attrs": {"url": "https://example.test/card"}}
        ]}]});
        assert_eq!(
            description_text(Some(&inline)).as_deref(),
            Some("@Alex:wave:Readyhttps://example.test/card")
        );
        let panel = json!({"type": "doc", "content": [{"type": "panel", "attrs": {"panelType": "info"},
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "Panel text"}]}]}]});
        assert_eq!(
            description_text(Some(&panel)).as_deref(),
            Some("Panel text")
        );
        // Non-string attrs fall through to the next attribute.
        assert_eq!(
            adf_text(&json!({"attrs": {"text": 5, "shortName": ":x:"}})),
            ":x:"
        );
        assert_eq!(
            description_text(Some(&json!("  plain  "))).as_deref(),
            Some("plain")
        );
        assert_eq!(description_text(Some(&json!("   "))), None);
        assert_eq!(description_text(Some(&json!(42))), None);
        assert_eq!(description_text(None), None);
    }
}
