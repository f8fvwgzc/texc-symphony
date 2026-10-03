//! Tracker errors.
//!
//! `Display` keeps the Elixir snake_case reason tags (`github_api_status: 503`,
//! `missing_linear_api_token`) because log lines embed them. [`TrackerError::inspect`] renders the
//! Elixir `inspect/1` form used in agent-tool `"reason"` fields (`:timeout`,
//! `{:github_api_status, 503}`), and [`TrackerError::category`] maps every error onto the portable
//! SPEC §11.4 categories.

use std::fmt;

use serde_json::Value;
use symphony_core::{ConfigError, TrackerConfigError};

use crate::transport::TransportError;

/// The provider an error came from (the prefix of the Elixir reason atoms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// Linear GraphQL.
    Linear,
    /// GitHub Issues REST.
    Github,
    /// GitLab Issues REST.
    Gitlab,
    /// Jira Cloud REST v3.
    Jira,
    /// Asana REST.
    Asana,
}

impl Provider {
    /// Lowercase tag used in reason atoms (`github`, `gitlab`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Jira => "jira",
            Self::Asana => "asana",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// SPEC §11.4 portable error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    /// `tracker.kind` is not a supported adapter.
    UnsupportedTrackerKind,
    /// Configuration is missing or invalid.
    InvalidTrackerConfig,
    /// The tracker credential is missing.
    MissingTrackerSecret,
    /// Transport failure before a response was received.
    TrackerRequest,
    /// Non-success HTTP status.
    TrackerStatus,
    /// Malformed or semantically invalid payload.
    TrackerResponse,
    /// Pagination integrity failure.
    TrackerPagination,
    /// HTTP 429 after retries.
    TrackerRateLimited,
}

impl ErrorCategory {
    /// The snake_case category name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedTrackerKind => "unsupported_tracker_kind",
            Self::InvalidTrackerConfig => "invalid_tracker_config",
            Self::MissingTrackerSecret => "missing_tracker_secret",
            Self::TrackerRequest => "tracker_request",
            Self::TrackerStatus => "tracker_status",
            Self::TrackerResponse => "tracker_response",
            Self::TrackerPagination => "tracker_pagination",
            Self::TrackerRateLimited => "tracker_rate_limited",
        }
    }
}

impl fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every failure a tracker read, write or tool call can produce.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TrackerError {
    /// `tracker.kind` is absent.
    #[error("missing_tracker_kind")]
    MissingTrackerKind,
    /// `tracker.kind` is not one of the six adapters (`{:unsupported_tracker_kind, kind}`).
    #[error("unsupported_tracker_kind: {0:?}")]
    UnsupportedTrackerKind(String),
    /// Adapter configuration/secret failure (the core validation atoms, e.g. `missing_github_token`).
    #[error(transparent)]
    Config(#[from] TrackerConfigError),
    /// `assignee: me` could not be resolved through the Linear `viewer` query.
    #[error("missing_linear_viewer_identity")]
    MissingLinearViewerIdentity,
    /// Top-level GraphQL `errors` without `data.issues.nodes` (`{:linear_graphql_errors, errors}`).
    #[error("linear_graphql_errors: {0}")]
    LinearGraphqlErrors(Value),
    /// Non-success HTTP status (`{:<provider>_api_status, status}`).
    #[error("{provider}_api_status: {status}")]
    ApiStatus {
        /// Provider.
        provider: Provider,
        /// HTTP status code.
        status: u16,
    },
    /// Transport failure (`{:<provider>_api_request, reason}`); never contains credentials.
    #[error("{provider}_api_request: {source}")]
    ApiRequest {
        /// Provider.
        provider: Provider,
        /// The scrubbed transport failure.
        source: TransportError,
    },
    /// Unexpected payload shape or a malformed record in an id refresh (`:<provider>_unknown_payload`).
    #[error("{0}_unknown_payload")]
    UnknownPayload(Provider),
    /// `fetch_issues_by_ids` got an id that is not a positive integer (`:invalid_<provider>_issue_id`).
    #[error("invalid_{0}_issue_id")]
    InvalidIssueId(Provider),
    /// Unsupported HTTP method for the provider (`:invalid_<provider>_method`).
    #[error("invalid_{0}_method")]
    InvalidMethod(Provider),
    /// The provider said there is another page but gave no cursor
    /// (`linear_missing_end_cursor`, `jira_missing_next_page_token`, `asana_missing_next_page_offset`).
    #[error("{}", missing_cursor_tag(*.0))]
    MissingPageCursor(Provider),
    /// New in the Rust port: more than [`crate::MAX_PAGES`] pages were requested in one read.
    #[error("{0}_pagination_limit_exceeded")]
    PaginationLimitExceeded(Provider),
    /// New in the Rust port: the provider returned a cursor it had already returned in this read.
    #[error("{0}_pagination_repeated_cursor")]
    PaginationRepeatedCursor(Provider),
}

fn missing_cursor_tag(provider: Provider) -> &'static str {
    match provider {
        Provider::Linear => "linear_missing_end_cursor",
        Provider::Jira => "jira_missing_next_page_token",
        Provider::Asana => "asana_missing_next_page_offset",
        Provider::Github => "github_missing_next_page",
        Provider::Gitlab => "gitlab_missing_next_page",
    }
}

impl From<ConfigError> for TrackerError {
    fn from(err: ConfigError) -> Self {
        match err {
            ConfigError::Tracker(inner) => Self::Config(inner),
            ConfigError::UnsupportedTrackerKind(kind) => Self::UnsupportedTrackerKind(kind),
            // Remaining variants concern workflow loading, which never reaches the tracker crate;
            // `missing_tracker_kind` is the only sensible tracker-side reading of them.
            _ => Self::MissingTrackerKind,
        }
    }
}

impl TrackerError {
    /// Wraps a transport failure for `provider`.
    pub fn request(provider: Provider, source: TransportError) -> Self {
        Self::ApiRequest { provider, source }
    }

    /// The bare snake_case reason tag (no payload), e.g. `github_api_status`.
    pub fn tag(&self) -> String {
        match self {
            Self::MissingTrackerKind => "missing_tracker_kind".into(),
            Self::UnsupportedTrackerKind(_) => "unsupported_tracker_kind".into(),
            Self::Config(err) => err.tag(),
            Self::MissingLinearViewerIdentity => "missing_linear_viewer_identity".into(),
            Self::LinearGraphqlErrors(_) => "linear_graphql_errors".into(),
            Self::ApiStatus { provider, .. } => format!("{provider}_api_status"),
            Self::ApiRequest { provider, .. } => format!("{provider}_api_request"),
            other => other.to_string(),
        }
    }

    /// Elixir `inspect/1` of the reason term (used for the `"reason"` field of tool error payloads).
    pub fn inspect(&self) -> String {
        match self {
            Self::UnsupportedTrackerKind(kind) => {
                format!("{{:unsupported_tracker_kind, {}}}", inspect_string(kind))
            }
            Self::LinearGraphqlErrors(errors) => {
                format!("{{:linear_graphql_errors, {errors}}}")
            }
            Self::ApiStatus { provider, status } => {
                format!("{{:{provider}_api_status, {status}}}")
            }
            Self::ApiRequest { provider, source } => {
                format!("{{:{provider}_api_request, {}}}", source.inspect())
            }
            other => format!(":{}", other.tag()),
        }
    }

    /// SPEC §11.4 category.
    pub fn category(&self) -> ErrorCategory {
        use TrackerConfigError as C;
        match self {
            Self::UnsupportedTrackerKind(_) => ErrorCategory::UnsupportedTrackerKind,
            Self::MissingTrackerKind => ErrorCategory::InvalidTrackerConfig,
            Self::Config(
                C::MissingLinearApiToken
                | C::MissingGithubToken
                | C::MissingGitlabApiKey
                | C::MissingJiraApiToken
                | C::MissingAsanaApiKey,
            ) => ErrorCategory::MissingTrackerSecret,
            Self::Config(_) | Self::InvalidMethod(_) | Self::InvalidIssueId(_) => {
                ErrorCategory::InvalidTrackerConfig
            }
            Self::ApiRequest { .. } => ErrorCategory::TrackerRequest,
            Self::ApiStatus { status: 429, .. } => ErrorCategory::TrackerRateLimited,
            Self::ApiStatus { .. } => ErrorCategory::TrackerStatus,
            Self::MissingLinearViewerIdentity
            | Self::LinearGraphqlErrors(_)
            | Self::UnknownPayload(_) => ErrorCategory::TrackerResponse,
            Self::MissingPageCursor(_)
            | Self::PaginationLimitExceeded(_)
            | Self::PaginationRepeatedCursor(_) => ErrorCategory::TrackerPagination,
        }
    }

    /// Operator-facing summary used by the orchestrator when a candidate fetch fails.
    pub fn user_message(&self) -> String {
        match self {
            Self::Config(TrackerConfigError::MissingLinearApiToken) => {
                "Tracker API token missing in WORKFLOW.md".into()
            }
            Self::Config(TrackerConfigError::MissingLinearProjectSlug) => {
                "Tracker project scope missing in WORKFLOW.md".into()
            }
            other => other.inspect(),
        }
    }
}

/// Elixir `inspect/1` of a binary: double-quoted with `\"`, `\\` and control characters escaped.
pub fn inspect_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_inspect_and_categories() {
        let status = TrackerError::ApiStatus {
            provider: Provider::Github,
            status: 503,
        };
        assert_eq!(status.to_string(), "github_api_status: 503");
        assert_eq!(status.tag(), "github_api_status");
        assert_eq!(status.inspect(), "{:github_api_status, 503}");
        assert_eq!(status.category(), ErrorCategory::TrackerStatus);
        let limited = TrackerError::ApiStatus {
            provider: Provider::Jira,
            status: 429,
        };
        assert_eq!(limited.category(), ErrorCategory::TrackerRateLimited);

        let timeout = TrackerError::request(Provider::Linear, TransportError::Timeout);
        assert_eq!(timeout.inspect(), "{:linear_api_request, :timeout}");
        assert_eq!(timeout.category(), ErrorCategory::TrackerRequest);

        let token = TrackerError::Config(TrackerConfigError::MissingGithubToken);
        assert_eq!(token.inspect(), ":missing_github_token");
        assert_eq!(token.category(), ErrorCategory::MissingTrackerSecret);

        assert_eq!(
            TrackerError::MissingPageCursor(Provider::Jira).to_string(),
            "jira_missing_next_page_token"
        );
        assert_eq!(
            TrackerError::MissingPageCursor(Provider::Linear).inspect(),
            ":linear_missing_end_cursor"
        );
        assert_eq!(
            TrackerError::UnknownPayload(Provider::Asana).inspect(),
            ":asana_unknown_payload"
        );
        assert_eq!(
            TrackerError::InvalidIssueId(Provider::Gitlab).to_string(),
            "invalid_gitlab_issue_id"
        );
        assert_eq!(
            TrackerError::UnsupportedTrackerKind("future-tracker".into()).inspect(),
            "{:unsupported_tracker_kind, \"future-tracker\"}"
        );
        assert_eq!(
            TrackerError::PaginationLimitExceeded(Provider::Github).category(),
            ErrorCategory::TrackerPagination
        );
    }

    #[test]
    fn user_messages_match_orchestrator_special_cases() {
        assert_eq!(
            TrackerError::Config(TrackerConfigError::MissingLinearApiToken).user_message(),
            "Tracker API token missing in WORKFLOW.md"
        );
        assert_eq!(
            TrackerError::Config(TrackerConfigError::MissingLinearProjectSlug).user_message(),
            "Tracker project scope missing in WORKFLOW.md"
        );
    }

    #[test]
    fn inspect_string_escapes_like_elixir() {
        assert_eq!(inspect_string("tool"), "\"tool\"");
        assert_eq!(inspect_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }
}
