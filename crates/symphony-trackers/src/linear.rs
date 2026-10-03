//! Linear adapter (`SymphonyElixir.Linear.{Adapter, Client, AgentTool}`).
//!
//! - Endpoint `tracker.endpoint` (default `https://api.linear.app/graphql`), header
//!   `Authorization: <api_key>` (raw key, no `Bearer`), POST only, never retried.
//! - Candidate reads page 50 issues at a time (`SymphonyLinearPoll`); id refreshes batch 50 ids
//!   (`SymphonyLinearIssuesById`) and fail on any malformed node.
//! - `assignee` routing: `me` resolves through `SymphonyLinearViewer` on every read; any other value
//!   is compared to the assignee id.
//! - One agent tool, `linear_graphql` (raw GraphQL passthrough with the bound credential).

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde_json::{Value, json};
use symphony_core::config::{TrackerSettings, resolve_linear, secret_environment_names};
use symphony_core::issue::{normalize_labels, normalize_state, parse_datetime};
use symphony_core::{BlockerRef, Issue, TrackerConfigError};

use crate::error::{Provider, TrackerError, inspect_string};
use crate::rest::{opt_string, present_str, uniq};
use crate::tool::{ToolContext, ToolResult, pretty_json};
use crate::transport::{HttpClient, HttpRequest, Method, scrub_secrets};
use crate::{MAX_PAGES, Tracker};

/// Page size for state reads and id batches.
pub const ISSUE_PAGE_SIZE: usize = 50;
const MAX_ERROR_BODY_LOG_BYTES: usize = 1_000;

/// `SymphonyLinearPoll`: candidate issues by state (verbatim from the Elixir client).
pub const POLL_QUERY: &str = r#"query SymphonyLinearPoll($projectSlug: String!, $stateNames: [String!]!, $first: Int!, $relationFirst: Int!, $after: String) {
  issues(filter: {project: {slugId: {eq: $projectSlug}}, state: {name: {in: $stateNames}}}, first: $first, after: $after) {
    nodes {
      id
      identifier
      title
      description
      priority
      state {
        name
      }
      branchName
      url
      assignee {
        id
      }
      labels {
        nodes {
          name
        }
      }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue {
            id
            identifier
            state {
              name
            }
          }
        }
      }
      createdAt
      updatedAt
    }
    pageInfo {
      hasNextPage
      endCursor
    }
  }
}
"#;

/// `SymphonyLinearIssuesById`: id refresh (verbatim).
pub const ISSUES_BY_ID_QUERY: &str = r#"query SymphonyLinearIssuesById($ids: [ID!]!, $projectSlug: String!, $first: Int!, $relationFirst: Int!) {
  issues(filter: {id: {in: $ids}, project: {slugId: {eq: $projectSlug}}}, first: $first) {
    nodes {
      id
      identifier
      title
      description
      priority
      state {
        name
      }
      branchName
      url
      assignee {
        id
      }
      labels {
        nodes {
          name
        }
      }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue {
            id
            identifier
            state {
              name
            }
          }
        }
      }
      createdAt
      updatedAt
    }
  }
}
"#;

/// `SymphonyLinearViewer`: resolves `assignee: me` (verbatim).
pub const VIEWER_QUERY: &str = r#"query SymphonyLinearViewer {
  viewer {
    id
  }
}
"#;

/// Name of the Linear agent tool.
pub const LINEAR_GRAPHQL_TOOL: &str = "linear_graphql";
const LINEAR_GRAPHQL_DESCRIPTION: &str =
    "Execute a raw GraphQL query or mutation against Linear using Symphony's configured auth.\n";

/// The `linear` adapter.
#[derive(Debug, Clone)]
pub struct LinearTracker {
    http: HttpClient,
}

/// Assignee routing filter: the set of assignee ids that count as "this worker".
pub type AssigneeFilter = HashSet<String>;

impl LinearTracker {
    /// Adapter over `http`.
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    /// `Client.graphql/3`: POSTs `{"query", "variables"}` with the settings' endpoint and key.
    ///
    /// HTTP 200 returns the decoded body (whatever its shape); any other status is logged with a
    /// summarized body and becomes `linear_api_status`; transport failures become
    /// `linear_api_request`. A missing key returns `missing_linear_api_token` (Elixir wrapped it as a
    /// transport error; see the migration notes).
    pub async fn graphql(
        &self,
        settings: &TrackerSettings,
        query: &str,
        variables: Value,
    ) -> Result<Value, TrackerError> {
        let Some(api_key) = settings.api_key.as_deref() else {
            tracing::error!("Linear GraphQL request failed: :missing_linear_api_token");
            return Err(TrackerConfigError::MissingLinearApiToken.into());
        };
        let endpoint = settings.endpoint.as_deref().unwrap_or_default();
        let request = HttpRequest::new(Method::Post, endpoint)
            .credential_header("Authorization", api_key, api_key)
            .header("Content-Type", "application/json")
            .json(Some(json!({"query": query, "variables": variables})));
        match self.http.send(request).await {
            Ok(response) if response.status == 200 => Ok(response.body),
            Ok(response) => {
                let secrets = [api_key.to_owned()];
                tracing::error!(
                    "Linear GraphQL request failed status={} body={}",
                    response.status,
                    scrub_secrets(&summarize_error_body(&response.body), &secrets)
                );
                Err(TrackerError::ApiStatus {
                    provider: Provider::Linear,
                    status: response.status,
                })
            }
            Err(err) => {
                tracing::error!("Linear GraphQL request failed: {}", err.inspect());
                Err(TrackerError::request(Provider::Linear, err))
            }
        }
    }

    async fn assignee_filter(
        &self,
        settings: &TrackerSettings,
    ) -> Result<Option<AssigneeFilter>, TrackerError> {
        let Some(assignee) = settings.assignee.as_deref().map(str::trim) else {
            return Ok(None);
        };
        match assignee {
            "" => Ok(None),
            "me" => {
                let body = self.graphql(settings, VIEWER_QUERY, json!({})).await?;
                let viewer_id = body
                    .pointer("/data/viewer")
                    .filter(|v| v.is_object())
                    .and_then(|viewer| viewer.get("id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .ok_or(TrackerError::MissingLinearViewerIdentity)?;
                Ok(Some(HashSet::from([viewer_id.to_owned()])))
            }
            other => Ok(Some(HashSet::from([other.to_owned()]))),
        }
    }

    async fn fetch_state_pages(
        &self,
        settings: &TrackerSettings,
        project_slug: &str,
        states: &[String],
        filter: Option<&AssigneeFilter>,
    ) -> Result<Vec<Issue>, TrackerError> {
        let terminal = terminal_set(settings);
        let mut issues = Vec::new();
        let mut after: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..MAX_PAGES {
            let variables = json!({
                "projectSlug": project_slug,
                "stateNames": states,
                "first": ISSUE_PAGE_SIZE,
                "relationFirst": ISSUE_PAGE_SIZE,
                "after": after,
            });
            let body = self.graphql(settings, POLL_QUERY, variables).await?;
            let (page, page_info) = decode_page(&body, filter, &terminal)?;
            issues.extend(page);
            match next_page_cursor(page_info)? {
                None => return Ok(issues),
                Some(cursor) => {
                    if !seen.insert(cursor.clone()) {
                        return Err(TrackerError::PaginationRepeatedCursor(Provider::Linear));
                    }
                    after = Some(cursor);
                }
            }
        }
        Err(TrackerError::PaginationLimitExceeded(Provider::Linear))
    }

    async fn fetch_id_batches(
        &self,
        settings: &TrackerSettings,
        project_slug: &str,
        ids: &[String],
        filter: Option<&AssigneeFilter>,
    ) -> Result<Vec<Issue>, TrackerError> {
        let terminal = terminal_set(settings);
        let mut issues = Vec::new();
        for batch in ids.chunks(ISSUE_PAGE_SIZE) {
            let variables = json!({
                "ids": batch,
                "projectSlug": project_slug,
                "first": batch.len(),
                "relationFirst": ISSUE_PAGE_SIZE,
            });
            let body = self
                .graphql(settings, ISSUES_BY_ID_QUERY, variables)
                .await?;
            issues.extend(decode_strict(&body, filter, &terminal)?);
        }
        Ok(sort_by_requested(issues, ids))
    }
}

fn required_read_config(settings: &TrackerSettings) -> Result<&str, TrackerError> {
    if settings.api_key.is_none() {
        return Err(TrackerConfigError::MissingLinearApiToken.into());
    }
    settings
        .project_slug
        .as_deref()
        .ok_or_else(|| TrackerConfigError::MissingLinearProjectSlug.into())
}

fn terminal_set(settings: &TrackerSettings) -> HashSet<String> {
    settings
        .terminal_states
        .iter()
        .flatten()
        .map(|s| normalize_state(s))
        .collect()
}

/// Pagination state of one page (`hasNextPage`, `endCursor`); `None` when `pageInfo` is incomplete.
type PageInfo = Option<(bool, Option<String>)>;

fn decode_nodes(body: &Value) -> Result<&Vec<Value>, TrackerError> {
    if let Some(nodes) = body.pointer("/data/issues/nodes").and_then(Value::as_array) {
        return Ok(nodes);
    }
    match body.as_object().and_then(|o| o.get("errors")) {
        Some(errors) => Err(TrackerError::LinearGraphqlErrors(errors.clone())),
        None => Err(TrackerError::UnknownPayload(Provider::Linear)),
    }
}

fn decode_page(
    body: &Value,
    filter: Option<&AssigneeFilter>,
    terminal: &HashSet<String>,
) -> Result<(Vec<Issue>, PageInfo), TrackerError> {
    let nodes = decode_nodes(body)?;
    let normalized: Vec<Option<Issue>> = nodes
        .iter()
        .map(|node| normalize_issue(node, filter, terminal))
        .collect();
    let malformed = normalized.iter().filter(|i| i.is_none()).count();
    if malformed > 0 {
        tracing::warn!("Dropping malformed Linear issue records count={malformed}");
    }
    let page_info = body
        .pointer("/data/issues/pageInfo")
        .and_then(Value::as_object)
        .filter(|pi| pi.contains_key("hasNextPage") && pi.contains_key("endCursor"))
        .map(|pi| {
            (
                pi.get("hasNextPage") == Some(&Value::Bool(true)),
                pi.get("endCursor")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            )
        });
    Ok((normalized.into_iter().flatten().collect(), page_info))
}

fn next_page_cursor(page_info: PageInfo) -> Result<Option<String>, TrackerError> {
    match page_info {
        Some((true, Some(cursor))) if !cursor.is_empty() => Ok(Some(cursor)),
        Some((true, _)) => Err(TrackerError::MissingPageCursor(Provider::Linear)),
        _ => Ok(None),
    }
}

fn decode_strict(
    body: &Value,
    filter: Option<&AssigneeFilter>,
    terminal: &HashSet<String>,
) -> Result<Vec<Issue>, TrackerError> {
    decode_nodes(body)?
        .iter()
        .map(|node| {
            normalize_issue(node, filter, terminal)
                .ok_or(TrackerError::UnknownPayload(Provider::Linear))
        })
        .collect()
}

fn sort_by_requested(mut issues: Vec<Issue>, ids: &[String]) -> Vec<Issue> {
    let index: HashMap<&str, usize> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let fallback = index.len();
    issues.sort_by_key(|issue| {
        issue
            .id
            .as_deref()
            .and_then(|id| index.get(id).copied())
            .unwrap_or(fallback)
    });
    issues
}

/// Normalizes one Linear issue node; `None` when `id`, `identifier`, `title` or `state.name` is not
/// a non-blank string.
///
/// `dispatchable` = assigned to this worker (no filter: always) and not (state `todo` with a blocker
/// whose state is missing or not in `terminal_states`).
pub fn normalize_issue(
    node: &Value,
    filter: Option<&AssigneeFilter>,
    terminal_states: &HashSet<String>,
) -> Option<Issue> {
    let obj = node.as_object()?;
    let id = present_str(obj.get("id"))?;
    let identifier = present_str(obj.get("identifier"))?;
    let title = present_str(obj.get("title"))?;
    let state = present_str(node.pointer("/state/name"))?;
    let assignee = obj.get("assignee").filter(|a| a.is_object());
    let blocked_by = extract_blockers(node);
    let assigned = match filter {
        None => true,
        Some(match_values) => assignee
            .and_then(|a| a.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .is_some_and(|id| !id.is_empty() && match_values.contains(id)),
    };
    let blocked = normalize_state(state) == "todo"
        && blocked_by.iter().any(|blocker| match &blocker.state {
            Some(s) => !terminal_states.contains(&normalize_state(s)),
            None => true,
        });
    Some(Issue {
        id: Some(id.to_owned()),
        native_ref: None,
        identifier: Some(identifier.to_owned()),
        title: Some(title.to_owned()),
        description: opt_string(obj.get("description")),
        priority: obj.get("priority").and_then(Value::as_i64),
        state: Some(state.to_owned()),
        branch_name: opt_string(obj.get("branchName")),
        url: opt_string(obj.get("url")),
        assignee_id: assignee.and_then(|a| opt_string(a.get("id"))),
        labels: extract_labels(node),
        blocked_by,
        dispatchable: assigned && !blocked,
        created_at: obj
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(parse_datetime),
        updated_at: obj
            .get("updatedAt")
            .and_then(Value::as_str)
            .and_then(parse_datetime),
    })
}

fn extract_labels(node: &Value) -> Vec<String> {
    let Some(labels) = node.pointer("/labels/nodes").and_then(Value::as_array) else {
        return Vec::new();
    };
    normalize_labels(
        labels
            .iter()
            .filter_map(|label| label.get("name").and_then(Value::as_str)),
    )
}

fn extract_blockers(node: &Value) -> Vec<BlockerRef> {
    let Some(relations) = node
        .pointer("/inverseRelations/nodes")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    relations
        .iter()
        .filter_map(|relation| {
            let kind = relation.get("type")?.as_str()?;
            let issue = relation.get("issue").filter(|i| i.is_object())?;
            (kind.trim().to_lowercase() == "blocks").then(|| BlockerRef {
                id: opt_string(issue.get("id")),
                identifier: opt_string(issue.get("identifier")),
                state: opt_string(issue.pointer("/state/name")),
            })
        })
        .collect()
}

/// Non-200 body summary for logs: strings collapse whitespace, trim, cap at 1000 bytes (on a char
/// boundary, `...<truncated>` appended) and are quoted; other bodies render as compact JSON.
pub fn summarize_error_body(body: &Value) -> String {
    match body {
        Value::String(text) => {
            let collapsed = text.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
            inspect_string(&truncate(&collapsed))
        }
        other => truncate(&other.to_string()),
    }
}

fn truncate(text: &str) -> String {
    if text.len() <= MAX_ERROR_BODY_LOG_BYTES {
        return text.to_owned();
    }
    let mut cut = MAX_ERROR_BODY_LOG_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...<truncated>", &text[..cut])
}

/// The `linear_graphql` tool spec.
pub fn tool_spec() -> Value {
    json!({
        "name": LINEAR_GRAPHQL_TOOL,
        "description": LINEAR_GRAPHQL_DESCRIPTION,
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["query"],
            "properties": {
                "query": {
                    "type": "string",
                    "description": "GraphQL query or mutation document to execute against Linear."
                },
                "variables": {
                    "type": ["object", "null"],
                    "description": "Optional GraphQL variables object.",
                    "additionalProperties": true
                }
            }
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArgError {
    MissingQuery,
    InvalidArguments,
    InvalidVariables,
}

fn normalize_arguments(arguments: &Value) -> Result<(String, Value), ArgError> {
    match arguments {
        Value::String(raw) => match raw.trim() {
            "" => Err(ArgError::MissingQuery),
            query => Ok((query.to_owned(), json!({}))),
        },
        Value::Object(args) => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .ok_or(ArgError::MissingQuery)?;
            let variables = match args.get("variables") {
                None | Some(Value::Null) | Some(Value::Bool(false)) => json!({}),
                Some(v @ Value::Object(_)) => v.clone(),
                Some(_) => return Err(ArgError::InvalidVariables),
            };
            Ok((query.to_owned(), variables))
        }
        _ => Err(ArgError::InvalidArguments),
    }
}

fn argument_error(err: ArgError) -> ToolResult {
    ToolResult::error_message(match err {
        ArgError::MissingQuery => "`linear_graphql` requires a non-empty `query` string.",
        ArgError::InvalidArguments => {
            "`linear_graphql` expects either a GraphQL query string or an object with `query` and optional `variables`."
        }
        ArgError::InvalidVariables => {
            "`linear_graphql.variables` must be a JSON object when provided."
        }
    })
}

/// Tool error payload for a client failure.
fn client_error(err: &TrackerError) -> ToolResult {
    match err {
        TrackerError::Config(TrackerConfigError::MissingLinearApiToken) => {
            ToolResult::error_message(
                "Symphony is missing Linear auth. Set `tracker.provider.api_key` in `WORKFLOW.md` or export `LINEAR_API_KEY`.",
            )
        }
        TrackerError::ApiStatus {
            provider: Provider::Linear,
            status,
        } => ToolResult::failure(&json!({
            "error": {
                "message": format!("Linear GraphQL request failed with HTTP {status}."),
                "status": status,
            }
        })),
        TrackerError::ApiRequest {
            provider: Provider::Linear,
            source,
        } => ToolResult::failure(&json!({
            "error": {
                "message": "Linear GraphQL request failed before receiving a successful response.",
                "reason": source.inspect(),
            }
        })),
        other => ToolResult::failure(&json!({
            "error": {
                "message": "Linear GraphQL tool execution failed.",
                "reason": other.inspect(),
            }
        })),
    }
}

/// Success unless the body carries a non-empty top-level `errors` list; the whole body is returned.
fn graphql_response(body: &Value) -> ToolResult {
    let success = !body
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty());
    let output = match body {
        Value::Object(_) | Value::Array(_) => pretty_json(body),
        Value::String(text) => inspect_string(text),
        Value::Null => "nil".into(),
        other => other.to_string(),
    };
    ToolResult::new(success, output)
}

#[async_trait]
impl Tracker for LinearTracker {
    fn kind(&self) -> &'static str {
        "linear"
    }

    fn validate_config(&self, settings: &TrackerSettings) -> Result<(), TrackerError> {
        resolve_linear(settings).map(drop).map_err(Into::into)
    }

    /// States are deduplicated and sent verbatim (not trimmed or lowercased).
    async fn fetch_issues_by_states(
        &self,
        settings: &TrackerSettings,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let states = uniq(states);
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let project_slug = required_read_config(settings)?;
        let filter = self.assignee_filter(settings).await?;
        self.fetch_state_pages(settings, project_slug, &states, filter.as_ref())
            .await
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
        let project_slug = required_read_config(settings)?;
        let filter = self.assignee_filter(settings).await?;
        self.fetch_id_batches(settings, project_slug, &ids, filter.as_ref())
            .await
    }

    /// `["LINEAR_API_KEY"]` plus any `$VAR` used for `api_key` (computed at parse time).
    fn secret_environment_names(&self, settings: &TrackerSettings) -> Vec<String> {
        secret_environment_names(settings)
    }

    fn agent_tool_specs(&self) -> Vec<Value> {
        vec![tool_spec()]
    }

    async fn execute_agent_tool(
        &self,
        tool: Option<&str>,
        arguments: &Value,
        ctx: &ToolContext,
    ) -> ToolResult {
        if tool != Some(LINEAR_GRAPHQL_TOOL) {
            return ToolResult::unsupported(tool, &[LINEAR_GRAPHQL_TOOL]);
        }
        let (query, variables) = match normalize_arguments(arguments) {
            Ok(parsed) => parsed,
            Err(err) => return argument_error(err),
        };
        match self.graphql(&ctx.settings, &query, variables).await {
            Ok(body) => graphql_response(&body),
            Err(err) => client_error(&err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_are_verbatim() {
        assert!(POLL_QUERY.starts_with("query SymphonyLinearPoll("));
        assert!(POLL_QUERY.ends_with("}\n"));
        assert!(ISSUES_BY_ID_QUERY.contains("SymphonyLinearIssuesById"));
        assert!(ISSUES_BY_ID_QUERY.contains("projectSlug"));
        assert!(ISSUES_BY_ID_QUERY.contains("slugId"));
        assert_eq!(
            VIEWER_QUERY,
            "query SymphonyLinearViewer {\n  viewer {\n    id\n  }\n}\n"
        );
    }

    #[test]
    fn page_cursor_rules() {
        assert_eq!(
            next_page_cursor(Some((true, Some("c".into())))),
            Ok(Some("c".into()))
        );
        assert_eq!(
            next_page_cursor(Some((true, Some(String::new())))),
            Err(TrackerError::MissingPageCursor(Provider::Linear))
        );
        assert_eq!(
            next_page_cursor(Some((true, None))),
            Err(TrackerError::MissingPageCursor(Provider::Linear))
        );
        assert_eq!(next_page_cursor(Some((false, Some("c".into())))), Ok(None));
        assert_eq!(next_page_cursor(None), Ok(None));
    }

    #[test]
    fn page_merge_preserves_order() {
        let issue = |n: u32| Issue {
            id: Some(format!("{n}")),
            identifier: Some(format!("MT-{n}")),
            ..Issue::default()
        };
        let mut merged = Vec::new();
        for page in [vec![issue(1), issue(2)], vec![issue(3)]] {
            merged.extend(page);
        }
        let identifiers: Vec<_> = merged.iter().filter_map(|i| i.identifier.clone()).collect();
        assert_eq!(identifiers, ["MT-1", "MT-2", "MT-3"]);
    }

    #[test]
    fn summarizes_error_bodies() {
        assert_eq!(
            summarize_error_body(&Value::String("  bad \n\t request ".into())),
            "\"bad request\""
        );
        let long = "é".repeat(600);
        let summary = summarize_error_body(&Value::String(long));
        assert!(summary.ends_with("...<truncated>\""));
        assert_eq!(
            summarize_error_body(&json!({"errors": [{"message": "x"}]})),
            r#"{"errors":[{"message":"x"}]}"#
        );
    }

    #[test]
    fn client_failures_format_like_elixir() {
        let decode = |result: ToolResult| -> Value {
            assert!(!result.success);
            serde_json::from_str(&result.output).expect("json output")
        };
        assert_eq!(
            decode(client_error(&TrackerError::ApiStatus {
                provider: Provider::Linear,
                status: 503
            })),
            json!({"error": {"message": "Linear GraphQL request failed with HTTP 503.", "status": 503}})
        );
        assert_eq!(
            decode(client_error(&TrackerError::request(
                Provider::Linear,
                crate::transport::TransportError::Timeout
            ))),
            json!({"error": {
                "message": "Linear GraphQL request failed before receiving a successful response.",
                "reason": ":timeout"
            }})
        );
        assert_eq!(
            decode(client_error(&TrackerError::MissingLinearViewerIdentity)),
            json!({"error": {
                "message": "Linear GraphQL tool execution failed.",
                "reason": ":missing_linear_viewer_identity"
            }})
        );
    }

    #[test]
    fn graphql_responses_keep_the_whole_body() {
        let ok = graphql_response(&json!({"data": {"x": 1}, "errors": []}));
        assert!(ok.success, "an empty errors list is not a failure");
        let failed = graphql_response(&json!({"errors": [{"message": "nope"}]}));
        assert!(!failed.success);
        assert_eq!(graphql_response(&json!(true)).output, "true");
        assert_eq!(graphql_response(&Value::Null).output, "nil");
    }
}
