//! Token accounting and rate-limit extraction (`Orchestrator.extract_token_delta/2`,
//! `extract_rate_limits/1`; see `docs/token-accounting.md`).
//!
//! Extraction prefers **absolute** thread totals (`thread/tokenUsage/updated.params.tokenUsage.total`,
//! `codex/event/token_count ... info.total_token_usage`) and never reads `last`/`last_token_usage`
//! deltas. As in Elixir, a `turn/completed` message's `usage` is a fallback when no absolute total is
//! present; [`TokenAccumulator`]'s high-water marks keep that harmless when it is smaller.
//!
//! Incoming payloads are JSON, so only string keys exist; the Elixir atom-key variants collapse into
//! their string spellings (`:input`/`:output`/`:completion` have no JSON equivalent and are omitted).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Absolute-total paths, in precedence order.
const ABSOLUTE_PATHS: [&[&str]; 4] = [
    &["params", "msg", "payload", "info", "total_token_usage"],
    &["params", "msg", "info", "total_token_usage"],
    &["params", "tokenUsage", "total"],
    &["tokenUsage", "total"],
];

/// Keys that make a map count as a token-usage map (`integer_token_map?/1`).
const TOKEN_FIELDS: [&str; 10] = [
    "input_tokens",
    "output_tokens",
    "total_tokens",
    "prompt_tokens",
    "completion_tokens",
    "inputTokens",
    "outputTokens",
    "totalTokens",
    "promptTokens",
    "completionTokens",
];

const INPUT_FIELDS: [&str; 4] = [
    "input_tokens",
    "prompt_tokens",
    "promptTokens",
    "inputTokens",
];
const OUTPUT_FIELDS: [&str; 4] = [
    "output_tokens",
    "completion_tokens",
    "outputTokens",
    "completionTokens",
];
const TOTAL_FIELDS: [&str; 3] = ["total_tokens", "total", "totalTokens"];

/// Absolute (cumulative) token counters found in one Codex message; a missing field is `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Cumulative input/prompt tokens.
    pub input_tokens: Option<u64>,
    /// Cumulative output/completion tokens.
    pub output_tokens: Option<u64>,
    /// Cumulative total tokens.
    pub total_tokens: Option<u64>,
}

impl TokenUsage {
    /// Reads the counters from a usage map (first integer-like hit per field).
    pub fn from_map(usage: &Map<String, Value>) -> Self {
        Self {
            input_tokens: first_integer(usage, &INPUT_FIELDS),
            output_tokens: first_integer(usage, &OUTPUT_FIELDS),
            total_tokens: first_integer(usage, &TOTAL_FIELDS),
        }
    }
}

/// Elixir `integer_like/1`: a non-negative integer, or a string whose trimmed `Integer.parse` prefix is
/// a non-negative number (`"12"` and `" 12abc"` are 12). Floats and negatives are `None`.
pub fn integer_like(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => parse_integer_prefix(s.trim()),
        _ => None,
    }
}

fn parse_integer_prefix(s: &str) -> Option<u64> {
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let number: u64 = digits[..end].parse().ok()?;
    if negative && number != 0 {
        None
    } else {
        Some(number)
    }
}

fn first_integer(map: &Map<String, Value>, fields: &[&str]) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| map.get(*field).and_then(integer_like))
}

fn is_token_map(map: &Map<String, Value>) -> bool {
    TOKEN_FIELDS
        .iter()
        .any(|field| map.get(*field).and_then(integer_like).is_some())
}

fn at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |acc, key| acc.as_object()?.get(*key))
}

fn token_map(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value
        .and_then(Value::as_object)
        .filter(|map| is_token_map(map))
}

fn absolute_usage(candidate: &Value) -> Option<&Map<String, Value>> {
    ABSOLUTE_PATHS
        .iter()
        .find_map(|path| token_map(at_path(candidate, path)))
}

fn turn_completed_usage(candidate: &Value) -> Option<&Map<String, Value>> {
    let object = candidate.as_object()?;
    if object.get("method").and_then(Value::as_str) != Some("turn/completed") {
        return None;
    }
    // Elixir: `Map.get(p, "usage") || get_in(p, ["params", "usage"])` — a falsy `usage` falls through.
    let direct = match object.get("usage") {
        Some(Value::Null | Value::Bool(false)) | None => at_path(candidate, &["params", "usage"]),
        other => other,
    };
    token_map(direct)
}

/// `extract_token_usage/1` over the candidate payloads (in order; the session passes the message's
/// top-level `usage` then the message itself): first an absolute total from any candidate, else a
/// `turn/completed` usage map, else `None`.
pub fn extract_token_usage<'a>(
    candidates: impl IntoIterator<Item = &'a Value>,
) -> Option<TokenUsage> {
    let candidates: Vec<&Value> = candidates.into_iter().collect();
    candidates
        .iter()
        .find_map(|c| absolute_usage(c))
        .or_else(|| candidates.iter().find_map(|c| turn_completed_usage(c)))
        .map(TokenUsage::from_map)
}

fn truthy(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null | Value::Bool(false)))
}

/// `rate_limits_map?/1`: a non-nil `limit_id`/`limit_name` and any `primary`/`secondary`/`credits` key.
pub fn is_rate_limits_map(value: &Value) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    let named = truthy(map.get("limit_id")) || truthy(map.get("limit_name"));
    named
        && ["primary", "secondary", "credits"]
            .iter()
            .any(|key| map.contains_key(*key))
}

fn rate_limits_from(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(map) => {
            let direct = match map.get("rate_limits") {
                Some(Value::Null | Value::Bool(false)) | None => None,
                other => other,
            };
            if let Some(direct) = direct.filter(|d| is_rate_limits_map(d)) {
                Some(direct)
            } else if is_rate_limits_map(value) {
                Some(value)
            } else {
                map.values().find_map(rate_limits_from)
            }
        }
        Value::Array(items) => items.iter().find_map(rate_limits_from),
        _ => None,
    }
}

/// `extract_rate_limits/1`: the first rate-limit map found depth-first in the candidates (a direct
/// `rate_limits` key wins over the map itself, which wins over nested values).
pub fn extract_rate_limits<'a>(candidates: impl IntoIterator<Item = &'a Value>) -> Option<Value> {
    candidates.into_iter().find_map(rate_limits_from).cloned()
}

/// Token counters (deltas or running totals).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Total tokens.
    pub total_tokens: u64,
}

impl TokenCounts {
    /// Adds `other` field by field (saturating).
    pub fn add(&mut self, other: TokenCounts) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }

    /// `true` when every counter is zero.
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

/// Per-running-entry (= per worker run = per thread) accumulator with high-water marks
/// (`codex_last_reported_*` + `codex_*_tokens` in the Elixir running entry).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenAccumulator {
    /// Highest absolute value reported so far per field (starts at 0).
    pub last_reported: TokenCounts,
    /// Sum of all applied deltas.
    pub totals: TokenCounts,
}

fn field_delta(next: Option<u64>, last: &mut u64) -> u64 {
    match next {
        Some(next) if next >= *last => {
            let delta = next - *last;
            *last = next;
            delta
        }
        // A smaller value (e.g. a per-turn usage after a larger thread total) yields no delta and keeps
        // the high-water mark (`max(old, reported)`).
        _ => 0,
    }
}

impl TokenAccumulator {
    /// Applies one extracted usage and returns the delta to add to global totals. `None` (no usage in the
    /// message) is a zero delta.
    pub fn apply(&mut self, usage: Option<&TokenUsage>) -> TokenCounts {
        let usage = usage.copied().unwrap_or_default();
        let delta = TokenCounts {
            input_tokens: field_delta(usage.input_tokens, &mut self.last_reported.input_tokens),
            output_tokens: field_delta(usage.output_tokens, &mut self.last_reported.output_tokens),
            total_tokens: field_delta(usage.total_tokens, &mut self.last_reported.total_tokens),
        };
        self.totals.add(delta);
        delta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage_of(payload: Value) -> Option<TokenUsage> {
        extract_token_usage([&payload])
    }

    #[test]
    fn integer_like_matches_elixir_integer_parse() {
        assert_eq!(integer_like(&json!(12)), Some(12));
        assert_eq!(integer_like(&json!("12")), Some(12));
        assert_eq!(integer_like(&json!(" 12abc ")), Some(12));
        assert_eq!(integer_like(&json!("+5")), Some(5));
        assert_eq!(integer_like(&json!("-0")), Some(0));
        assert_eq!(integer_like(&json!("-3")), None);
        assert_eq!(integer_like(&json!(-3)), None);
        assert_eq!(integer_like(&json!(1.5)), None);
        assert_eq!(integer_like(&json!("abc")), None);
        assert_eq!(integer_like(&json!("")), None);
        assert_eq!(integer_like(&Value::Null), None);
    }

    #[test]
    fn token_count_cumulative_payloads_accumulate() {
        let first = json!({"method": "codex/event/token_count", "params": {"msg": {"type": "token_count",
            "info": {"total_token_usage": {"input_tokens": "2", "output_tokens": 2, "total_tokens": 4}}}}});
        let second = json!({"method": "codex/event/token_count", "params": {"msg": {"type": "token_count",
            "info": {"total_token_usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}}}});
        let mut acc = TokenAccumulator::default();
        acc.apply(usage_of(first).as_ref());
        acc.apply(usage_of(second).as_ref());
        assert_eq!(
            acc.totals,
            TokenCounts {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15
            }
        );
    }

    #[test]
    fn total_token_usage_wins_over_last_token_usage() {
        let payload = json!({"method": "codex/event/token_count", "params": {"msg": {"type": "event_msg",
            "payload": {"type": "token_count", "info": {
                "last_token_usage": {"input_tokens": 2, "output_tokens": 1, "total_tokens": 3},
                "total_token_usage": {"input_tokens": 200, "output_tokens": 100, "total_tokens": 300}}}}}});
        assert_eq!(
            usage_of(payload),
            Some(TokenUsage {
                input_tokens: Some(200),
                output_tokens: Some(100),
                total_tokens: Some(300)
            })
        );
    }

    #[test]
    fn thread_token_usage_totals_are_monotonic() {
        let mut acc = TokenAccumulator::default();
        for (input, output, total) in [(8, 3, 11), (10, 4, 14)] {
            let payload = json!({"method": "thread/tokenUsage/updated", "params": {"tokenUsage": {
                "total": {"input_tokens": input, "output_tokens": output, "total_tokens": total},
                "last": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}}});
            acc.apply(usage_of(payload).as_ref());
        }
        assert_eq!(
            acc.totals,
            TokenCounts {
                input_tokens: 10,
                output_tokens: 4,
                total_tokens: 14
            }
        );
    }

    #[test]
    fn camel_case_thread_totals_are_read() {
        let payload = json!({"method": "thread/tokenUsage/updated", "params": {"threadId": "t", "turnId": "u",
            "tokenUsage": {"total": {"inputTokens": 7, "cachedInputTokens": 1, "outputTokens": 2,
                "reasoningOutputTokens": 0, "totalTokens": 9}, "modelContextWindow": 1000}}});
        assert_eq!(
            usage_of(payload),
            Some(TokenUsage {
                input_tokens: Some(7),
                output_tokens: Some(2),
                total_tokens: Some(9)
            })
        );
    }

    #[test]
    fn last_token_usage_alone_is_ignored() {
        let payload = json!({"method": "codex/event/token_count", "params": {"msg": {"type": "event_msg",
            "payload": {"type": "token_count", "info": {"last_token_usage":
                {"input_tokens": 8, "output_tokens": 3, "total_tokens": 11}}}}}});
        assert_eq!(usage_of(payload), None);
        let mut acc = TokenAccumulator::default();
        assert!(acc.apply(None).is_zero());
    }

    #[test]
    fn turn_completed_usage_is_a_fallback_and_clamped_by_high_water_marks() {
        let completed = json!({"method": "turn/completed", "usage": {"input_tokens": 5, "output_tokens": 1, "total_tokens": 6}});
        let nested = json!({"method": "turn/completed", "params": {"usage": {"total_tokens": 3}}});
        assert_eq!(usage_of(nested).and_then(|u| u.total_tokens), Some(3));
        let mut acc = TokenAccumulator::default();
        let absolute = json!({"params": {"tokenUsage": {"total": {"input_tokens": 50, "output_tokens": 10, "total_tokens": 60}}}});
        acc.apply(usage_of(absolute).as_ref());
        let delta = acc.apply(usage_of(completed).as_ref());
        assert!(delta.is_zero());
        assert_eq!(acc.totals.total_tokens, 60);
        // A generic `usage` on another method is not a fallback.
        assert_eq!(
            usage_of(json!({"method": "item/completed", "usage": {"total_tokens": 9}})),
            None
        );
    }

    #[test]
    fn top_level_usage_candidate_is_searched_first() {
        let usage = json!({"tokenUsage": {"total": {"total_tokens": 42}}});
        let payload = json!({"params": {"tokenUsage": {"total": {"total_tokens": 7}}}});
        let found = extract_token_usage([&usage, &payload]).and_then(|u| u.total_tokens);
        assert_eq!(found, Some(42));
    }

    #[test]
    fn rate_limits_are_found_depth_first() {
        let limits = json!({"limit_id": "codex", "primary": {"remaining": 90, "limit": 100},
            "secondary": null, "credits": {"has_credits": false, "unlimited": false, "balance": null}});
        let payload = json!({"method": "codex/event/token_count", "params": {"msg": {"type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": limits.clone()}}}});
        assert_eq!(extract_rate_limits([&payload]), Some(limits.clone()));
        let in_list = json!({"items": [{"x": 1}, limits.clone()]});
        assert_eq!(extract_rate_limits([&in_list]), Some(limits));
        // camelCase `limitId` is not recognised (Elixir parity, C.13 #9).
        let camel = json!({"rateLimits": {"limitId": "codex", "primary": {}}});
        assert_eq!(extract_rate_limits([&camel]), None);
        let unnamed = json!({"limit_id": null, "primary": {}});
        assert!(!is_rate_limits_map(&unnamed));
    }
}
