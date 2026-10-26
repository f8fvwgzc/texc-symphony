//! Secret redaction for everything Symphony persists or serves about a run.
//!
//! Agent events carry raw command lines and command output, which can contain credentials the
//! agent read or printed. Before an event reaches the run-history store (and from there
//! `GET /api/v1/runs/{id}/events`), [`Redactor`] replaces:
//!
//! * the exact values of the secrets Symphony knows (tracker credentials, the API token, the env
//!   vars stripped from agent children), and
//! * strings that look like well-known credential formats ([`PATTERNS`]): bearer headers,
//!   GitHub/GitLab/Linear/Slack/OpenAI/Anthropic/Google/AWS keys, JWTs, PEM private keys and
//!   `password=`/`api_key:`-style assignments,
//!
//! and blanks the values of JSON object keys that name a credential (`authorization`,
//! `api_key`, `password`, ...). Redaction is best effort: it lowers the blast radius of a leaked
//! event log, it does not make printing secrets safe.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;
use symphony_codex::{CodexEvent, CodexEventData, StreamMessage};
use symphony_core::config::SecretSource;
use symphony_core::config::llm::{LLM_API_KEY_FILE, LLM_GATEWAY_TOKEN_FILE};
use symphony_core::{EnvSource, Settings};

/// Replacement text for a redacted secret.
pub const REDACTED: &str = "[REDACTED]";

/// Shortest exact secret value that is scrubbed (shorter values would hit ordinary text).
pub const MIN_SECRET_LEN: usize = 8;

/// Credential formats redacted wherever they appear. Groups `header` and `assign` (when they
/// match) are preserved so the surrounding syntax stays readable (`Bearer [REDACTED]`,
/// `password=[REDACTED]`).
pub static PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    let patterns = [
        r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        r"(?P<header>(?i:bearer\s+|authorization:\s*basic\s+))[A-Za-z0-9._~+/=-]{12,}",
        r"\bgh[pousr]_[A-Za-z0-9]{20,}",
        r"\bgithub_pat_[A-Za-z0-9_]{20,}",
        r"\bglpat-[A-Za-z0-9_-]{20,}",
        r"\blin_(?:api|oauth)_[A-Za-z0-9]{20,}",
        r"\bxox[abprs]-[A-Za-z0-9-]{10,}",
        r"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}",
        r"\bAIza[0-9A-Za-z_-]{35}",
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        r"\b[0-9]/[0-9]{10,}:[0-9a-f]{20,}\b",
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
        // `\\?`: the quotes may be JSON-escaped (`password=\"...\"` inside a JSON string).
        r#"(?P<assign>(?i:api[_-]?key|access[_-]?token|auth[_-]?token|secret|password|passwd|client[_-]?secret)\\?["']?\s*[:=]\s*\\?["']?)[^\s"'\\,;&]{6,}"#,
    ];
    // The patterns are literals tested below; failing to compile them is a programming error.
    #[allow(clippy::expect_used)]
    Regex::new(&patterns.join("|")).expect("redaction patterns compile")
});

/// JSON object keys whose values are always redacted (compared case-insensitively).
const SECRET_KEYS: [&str; 14] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api_key",
    "apikey",
    "api-key",
    "access_token",
    "refresh_token",
    "id_token",
    "client_secret",
    "password",
    "private_key",
    "secret",
    "cookie",
];

/// Scrubs known secret values and credential-shaped strings (see the module docs).
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("secrets", &self.secrets.len())
            .finish()
    }
}

impl Redactor {
    /// Pattern-only redaction (no known secret values).
    pub fn patterns_only() -> Self {
        Self::default()
    }

    /// A redactor for `secrets` (values shorter than [`MIN_SECRET_LEN`] are ignored).
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            .filter(|s| s.len() >= MIN_SECRET_LEN)
            .collect();
        // Longest first, so a secret that contains another is replaced whole.
        secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        secrets.dedup();
        Self { secrets }
    }

    /// The secrets of `settings`: the resolved tracker key, the API token, the LLM contract's
    /// credential and the values of every env var stripped from agent children.
    pub fn from_settings(settings: &Settings, env: &dyn EnvSource) -> Self {
        let from_env = settings
            .secret_environment_names()
            .into_iter()
            .filter_map(|name| env.var(&name));
        // The LLM contract's key files (read whether or not the contract is on) and `api` backend
        // keys may live in files (mounted Secrets).
        let llm_files = [LLM_API_KEY_FILE, LLM_GATEWAY_TOKEN_FILE]
            .into_iter()
            .filter_map(|name| env.var(name))
            .map(|path| PathBuf::from(path.trim()));
        let files = symphony_core::config::backend::backend_secret_files(&settings.backends)
            .into_iter()
            .chain(llm_files)
            .filter_map(|path| SecretSource::File(path).read(env).ok());
        Self::new(
            from_env
                .chain(settings.tracker.api_key.clone())
                .chain(settings.server.api_token.clone())
                .chain(files),
        )
    }

    /// `text` with secrets replaced by [`REDACTED`].
    pub fn redact_str<'a>(&self, text: &'a str) -> Cow<'a, str> {
        let mut out = Cow::Borrowed(text);
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = Cow::Owned(out.replace(secret.as_str(), REDACTED));
            }
        }
        if PATTERNS.is_match(&out) {
            let replaced = PATTERNS
                .replace_all(&out, |caps: &Captures<'_>| {
                    let keep = caps
                        .name("header")
                        .or_else(|| caps.name("assign"))
                        .map_or("", |m| m.as_str());
                    format!("{keep}{REDACTED}")
                })
                .into_owned();
            out = Cow::Owned(replaced);
        }
        out
    }

    /// Redacts every string in `value` in place, and blanks values under credential keys.
    pub fn redact_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => {
                if let Cow::Owned(redacted) = self.redact_str(text) {
                    *text = redacted;
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| self.redact_value(item)),
            Value::Object(map) => {
                for (key, item) in map.iter_mut() {
                    let lower = key.to_ascii_lowercase();
                    if SECRET_KEYS.contains(&lower.as_str()) && !item.is_null() {
                        *item = Value::String(REDACTED.to_owned());
                    } else {
                        self.redact_value(item);
                    }
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn redact_message(&self, message: &mut StreamMessage) {
        self.redact_value(&mut message.payload);
        if let Cow::Owned(raw) = self.redact_str(&message.raw) {
            message.raw = raw;
        }
    }

    /// `event` with its messages, details and raw lines redacted (session ids, token counts
    /// and typed error reasons are kept).
    pub fn redact_event(&self, mut event: CodexEvent) -> CodexEvent {
        if let Some(usage) = event.usage.as_mut() {
            self.redact_value(usage);
        }
        match &mut event.data {
            CodexEventData::TurnCompleted(message)
            | CodexEventData::TurnInputRequired(message)
            | CodexEventData::ApprovalRequired(message)
            | CodexEventData::ToolCallCompleted(message)
            | CodexEventData::ToolCallFailed(message)
            | CodexEventData::UnsupportedToolCall(message)
            | CodexEventData::Notification(message)
            | CodexEventData::OtherMessage(message)
            | CodexEventData::ApprovalAutoApproved { message, .. } => self.redact_message(message),
            CodexEventData::TurnFailed { message, details }
            | CodexEventData::TurnCancelled { message, details } => {
                self.redact_message(message);
                self.redact_value(details);
            }
            CodexEventData::Malformed { raw } => {
                if let Cow::Owned(redacted) = self.redact_str(raw) {
                    *raw = redacted;
                }
            }
            CodexEventData::SessionStarted { .. }
            | CodexEventData::StartupFailed { .. }
            | CodexEventData::TurnEndedWithError { .. } => {}
        }
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use symphony_core::MapEnv;

    #[test]
    fn known_secret_values_are_replaced_longest_first() {
        let redactor = Redactor::new(["sup3r-s3cret".to_owned(), "sup3r-s3cret-and-more".into()]);
        assert_eq!(
            redactor.redact_str("a sup3r-s3cret-and-more b sup3r-s3cret"),
            "a [REDACTED] b [REDACTED]"
        );
        let short = Redactor::new(["abc".to_owned()]);
        assert_eq!(short.redact_str("abc"), "abc");
    }

    #[test]
    fn credential_shaped_strings_are_replaced() {
        let redactor = Redactor::patterns_only();
        for (input, expected) in [
            (
                "curl -H 'Authorization: Bearer abcdefghijklmnop1234'",
                "curl -H 'Authorization: Bearer [REDACTED]'",
            ),
            (
                "token ghp_0123456789abcdefghijABCDEFGHIJ",
                "token [REDACTED]",
            ),
            (
                "GH=github_pat_11ABCDEFG0123456789_abcdefghij",
                "GH=[REDACTED]",
            ),
            ("glpat-abcdefghij0123456789", "[REDACTED]"),
            (
                "LINEAR=lin_api_abcdefghij0123456789xyz",
                "LINEAR=[REDACTED]",
            ),
            ("sk-ant-api03-abcdefghij0123456789", "[REDACTED]"),
            (
                "OPENAI_API_KEY=sk-proj-abcdefghij0123456789",
                "OPENAI_API_KEY=[REDACTED]",
            ),
            ("key AKIAABCDEFGHIJKLMNOP!", "key [REDACTED]!"),
            ("password=hunter2hunter2", "password=[REDACTED]"),
            (
                "\"api_key\": \"0123456789abcdef\"",
                "\"api_key\": \"[REDACTED]\"",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----",
                "[REDACTED]",
            ),
            (
                "jwt eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.SflKxwRJSMeKKF2QT4fw",
                "jwt [REDACTED]",
            ),
        ] {
            assert_eq!(redactor.redact_str(input), expected, "{input}");
        }
        for harmless in [
            "cargo test -p symphony-runtime",
            "a basic implementation of token authentication",
            "total_tokens=123456",
            "input_tokens: 4096",
            "see https://github.com/f8fvwgzc/texc-symphony/pull/2",
        ] {
            assert_eq!(redactor.redact_str(harmless), harmless);
        }
    }

    #[test]
    fn json_values_are_redacted_recursively_and_by_key() {
        let redactor = Redactor::new(["tracker-token-value".to_owned()]);
        let mut value = json!({
            "method": "item/completed",
            "params": {"item": {
                "command": "echo tracker-token-value",
                "aggregatedOutput": "Bearer abcdefghijklmnopqrstu\nok",
                "headers": {"Authorization": "Basic xyz", "Accept": "json"},
            }},
            "usage": {"total_tokens": 10, "access_token": null},
        });
        redactor.redact_value(&mut value);
        assert_eq!(
            value,
            json!({
                "method": "item/completed",
                "params": {"item": {
                    "command": "echo [REDACTED]",
                    "aggregatedOutput": "Bearer [REDACTED]\nok",
                    "headers": {"Authorization": "[REDACTED]", "Accept": "json"},
                }},
                "usage": {"total_tokens": 10, "access_token": null},
            })
        );
    }

    #[test]
    fn events_are_redacted_in_payload_and_raw_line() {
        let redactor = Redactor::new(["task-token-0123456789".to_owned()]);
        let payload = json!({"method": "item/completed",
            "params": {"item": {"aggregatedOutput": "GH_TOKEN=task-token-0123456789"}}});
        let event = CodexEvent::new(
            CodexEventData::Notification(StreamMessage {
                raw: payload.to_string(),
                payload,
            }),
            None,
            None,
        );
        let redacted = redactor.redact_event(event);
        let json = redacted.to_json().to_string();
        assert!(!json.contains("task-token-0123456789"), "{json}");
        assert!(json.contains("GH_TOKEN=[REDACTED]"), "{json}");
        assert_eq!(redacted.method(), Some("item/completed"));
    }

    #[test]
    fn settings_secrets_come_from_the_stripped_env_vars_and_tokens() {
        let mut settings = Settings::default();
        settings.tracker.kind = Some("github".into());
        settings.server.api_token = Some("api-token-for-symphony".into());
        let env = MapEnv::new().with("GITHUB_TOKEN", "plain-gh-token-value");
        let redactor = Redactor::from_settings(&settings, &env);
        assert_eq!(
            redactor.redact_str("plain-gh-token-value api-token-for-symphony"),
            "[REDACTED] [REDACTED]"
        );
    }

    #[test]
    fn backend_keys_are_stripped_and_redacted() {
        use symphony_core::config;
        let dir = tempfile::tempdir().unwrap();
        let key_file = dir.path().join("api-key");
        std::fs::write(&key_file, "file-key-0123456789\n").unwrap();
        let serde_json::Value::Object(map) = serde_json::json!({"backends": {
            "cc": {"kind": "claude_code", "api_key_env": "MY_CLAUDE_KEY"},
            "api": {"kind": "api", "provider": "openai", "model": "m",
                "api_key_file": key_file.display().to_string()},
        }}) else {
            unreachable!("an object")
        };
        let env = MapEnv::new().with("MY_CLAUDE_KEY", "claude-key-0123456789");
        let settings = config::parse(&map, &env).unwrap();
        assert!(
            settings
                .secret_environment_names()
                .contains(&"MY_CLAUDE_KEY".to_owned()),
            "removed from hooks, Codex and the api shell"
        );
        let redactor = Redactor::from_settings(&settings, &env);
        assert_eq!(
            redactor.redact_str("claude-key-0123456789 file-key-0123456789"),
            "[REDACTED] [REDACTED]"
        );
    }

    #[test]
    fn stray_llm_keys_are_stripped_and_redacted_while_the_contract_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let token_file = dir.path().join("gateway-token");
        std::fs::write(&token_file, "stray-gateway-token-0123\n").unwrap();
        // No LLM_PROVIDER or LLM_GATEWAY_URL: the contract is off.
        let env = MapEnv::new()
            .with("LLM_API_KEY", "stray-llm-key-0123456789")
            .with("LLM_GATEWAY_TOKEN_FILE", token_file.display().to_string());
        let settings = symphony_core::config::parse(&serde_json::Map::new(), &env).unwrap();
        assert!(settings.llm.is_none());
        let names = settings.secret_environment_names();
        for name in ["LLM_API_KEY", "LLM_API_KEY_FILE", "LLM_GATEWAY_TOKEN_FILE"] {
            assert!(
                names.contains(&name.to_owned()),
                "{name} stripped: {names:?}"
            );
        }
        let redactor = Redactor::from_settings(&settings, &env);
        assert_eq!(
            redactor.redact_str("stray-llm-key-0123456789 stray-gateway-token-0123"),
            "[REDACTED] [REDACTED]"
        );
    }
}
