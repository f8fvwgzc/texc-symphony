//! Agent-tool scoping (2026-10-25 audit, item A2).
//!
//! The provider-native agent tools run with the operator's tracker credential, so every call is
//! checked against `tracker.agent_tools` and the configured repository/project **before** any
//! request is sent:
//!
//! - **Methods**: the intersection of the tool's methods and `tracker.agent_tools.methods`
//!   (default `GET`, `POST`, `PATCH`, `PUT`; `DELETE` only when listed).
//! - **Paths**: each adapter accepts only paths inside its configured scope (see the adapters);
//!   `.`/`..` segments (also percent-encoded), empty segments, encoded slashes outside the one
//!   place an adapter expects them, backslashes, NUL and fragments are refused outright.
//! - **Admin endpoints** (webhooks, keys, members, secrets, branch protection, the repository or
//!   project resource itself) need `tracker.agent_tools.allow_admin_paths: true`.
//!
//! A refusal is a tool failure `{"error": {"message", "rule"}}`; `rule` is a stable snake_case tag.

use percent_encoding::percent_decode_str;
use serde_json::{Value, json};
use symphony_core::config::schema::AgentToolSettings;

use crate::rest::RestToolSpec;
use crate::tool::ToolResult;
use crate::transport::Method;

/// Why an agent tool call was refused before any request was sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denied {
    /// Stable snake_case rule tag (`method_not_allowed`, `path_out_of_scope`, `admin_path`, ...).
    pub rule: &'static str,
    /// Human-readable explanation for the agent.
    pub message: String,
}

impl Denied {
    /// A refusal under `rule`.
    pub fn new(rule: &'static str, message: impl Into<String>) -> Self {
        Self {
            rule,
            message: message.into(),
        }
    }

    /// The tool failure sent back to the agent.
    pub fn tool_result(&self) -> ToolResult {
        ToolResult::failure(&json!({"error": {"message": self.message, "rule": self.rule}}))
    }
}

/// One path segment: as sent (`raw`) and percent-decoded once (`decoded`, what the provider sees).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The segment as written by the agent.
    pub raw: String,
    /// The segment percent-decoded once.
    pub decoded: String,
}

impl Segment {
    /// `true` when the decoded segment equals `value` ignoring ASCII case.
    pub fn is(&self, value: &str) -> bool {
        self.decoded.eq_ignore_ascii_case(value)
    }
}

/// The methods `spec` may advertise under `policy`, in the tool's own order.
pub fn allowed_methods(spec: &RestToolSpec, policy: &AgentToolSettings) -> Vec<&'static str> {
    spec.methods
        .iter()
        .copied()
        .filter(|method| policy.allows_method(method))
        .collect()
}

/// Refuses a method outside `tracker.agent_tools.methods`.
pub fn check_method(
    spec: &RestToolSpec,
    method: Method,
    policy: &AgentToolSettings,
) -> Result<(), Denied> {
    let name = method.as_str();
    if policy.allows_method(name) {
        return Ok(());
    }
    Err(Denied::new(
        "method_not_allowed",
        format!(
            "`{}` may not use {name}: it is not in tracker.agent_tools.methods (allowed: {}).",
            spec.name,
            allowed_methods(spec, policy).join(", ")
        ),
    ))
}

/// Splits the path part of a tool path (an inline `?query` is ignored) into segments, refusing
/// anything a provider could resolve outside the literal path: `.`/`..` (also encoded), empty
/// segments (a single trailing `/` is allowed), backslashes, NUL, fragments and undecodable
/// percent escapes. Encoded slashes are kept in `decoded`; adapters refuse them where they do not
/// expect one.
pub fn split_path(tool: &str, path: &str) -> Result<Vec<Segment>, Denied> {
    let unsafe_path =
        |why: &str| Denied::new("unsafe_path", format!("`{tool}.path` was refused: {why}."));
    if path.contains('#') {
        return Err(unsafe_path("fragments are not allowed"));
    }
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    let Some(rest) = path.strip_prefix('/') else {
        return Err(unsafe_path("it must start with `/`"));
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Ok(Vec::new());
    }
    rest.split('/')
        .map(|raw| {
            let decoded = percent_decode_str(raw)
                .decode_utf8()
                .map_err(|_| unsafe_path("it contains an undecodable percent escape"))?
                .into_owned();
            if raw.is_empty() {
                return Err(unsafe_path("it contains an empty segment"));
            }
            if decoded.split('/').any(|part| part == "." || part == "..") {
                return Err(unsafe_path("`.` and `..` segments are not allowed"));
            }
            if decoded.contains(['\\', '\0']) {
                return Err(unsafe_path("backslashes and NUL are not allowed"));
            }
            Ok(Segment {
                raw: raw.to_owned(),
                decoded,
            })
        })
        .collect()
}

/// Refuses an encoded `/` inside any of `segments`.
pub fn refuse_encoded_slashes(tool: &str, segments: &[Segment]) -> Result<(), Denied> {
    if segments.iter().any(|s| s.decoded.contains('/')) {
        return Err(Denied::new(
            "unsafe_path",
            format!("`{tool}.path` was refused: encoded `/` is not allowed here."),
        ));
    }
    Ok(())
}

/// The refusal for a path outside the configured scope.
pub fn out_of_scope(tool: &str, scope: &str) -> Denied {
    Denied::new(
        "path_out_of_scope",
        format!("`{tool}` is limited to {scope}."),
    )
}

/// The refusal for an administrative endpoint.
pub fn admin_path(tool: &str, what: &str) -> Denied {
    Denied::new(
        "admin_path",
        format!(
            "`{tool}` may not call {what}: administrative endpoints need tracker.agent_tools.allow_admin_paths: true."
        ),
    )
}

/// `true` when `rest` (the segments below the scope root) starts with one of `admin` (a list of
/// `/`-separated subpaths; `*` matches any single segment).
pub fn matches_admin(rest: &[Segment], admin: &[&str]) -> bool {
    admin.iter().any(|pattern| {
        let parts: Vec<&str> = pattern.split('/').collect();
        parts.len() <= rest.len()
            && parts
                .iter()
                .zip(rest)
                .all(|(part, segment)| *part == "*" || segment.is(part))
    })
}

/// The scope root itself (the repository or project resource) is read-only unless admin paths
/// are allowed.
pub fn check_scope_root(
    tool: &str,
    rest: &[Segment],
    method: Method,
    policy: &AgentToolSettings,
    what: &str,
) -> Result<(), Denied> {
    if rest.is_empty() && method != Method::Get && !policy.allow_admin_paths {
        return Err(admin_path(tool, &format!("{method} on {what}")));
    }
    Ok(())
}

/// String values of `value` at `pointer` when it is an array (`[]` otherwise).
pub fn string_array<'a>(value: Option<&'a Value>, pointer: &str) -> Vec<&'a str> {
    value
        .and_then(|v| v.pointer(pointer))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(path: &str) -> Vec<String> {
        split_path("t", path)
            .unwrap()
            .into_iter()
            .map(|s| s.decoded)
            .collect()
    }

    #[test]
    fn paths_split_into_decoded_segments() {
        assert_eq!(decoded("/repos/o/r/issues/"), ["repos", "o", "r", "issues"]);
        assert_eq!(decoded("/repos/o/r?x=1/.."), ["repos", "o", "r"]);
        assert_eq!(decoded("/projects/g%2Fp"), ["projects", "g/p"]);
        assert_eq!(decoded("/"), Vec::<String>::new());
    }

    #[test]
    fn traversal_and_odd_paths_are_refused() {
        for path in [
            "/repos/o/r/../../user",
            "/repos/o/r/%2e%2e/x",
            "/repos/o/r/%2E%2E",
            "/repos/o/r/./x",
            "/repos//o/r",
            "/repos/o/r#frag",
            "/repos/o/r/%5Cx",
            "/repos/o/r/%00",
            "/repos/o/r/%ff",
            "/projects/g%2F..%2Fother",
            "repos/o/r",
        ] {
            let err = split_path("t", path).unwrap_err();
            assert_eq!(err.rule, "unsafe_path", "{path}");
        }
    }

    #[test]
    fn admin_patterns_match_prefixes_and_wildcards() {
        let rest = split_path("t", "/branches/main/protection/x").unwrap();
        assert!(matches_admin(&rest, &["branches/*/protection"]));
        assert!(!matches_admin(&rest, &["hooks"]));
        let rest = split_path("t", "/Hooks/1").unwrap();
        assert!(matches_admin(&rest, &["hooks"]));
        assert!(!matches_admin(&[], &["hooks"]));
    }

    #[test]
    fn method_policy_is_enforced() {
        let spec = &crate::github::TOOL;
        let policy = AgentToolSettings::default();
        assert_eq!(
            allowed_methods(spec, &policy),
            ["GET", "POST", "PATCH", "PUT"]
        );
        assert!(check_method(spec, Method::Get, &policy).is_ok());
        let denied = check_method(spec, Method::Delete, &policy).unwrap_err();
        assert_eq!(denied.rule, "method_not_allowed");
        assert!(denied.message.contains("allowed: GET, POST, PATCH, PUT"));
        let policy = AgentToolSettings {
            methods: vec!["GET".into(), "DELETE".into()],
            ..AgentToolSettings::default()
        };
        assert_eq!(allowed_methods(spec, &policy), ["GET", "DELETE"]);
        assert!(check_method(spec, Method::Delete, &policy).is_ok());
        assert!(check_method(spec, Method::Post, &policy).is_err());
    }
}
