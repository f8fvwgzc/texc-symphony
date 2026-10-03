//! The normalized tracker work item (`SymphonyElixir.Tracker.Issue`) and the normalization helpers that
//! every tracker adapter shares.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A blocker reference (`%{id:, identifier:, state:}` in Elixir).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BlockerRef {
    /// Tracker id of the blocking issue.
    pub id: Option<String>,
    /// Human key of the blocking issue.
    pub identifier: Option<String>,
    /// Provider-native state of the blocking issue.
    pub state: Option<String>,
}

/// Normalized work item used by the orchestrator.
///
/// `id` is the stable dispatch identity within the tracker scope; `identifier` is the human key that also
/// names the workspace; `native_ref` carries non-secret provider ids for agent tools. Fields stay
/// optional because the memory tracker and tests build partial issues.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Issue {
    /// Stable dispatch identity (Linear UUID, GitHub number, GitLab IID, ...).
    pub id: Option<String>,
    /// Non-secret provider identifiers for provider-native tools.
    pub native_ref: Option<Map<String, Value>>,
    /// Human key (`MT-1`, `GH-12`, ...).
    pub identifier: Option<String>,
    /// Title.
    pub title: Option<String>,
    /// Body text.
    pub description: Option<String>,
    /// Priority (Linear only; 1..4 sort first).
    pub priority: Option<i64>,
    /// Provider-native state spelling (not lowercased).
    pub state: Option<String>,
    /// Linear branch name.
    pub branch_name: Option<String>,
    /// Web URL.
    pub url: Option<String>,
    /// Provider assignee id.
    pub assignee_id: Option<String>,
    /// Blocking issues.
    pub blocked_by: Vec<BlockerRef>,
    /// Normalized labels (trimmed, lowercased, blanks dropped, deduplicated).
    pub labels: Vec<String>,
    /// Adapter-derived eligibility; defaults to `false` like the Elixir struct.
    pub dispatchable: bool,
    /// Creation time (UTC).
    pub created_at: Option<DateTime<Utc>>,
    /// Last update time (UTC).
    pub updated_at: Option<DateTime<Utc>>,
}

impl Issue {
    /// `Issue.label_names/1`: the labels as stored.
    pub fn label_names(&self) -> &[String] {
        &self.labels
    }

    /// `Issue.routable?/2`: `dispatchable` and every required label (trim + lowercase) is present.
    ///
    /// An empty requirement list is satisfied; a blank required label normalizes to `""` and never
    /// matches, because adapters drop blank labels.
    pub fn routable<S: AsRef<str>>(&self, required_labels: &[S]) -> bool {
        if !self.dispatchable {
            return false;
        }
        let have: Vec<String> = self.labels.iter().map(|l| normalize_label(l)).collect();
        required_labels
            .iter()
            .all(|required| have.contains(&normalize_label(required.as_ref())))
    }
}

/// `String.trim |> String.downcase` (full Unicode lowercasing).
pub fn normalize_label(label: &str) -> String {
    label.trim().to_lowercase()
}

/// Issue/tracker state normalization (`Schema.normalize_issue_state/1`): trim + Unicode lowercase.
pub fn normalize_state(state: &str) -> String {
    state.trim().to_lowercase()
}

/// Adapter label normalization: trim, lowercase, drop blanks, dedupe keeping first occurrence.
pub fn normalize_labels<I, S>(labels: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out: Vec<String> = Vec::new();
    for label in labels {
        let normalized = normalize_label(label.as_ref());
        if !normalized.is_empty() && !out.contains(&normalized) {
            out.push(normalized);
        }
    }
    out
}

/// `true` for a string with non-whitespace content (the adapters' `present_string?/1`).
pub fn present_string(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

/// ISO-8601 timestamp with a mandatory offset (`Z` or `±HH:MM`), converted to UTC
/// (Elixir `DateTime.from_iso8601/1`). Returns `None` for anything else.
pub fn parse_datetime(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Like [`parse_datetime`], but first rewrites a trailing `±HHMM` offset to `±HH:MM`
/// (Jira's `2024-01-02T03:04:05.000+0000` format).
pub fn parse_datetime_compact_offset(raw: &str) -> Option<DateTime<Utc>> {
    let bytes = raw.as_bytes();
    let n = bytes.len();
    if n >= 5 {
        let tail = &bytes[n - 5..];
        if matches!(tail[0], b'+' | b'-') && tail[1..].iter().all(u8::is_ascii_digit) {
            let normalized = format!("{}:{}", &raw[..n - 2], &raw[n - 2..]);
            return parse_datetime(&normalized);
        }
    }
    parse_datetime(raw)
}

/// ISO-8601 rendering used for prompts and payloads (`2026-02-26T18:06:48Z`, sub-second digits only when
/// present).
pub fn format_datetime(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labeled(labels: &[&str], dispatchable: bool) -> Issue {
        Issue {
            labels: labels.iter().map(|s| s.to_string()).collect(),
            dispatchable,
            ..Issue::default()
        }
    }

    #[test]
    fn tracker_issue_helpers() {
        let issue = Issue {
            id: Some("abc".into()),
            labels: vec!["frontend".into(), "infra".into()],
            ..Issue::default()
        };
        assert_eq!(issue.label_names(), ["frontend", "infra"]);
        assert!(!Issue::default().dispatchable);
    }

    #[test]
    fn tracker_issue_routing_requires_every_configured_label() {
        let issue = labeled(&[" Symphony ", "JavaScript"], true);
        let none: [&str; 0] = [];
        assert!(issue.routable(&none));
        assert!(issue.routable(&["symphony"]));
        assert!(issue.routable(&["SYMPHONY", "javascript"]));
        assert!(!issue.routable(&["symph"]));
        assert!(!issue.routable(&[" "]));
        assert!(!issue.routable(&["symphony", "security"]));

        let blocked = labeled(&[" Symphony ", "JavaScript"], false);
        assert!(!blocked.routable(&none));
        assert!(!blocked.routable(&["symphony"]));
    }

    #[test]
    fn normalize_labels_trims_lowercases_dedupes_and_drops_blanks() {
        assert_eq!(
            normalize_labels([" Backend ", "backend", "", "  ", "UI", "ui "]),
            vec!["backend".to_string(), "ui".to_string()]
        );
        assert_eq!(normalize_state("  In Progress "), "in progress");
        assert!(present_string(Some(" x ")));
        assert!(!present_string(Some("  ")));
        assert!(!present_string(None));
    }

    #[test]
    fn datetimes_parse_with_offsets_and_render_as_utc_z() {
        let dt = parse_datetime("2026-02-26T19:06:48+01:00").expect("valid");
        assert_eq!(format_datetime(&dt), "2026-02-26T18:06:48Z");
        let millis = parse_datetime("2026-02-26T18:06:48.123Z").expect("valid");
        assert_eq!(format_datetime(&millis), "2026-02-26T18:06:48.123Z");
        assert!(parse_datetime("2026-02-26T18:06:48").is_none());
        assert!(parse_datetime("garbage").is_none());
        let jira = parse_datetime_compact_offset("2024-01-02T03:04:05.000+0200").expect("valid");
        assert_eq!(format_datetime(&jira), "2024-01-02T01:04:05Z");
        assert_eq!(
            parse_datetime_compact_offset("2024-01-02T03:04:05Z").map(|d| format_datetime(&d)),
            Some("2024-01-02T03:04:05Z".to_string())
        );
    }

    #[test]
    fn issue_serde_round_trips_with_defaults() {
        let issue: Issue =
            serde_json::from_str(r#"{"identifier":"MT-1","created_at":"2026-02-26T18:06:48Z"}"#)
                .expect("deserialize");
        assert_eq!(issue.identifier.as_deref(), Some("MT-1"));
        assert!(!issue.dispatchable);
        assert!(issue.labels.is_empty());
        let json = serde_json::to_value(&issue).expect("serialize");
        assert_eq!(json["created_at"], "2026-02-26T18:06:48Z");
        assert_eq!(json["blocked_by"], serde_json::json!([]));
        let back: Issue = serde_json::from_value(json).expect("round trip");
        assert_eq!(back, issue);
    }
}
