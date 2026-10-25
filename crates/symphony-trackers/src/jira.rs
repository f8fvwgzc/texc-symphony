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
//! - One agent tool, `jira_rest`: a REST v3 passthrough limited to the configured project — issue
//!   paths for `{KEY}-n` keys (numeric ids after one `GET` confirms the project), `POST
//!   /rest/api/3/issue` creating in the project, searches whose JQL names the project (and which are
//!   then wrapped as `project = "KEY" AND (...)`), and `GET` of the project resource — plus the
//!   `tracker.agent_tools` methods (see [`crate::scope`]).

// Modified 2026-10-25: scoped `jira_rest` (audit A2).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use symphony_core::config::schema::AgentToolSettings;
use symphony_core::config::tracker::validate_tracker;
use symphony_core::config::{
    JiraSettings, TrackerSettings, resolve_jira, secret_environment_names,
};
use symphony_core::issue::{normalize_labels, normalize_state, parse_datetime_compact_offset};
use symphony_core::{BlockerRef, EnvSource, Issue, TrackerConfigError};

use crate::error::{Provider, TrackerError};
use crate::rest::{
    RestCall, RestResponse, RestToolMessages, RestToolSpec, encode_unreserved, map_status,
    normalize_candidate_page, present_str, query_pairs, state_set, uniq,
};
use crate::scope::{self, Denied};
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
    description: "Execute a Jira Cloud REST v3 request for the configured project (its issues, searches scoped with `project = KEY`) using Symphony's configured auth.\n",
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

    /// `true` when issue `id` exists and belongs to the configured project (one `GET`).
    async fn issue_in_project(
        &self,
        settings: &JiraSettings,
        id: &str,
    ) -> Result<bool, TrackerError> {
        let path = format!("/rest/api/3/issue/{}", encode_unreserved(id));
        let query = Map::from_iter([("fields".to_owned(), json!("project"))]);
        let response = self
            .perform(settings, Method::Get, &path, &query, None)
            .await?;
        let issue = map_status(Provider::Jira, Method::Get, &path, response, true)?;
        let key = issue
            .as_ref()
            .and_then(|i| i.pointer("/fields/project/key"))
            .and_then(Value::as_str);
        Ok(same_project_key(key, &settings.project_key))
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

/// What a `jira_rest` call targets once its path passed the static checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeTarget {
    /// Inside the project (a `{KEY}-n` issue, a scoped search, the project resource).
    Project,
    /// `/rest/api/3/issue/{numeric id}...`: allowed when the issue's project is the configured one.
    IssueId(String),
}

/// `{KEY}-{digits}` for the configured project key (case-insensitive).
fn is_project_issue_key(value: &str, project_key: &str) -> bool {
    value.rsplit_once('-').is_some_and(|(key, number)| {
        key.eq_ignore_ascii_case(project_key)
            && !number.is_empty()
            && number.bytes().all(|b| b.is_ascii_digit())
    })
}

/// The static part of the `jira_rest` scope (see the module docs). A search's JQL is rewritten in
/// `call` to `project = "KEY" AND (<jql>) [ORDER BY ...]`, so an `OR` cannot widen it.
pub fn check_scope(
    settings: &JiraSettings,
    call: &mut RestCall,
    _policy: &AgentToolSettings,
) -> Result<ScopeTarget, Denied> {
    let segments = scope::split_path(TOOL.name, &call.path)?;
    scope::refuse_encoded_slashes(TOOL.name, &segments)?;
    let key = settings.project_key.as_str();
    let out_of_scope = || {
        scope::out_of_scope(
            TOOL.name,
            &format!(
                "project {key}: /rest/api/3/issue/{key}-<n>..., POST /rest/api/3/issue in {key}, /rest/api/3/search[/jql] with `project = {key}` and GET /rest/api/3/project/{key}..."
            ),
        )
    };
    let tail = match segments.as_slice() {
        [rest, api, version, tail @ ..] if rest.is("rest") && api.is("api") && version.is("3") => {
            tail
        }
        _ => return Err(out_of_scope()),
    };
    match tail {
        [issue] if issue.is("issue") => {
            let project = call
                .body
                .as_ref()
                .and_then(|b| b.pointer("/fields/project/key"))
                .and_then(Value::as_str);
            if call.method == Method::Post && same_project_key(project, key) {
                Ok(ScopeTarget::Project)
            } else {
                Err(Denied::new(
                    "body_out_of_scope",
                    format!(
                        "`{}` may only create issues with POST /rest/api/3/issue whose fields.project.key is {key}.",
                        TOOL.name
                    ),
                ))
            }
        }
        [issue, id, ..] if issue.is("issue") => {
            if is_project_issue_key(&id.decoded, key) {
                Ok(ScopeTarget::Project)
            } else if !id.decoded.is_empty() && id.decoded.bytes().all(|b| b.is_ascii_digit()) {
                Ok(ScopeTarget::IssueId(id.decoded.clone()))
            } else {
                Err(out_of_scope())
            }
        }
        [search] | [search, _]
            if search.is("search") && tail.get(1).is_none_or(|s| s.is("jql")) =>
        {
            scope_search(call, key).map(|()| ScopeTarget::Project)
        }
        [project, project_key, ..] if project.is("project") && project_key.is(key) => {
            if call.method == Method::Get {
                Ok(ScopeTarget::Project)
            } else {
                Err(scope::admin_path(
                    TOOL.name,
                    &format!("{} {}", call.method, call.path),
                ))
            }
        }
        _ => Err(out_of_scope()),
    }
}

/// Requires the search's JQL (`query.jql` for `GET`, `body.jql` for `POST`) to name the project,
/// then wraps it so it cannot reach other projects.
fn scope_search(call: &mut RestCall, key: &str) -> Result<(), Denied> {
    let missing = || {
        Denied::new(
            "jql_out_of_scope",
            format!(
                "`{}` searches must pass JQL (query.jql for GET, body.jql for POST) that includes `project = {key}`.",
                TOOL.name
            ),
        )
    };
    if call.path.contains('?') {
        return Err(missing());
    }
    let slot = match call.method {
        Method::Get => call.query.get_mut("jql"),
        _ => call
            .body
            .as_mut()
            .and_then(Value::as_object_mut)
            .and_then(|body| body.get_mut("jql")),
    };
    let Some(slot) = slot else {
        return Err(missing());
    };
    let Some(jql) = slot
        .as_str()
        .filter(|jql| jql_balanced(jql) && jql_names_project(jql, key))
    else {
        return Err(missing());
    };
    *slot = Value::String(scoped_jql(jql, key));
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JqlToken {
    Word(String),
    Text(String),
    Punct(char),
}

/// Splits JQL into words, quoted strings and operator characters (with byte offsets).
fn jql_tokens(jql: &str) -> Vec<(usize, JqlToken)> {
    let mut tokens = Vec::new();
    let mut chars = jql.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '"' | '\'' => {
                let mut text = String::new();
                while let Some((_, next)) = chars.next() {
                    match next {
                        '\\' => {
                            if let Some((_, escaped)) = chars.next() {
                                text.push(escaped);
                            }
                        }
                        q if q == c => break,
                        other => text.push(other),
                    }
                }
                tokens.push((start, JqlToken::Text(text)));
            }
            '=' | '!' | '<' | '>' | '~' | '(' | ')' | ',' => {
                tokens.push((start, JqlToken::Punct(c)));
            }
            c => {
                let mut word = String::from(c);
                while let Some((_, next)) = chars.peek() {
                    if next.is_whitespace() || "\"'=!<>~(),".contains(*next) {
                        break;
                    }
                    word.push(*next);
                    chars.next();
                }
                tokens.push((start, JqlToken::Word(word)));
            }
        }
    }
    tokens
}

fn jql_value_is(token: Option<&(usize, JqlToken)>, key: &str) -> bool {
    matches!(token, Some((_, JqlToken::Word(v) | JqlToken::Text(v))) if v.eq_ignore_ascii_case(key))
}

fn jql_word_is(token: Option<&(usize, JqlToken)>, word: &str) -> bool {
    matches!(token, Some((_, JqlToken::Word(w))) if w.eq_ignore_ascii_case(word))
}

/// `true` when the JQL contains `project = KEY` or `project in (KEY, ...)` (any case, quoted or
/// not) outside string literals.
pub fn jql_names_project(jql: &str, key: &str) -> bool {
    let tokens = jql_tokens(jql);
    (0..tokens.len()).any(|i| {
        jql_word_is(tokens.get(i), "project")
            && match tokens.get(i + 1) {
                Some((_, JqlToken::Punct('='))) => jql_value_is(tokens.get(i + 2), key),
                Some((_, JqlToken::Word(w))) if w.eq_ignore_ascii_case("in") => {
                    matches!(tokens.get(i + 2), Some((_, JqlToken::Punct('('))))
                        && jql_value_is(tokens.get(i + 3), key)
                }
                _ => false,
            }
    })
}

/// `true` when the JQL's parentheses (outside string literals) balance and never close more than
/// they opened, so [`scoped_jql`]'s wrapper cannot be escaped with a stray `)`.
pub fn jql_balanced(jql: &str) -> bool {
    let mut depth = 0i64;
    for (_, token) in jql_tokens(jql) {
        match token {
            JqlToken::Punct('(') => depth += 1,
            JqlToken::Punct(')') => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// `project = "KEY" AND (<jql>)`, keeping a top-level `ORDER BY` clause at the end.
pub fn scoped_jql(jql: &str, key: &str) -> String {
    let tokens = jql_tokens(jql);
    let mut depth = 0i32;
    let mut order_at = None;
    for (i, (offset, token)) in tokens.iter().enumerate() {
        match token {
            JqlToken::Punct('(') => depth += 1,
            JqlToken::Punct(')') => depth -= 1,
            JqlToken::Word(w)
                if depth == 0
                    && w.eq_ignore_ascii_case("order")
                    && jql_word_is(tokens.get(i + 1), "by") =>
            {
                order_at = Some(*offset);
            }
            _ => {}
        }
    }
    let (filter, order) = match order_at {
        Some(at) => (&jql[..at], Some(jql[at..].trim())),
        None => (jql, None),
    };
    let mut scoped = format!("project = {} AND ({})", jql_quote(key), filter.trim());
    if let Some(order) = order {
        scoped.push(' ');
        scoped.push_str(order);
    }
    scoped
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

    fn agent_tool_specs(&self, settings: &TrackerSettings) -> Vec<Value> {
        vec![TOOL.spec(&settings.agent_tools)]
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
        let mut call = match TOOL.normalize(arguments) {
            Ok(call) => call,
            Err(err) => return TOOL.argument_error(err),
        };
        let resolved = match self.settings(&ctx.settings) {
            Ok(resolved) => resolved,
            Err(err) => return TOOL.client_error(&err),
        };
        let policy = &ctx.settings.agent_tools;
        let target = match scope::check_method(&TOOL, call.method, policy)
            .and_then(|()| check_scope(&resolved, &mut call, policy))
        {
            Ok(target) => target,
            Err(denied) => return denied.tool_result(),
        };
        if let ScopeTarget::IssueId(id) = &target {
            match self.issue_in_project(&resolved, id).await {
                Ok(true) => {}
                Ok(false) => {
                    return Denied::new(
                        "path_out_of_scope",
                        format!(
                            "`{}` is limited to project {}: issue {id} is not part of it.",
                            TOOL.name, resolved.project_key
                        ),
                    )
                    .tool_result();
                }
                Err(err) => return TOOL.client_error(&err),
            }
        }
        match self
            .perform(&resolved, call.method, &call.path, &call.query, call.body)
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
    fn unbalanced_jql_cannot_escape_the_project_wrapper() {
        assert!(jql_balanced("project = SYM AND (status = Open)"));
        assert!(jql_balanced("summary ~ \"a ) b\" AND project = SYM"));
        assert!(!jql_balanced("project = SYM) OR (project = OTHER"));
        assert!(!jql_balanced("project = SYM AND (status = Open"));
        assert!(!jql_balanced(")("));
    }

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
