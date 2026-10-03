//! Shared pieces of the four REST adapters (GitHub, GitLab, Jira, Asana): percent-encoding, query
//! stringification, status mapping, page normalization and the raw-passthrough agent tool.

use std::collections::HashSet;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Map, Value, json};
use symphony_core::Issue;
use symphony_core::TrackerConfigError;
use symphony_core::issue::normalize_state;

use crate::error::{Provider, TrackerError};
use crate::tool::ToolResult;
use crate::transport::Method;

/// Everything outside `A-Za-z0-9-._~` (Elixir `URI.char_unreserved?/1`).
const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// `URI.encode(value, &URI.char_unreserved?/1)`.
pub fn encode_unreserved(value: &str) -> String {
    utf8_percent_encode(value, UNRESERVED).to_string()
}

/// `URI.encode_query/1` input preparation: scalars are stringified (`10` -> `"10"`, `true` ->
/// `"true"`, `null` -> `""`); nested arrays/objects are rejected (Elixir raised).
pub fn query_pairs(query: &Map<String, Value>) -> Option<Vec<(String, String)>> {
    query
        .iter()
        .map(|(key, value)| {
            let text = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => String::new(),
                Value::Array(_) | Value::Object(_) => return None,
            };
            Some((key.clone(), text))
        })
        .collect()
}

/// Status and decoded body of a REST call (no status mapping).
#[derive(Debug, Clone, PartialEq)]
pub struct RestResponse {
    /// HTTP status.
    pub status: u16,
    /// Decoded body (JSON, or a string for non-JSON responses).
    pub body: Value,
}

/// Human provider name used in log lines.
fn display_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Linear => "Linear",
        Provider::Github => "GitHub",
        Provider::Gitlab => "GitLab",
        Provider::Jira => "Jira",
        Provider::Asana => "Asana",
    }
}

/// `request_with_settings`: 2xx -> `Some(body)`; 404 with `allow_not_found` -> `None`; other statuses
/// are logged and become `{:<provider>_api_status, status}`.
pub fn map_status(
    provider: Provider,
    method: Method,
    path: &str,
    response: RestResponse,
    allow_not_found: bool,
) -> Result<Option<Value>, TrackerError> {
    match response.status {
        200..=299 => Ok(Some(response.body)),
        404 if allow_not_found => Ok(None),
        status => {
            tracing::error!(
                "{} API request failed status={} method={} path={}",
                display_name(provider),
                status,
                method,
                path
            );
            Err(TrackerError::ApiStatus { provider, status })
        }
    }
}

/// Elixir `Integer.parse(id)` consuming the whole string with a positive result (`"+5"`, `"05"`
/// accepted; `" 5"`, `"5a"`, `"0"` rejected).
pub fn parse_positive_id(value: &str) -> Option<u64> {
    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok().filter(|n| *n > 0)
}

/// `Enum.uniq/1` over ids/states (first occurrence wins).
pub fn uniq(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .iter()
        .filter(|v| seen.insert(v.as_str()))
        .cloned()
        .collect()
}

/// Normalized requested-state set (`normalize_state/1` of each entry).
pub fn state_set(states: &[String]) -> HashSet<String> {
    states.iter().map(|s| normalize_state(s)).collect()
}

/// Normalizes one page of candidate records: malformed records (`None`) are counted, logged as
/// `Dropping malformed <label> records count=<n>` and dropped; the rest are kept when their state is
/// in `requested`.
pub fn normalize_candidate_page<F>(
    raw: &[Value],
    label: &str,
    requested: &HashSet<String>,
    normalize: F,
) -> Vec<Issue>
where
    F: Fn(&Value) -> Option<Issue>,
{
    let normalized: Vec<Option<Issue>> = raw.iter().map(normalize).collect();
    let malformed = normalized.iter().filter(|i| i.is_none()).count();
    if malformed > 0 {
        tracing::warn!("Dropping malformed {label} records count={malformed}");
    }
    normalized
        .into_iter()
        .flatten()
        .filter(|issue| requested.contains(&normalize_state(issue.state.as_deref().unwrap_or(""))))
        .collect()
}

/// Present (non-blank) string field.
pub fn present_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// String field (any string, possibly blank).
pub fn opt_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_owned)
}

/// Trimmed string; blank or non-string -> `None` (`blank_to_nil`).
pub fn trimmed_string(value: Option<&Value>) -> Option<String> {
    let trimmed = value.and_then(Value::as_str)?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Builds a `native_ref` object from key/value pairs, dropping `null`/absent values.
pub fn native_ref(entries: Vec<(&str, Option<Value>)>) -> Map<String, Value> {
    entries
        .into_iter()
        .filter_map(|(k, v)| match v {
            None | Some(Value::Null) => None,
            Some(value) => Some((k.to_owned(), value)),
        })
        .collect()
}

/// Static description of a provider's raw REST passthrough tool.
#[derive(Debug)]
pub struct RestToolSpec {
    /// Provider for error mapping.
    pub provider: Provider,
    /// Tool name (`github_api`, ...).
    pub name: &'static str,
    /// Tool description (ends with `\n`, like the Elixir heredoc).
    pub description: &'static str,
    /// Allowed methods (also the schema enum).
    pub methods: &'static [&'static str],
    /// `method` property description.
    pub method_description: &'static str,
    /// `path` property description.
    pub path_description: &'static str,
    /// Query-parameter argument key (`params` for GitHub, `query` elsewhere).
    pub query_key: &'static str,
    /// Required path prefix (`/` or `/rest/api/3/`).
    pub path_prefix: &'static str,
    /// The credential error that gets the dedicated auth message.
    pub missing_auth: TrackerConfigError,
    /// Error messages.
    pub messages: RestToolMessages,
}

/// User-facing error messages of a REST tool.
#[derive(Debug)]
pub struct RestToolMessages {
    /// Arguments are not an object.
    pub invalid_arguments: &'static str,
    /// Bad or missing method.
    pub invalid_method: &'static str,
    /// Bad or missing path.
    pub invalid_path: &'static str,
    /// Query parameters are not an object (or contain nested values).
    pub invalid_query: &'static str,
    /// Missing credential.
    pub missing_auth: &'static str,
    /// Transport failure.
    pub request_failed: &'static str,
    /// Any other failure.
    pub execution_failed: &'static str,
}

/// A validated REST tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct RestCall {
    /// Method (validated against the allow-list).
    pub method: Method,
    /// Trimmed relative path (may carry an inline `?query`).
    pub path: String,
    /// Query parameters (object).
    pub query: Map<String, Value>,
    /// JSON body; only `null`/absent means "no body".
    pub body: Option<Value>,
}

/// Argument validation failures of a REST tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestArgError {
    /// Arguments are not an object.
    InvalidArguments,
    /// Bad method.
    InvalidMethod,
    /// Bad path.
    InvalidPath,
    /// Bad query/params.
    InvalidQuery,
}

impl RestToolSpec {
    /// `{"name", "description", "inputSchema"}`.
    pub fn spec(&self) -> Value {
        let mut properties = Map::new();
        properties.insert(
            "method".into(),
            json!({
                "type": "string",
                "enum": self.methods,
                "description": self.method_description,
            }),
        );
        properties.insert(
            "path".into(),
            json!({"type": "string", "description": self.path_description}),
        );
        properties.insert(
            self.query_key.into(),
            json!({
                "type": ["object", "null"],
                "description": "Optional query parameters.",
                "additionalProperties": true,
            }),
        );
        properties.insert(
            "body".into(),
            json!({"description": "Optional JSON request body."}),
        );
        json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["method", "path"],
                "properties": properties,
            }
        })
    }

    /// Validates the arguments (first failure wins: arguments, method, path, query).
    pub fn normalize(&self, arguments: &Value) -> Result<RestCall, RestArgError> {
        let Value::Object(args) = arguments else {
            return Err(RestArgError::InvalidArguments);
        };
        let method = args
            .get("method")
            .and_then(Value::as_str)
            .map(|m| m.trim().to_uppercase())
            .filter(|m| self.methods.contains(&m.as_str()))
            .and_then(|m| Method::parse(&m))
            .ok_or(RestArgError::InvalidMethod)?;
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| {
                p.starts_with(self.path_prefix)
                    && !["://", "\n", "\r", "\0"].iter().any(|bad| p.contains(bad))
            })
            .ok_or(RestArgError::InvalidPath)?
            .to_owned();
        let query = match args.get(self.query_key) {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) if query_pairs(map).is_some() => map.clone(),
            Some(_) => return Err(RestArgError::InvalidQuery),
        };
        let body = match args.get("body") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.clone()),
        };
        Ok(RestCall {
            method,
            path,
            query,
            body,
        })
    }

    /// Failure response for an argument error.
    pub fn argument_error(&self, err: RestArgError) -> ToolResult {
        let message = match err {
            RestArgError::InvalidArguments => self.messages.invalid_arguments,
            RestArgError::InvalidMethod => self.messages.invalid_method,
            RestArgError::InvalidPath => self.messages.invalid_path,
            RestArgError::InvalidQuery => self.messages.invalid_query,
        };
        ToolResult::error_message(message)
    }

    /// Failure response for a client error.
    pub fn client_error(&self, err: &TrackerError) -> ToolResult {
        match err {
            TrackerError::Config(c) if *c == self.missing_auth => {
                ToolResult::error_message(self.messages.missing_auth)
            }
            TrackerError::ApiRequest { provider, source } if *provider == self.provider => {
                ToolResult::failure(&json!({
                    "error": {
                        "message": self.messages.request_failed,
                        "reason": source.inspect(),
                    }
                }))
            }
            other => ToolResult::failure(&json!({
                "error": {
                    "message": self.messages.execution_failed,
                    "reason": other.inspect(),
                }
            })),
        }
    }

    /// Success/failure response for a completed HTTP exchange: `{"status", "body"}`.
    pub fn response(&self, response: &RestResponse) -> ToolResult {
        ToolResult::from_payload(
            (200..=299).contains(&response.status),
            &json!({"status": response.status, "body": response.body}),
        )
    }

    /// Unsupported tool name response.
    pub fn unsupported(&self, tool: Option<&str>) -> ToolResult {
        ToolResult::unsupported(tool, &[self.name])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreserved_encoding_matches_uri_char_unreserved() {
        assert_eq!(encode_unreserved("group/project"), "group%2Fproject");
        assert_eq!(encode_unreserved("a b~c.d-e_f"), "a%20b~c.d-e_f");
        assert_eq!(encode_unreserved("SYM-1"), "SYM-1");
    }

    #[test]
    fn positive_ids_follow_integer_parse() {
        assert_eq!(parse_positive_id("+5"), Some(5));
        assert_eq!(parse_positive_id("05"), Some(5));
        assert_eq!(parse_positive_id(" 5"), None);
        assert_eq!(parse_positive_id("5a"), None);
        assert_eq!(parse_positive_id("0"), None);
        assert_eq!(parse_positive_id("-3"), None);
        assert_eq!(parse_positive_id("not-a-number"), None);
    }

    #[test]
    fn query_pairs_stringify_scalars_and_reject_nested_values() {
        let Value::Object(map) = json!({"a": 10, "b": true, "c": "x", "d": null}) else {
            unreachable!()
        };
        assert_eq!(
            query_pairs(&map),
            Some(vec![
                ("a".into(), "10".into()),
                ("b".into(), "true".into()),
                ("c".into(), "x".into()),
                ("d".into(), String::new()),
            ])
        );
        let Value::Object(nested) = json!({"a": [1]}) else {
            unreachable!()
        };
        assert_eq!(query_pairs(&nested), None);
    }
}
