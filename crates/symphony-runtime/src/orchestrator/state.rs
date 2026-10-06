//! Orchestrator state (`Orchestrator.State`, B.2.2) and the pure scheduling rules: candidate
//! selection, dispatch order, slot accounting, host selection, retry delays and blocker detection.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use symphony_codex::{CodexEvent, CodexEventData, CodexEventKind, TokenAccumulator, TokenCounts};
use symphony_core::config::AgentSettings;
use symphony_core::issue::{normalize_state, present_string};
use symphony_core::{Issue, Settings};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::recorder::RunRecorder;
use crate::snapshot::{CodexMessage, CodexTotals};
use crate::workspace::worker_hosts;

/// Base of the exponential failure backoff.
pub const FAILURE_RETRY_BASE_MS: u64 = 10_000;
/// Gap between the tick (`checking now…` rendered) and the poll cycle.
pub const POLL_TRANSITION_RENDER_DELAY: Duration = Duration::from_millis(20);

/// Error text of a blocked MCP elicitation.
pub const MCP_ELICITATION_BLOCKER: &str = "codex MCP elicitation requires operator input";
/// Method of an MCP elicitation request.
pub const MCP_ELICITATION_METHOD: &str = "mcpServer/elicitation/request";

/// Handle on a spawned worker task.
#[derive(Debug)]
pub(crate) struct WorkerHandle {
    pub cancel: CancellationToken,
    pub abort: AbortHandle,
    /// Resolves (with an error) once the worker future has been dropped (finished or aborted).
    pub done: oneshot::Receiver<()>,
}

/// One running agent (`RunningEntry`).
#[derive(Debug)]
pub(crate) struct RunningEntry {
    pub run_id: u64,
    pub worker: Option<WorkerHandle>,
    pub identifier: String,
    pub issue: Issue,
    pub worker_host: Option<String>,
    pub workspace_path: Option<String>,
    pub session_id: Option<String>,
    pub last_codex_message: Option<CodexMessage>,
    pub last_codex_timestamp: Option<DateTime<Utc>>,
    pub last_codex_event: Option<CodexEventKind>,
    /// Monotonic time of the last Codex activity (dispatch time initially); drives stall detection.
    pub last_activity: Instant,
    pub codex_app_server_pid: Option<String>,
    pub tokens: TokenAccumulator,
    pub turn_count: u32,
    /// `normalize_retry_attempt(attempt)`: 0 for a first dispatch.
    pub retry_attempt: u32,
    pub started_at: DateTime<Utc>,
    pub started: Instant,
    pub recorder: RunRecorder,
}

/// One blocked issue (`BlockedEntry`).
#[derive(Debug, Clone)]
pub(crate) struct BlockedEntry {
    pub identifier: String,
    pub issue: Option<Issue>,
    pub worker_host: Option<String>,
    pub workspace_path: Option<String>,
    pub session_id: Option<String>,
    pub error: String,
    pub blocked_at: DateTime<Utc>,
    pub last_codex_message: Option<CodexMessage>,
    pub last_codex_event: Option<CodexEventKind>,
    pub last_codex_timestamp: Option<DateTime<Utc>>,
}

/// One queued retry (`RetryEntry`).
#[derive(Debug)]
pub(crate) struct RetryEntry {
    pub attempt: u32,
    pub token: u64,
    pub timer: Option<AbortHandle>,
    pub due_at: Instant,
    pub identifier: String,
    pub issue_url: Option<String>,
    pub error: Option<String>,
    pub worker_host: Option<String>,
    pub workspace_path: Option<String>,
}

/// Metadata merged into a retry entry (`schedule_issue_retry/4` metadata map).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RetryMeta {
    pub identifier: Option<String>,
    pub issue_url: Option<String>,
    pub error: Option<String>,
    pub worker_host: Option<String>,
    pub workspace_path: Option<String>,
}

impl RetryEntry {
    pub(crate) fn meta(&self) -> RetryMeta {
        RetryMeta {
            identifier: Some(self.identifier.clone()),
            issue_url: self.issue_url.clone(),
            error: self.error.clone(),
            worker_host: self.worker_host.clone(),
            workspace_path: self.workspace_path.clone(),
        }
    }
}

/// Retry delay class (`delay_type`, never stored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayType {
    /// Re-check after a normal exit: 1 s when the attempt is 1.
    Continuation,
    /// Exponential failure backoff.
    Failure,
}

/// `retry_delay/2`: continuation attempt 1 → `agent.continuation_delay_ms` (1 s by default);
/// otherwise `min(10_000 · 2^min(attempt − 1, 10), max_retry_backoff_ms)`. No jitter, no maximum
/// count.
pub fn retry_delay(attempt: u32, delay_type: DelayType, agent: &AgentSettings) -> Duration {
    if delay_type == DelayType::Continuation && attempt == 1 {
        return Duration::from_millis(agent.continuation_delay_ms);
    }
    let power = attempt.saturating_sub(1).min(10);
    let delay = FAILURE_RETRY_BASE_MS.saturating_mul(1 << power);
    Duration::from_millis(delay.min(agent.max_retry_backoff_ms))
}

/// Normalized active/terminal state sets (blank entries dropped).
#[derive(Debug, Clone, Default)]
pub(crate) struct StateSets {
    pub active: HashSet<String>,
    pub terminal: HashSet<String>,
}

impl StateSets {
    pub(crate) fn from_settings(settings: &Settings) -> Self {
        let set = |states: &[String]| {
            states
                .iter()
                .map(|s| normalize_state(s))
                .filter(|s| !s.is_empty())
                .collect()
        };
        Self {
            active: set(settings.active_states()),
            terminal: set(settings.terminal_states()),
        }
    }

    pub(crate) fn is_active(&self, state: Option<&str>) -> bool {
        state.is_some_and(|s| self.active.contains(&normalize_state(s)))
    }

    pub(crate) fn is_terminal(&self, state: Option<&str>) -> bool {
        state.is_some_and(|s| self.terminal.contains(&normalize_state(s)))
    }
}

/// `candidate_issue?/3`: non-blank id/identifier/title/state, routable, active and not terminal.
pub(crate) fn candidate_issue(issue: &Issue, sets: &StateSets, settings: &Settings) -> bool {
    [&issue.id, &issue.identifier, &issue.title, &issue.state]
        .iter()
        .all(|field| present_string(field.as_deref()))
        && issue.routable(&settings.tracker.required_labels)
        && sets.is_active(issue.state.as_deref())
        && !sets.is_terminal(issue.state.as_deref())
}

fn priority_rank(priority: Option<i64>) -> i64 {
    priority.filter(|p| (1..=4).contains(p)).unwrap_or(5)
}

/// `sort_issues_for_dispatch/1`: priority 1..4 first (others rank 5), then oldest `created_at`
/// (missing last), then identifier (else id) bytewise. Stable; no dedupe.
pub fn sort_issues_for_dispatch(mut issues: Vec<Issue>) -> Vec<Issue> {
    issues.sort_by(|a, b| {
        let key = |i: &Issue| {
            (
                priority_rank(i.priority),
                i.created_at.map_or(i64::MAX, |dt| dt.timestamp_micros()),
            )
        };
        let tiebreak = |i: &Issue| {
            i.identifier
                .clone()
                .or_else(|| i.id.clone())
                .unwrap_or_default()
        };
        key(a)
            .cmp(&key(b))
            .then_with(|| tiebreak(a).as_bytes().cmp(tiebreak(b).as_bytes()))
    });
    issues
}

/// Result of [`State::select_worker_host`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostChoice {
    /// No SSH hosts configured: run locally.
    Local,
    /// Run on this SSH host.
    Host(String),
    /// Every host is at `max_concurrent_agents_per_host` (wait; never fall back to local).
    NoCapacity,
}

impl HostChoice {
    pub(crate) fn host(&self) -> Option<String> {
        match self {
            Self::Host(host) => Some(host.clone()),
            _ => None,
        }
    }
}

/// Monotonic token source for timers and run ids.
#[derive(Debug, Default)]
pub(crate) struct Counter(u64);

impl Counter {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

/// Orchestrator scheduling state (single owner: the orchestrator task).
#[derive(Debug, Default)]
pub(crate) struct State {
    pub poll_interval_ms: u64,
    pub max_concurrent_agents: u32,
    pub next_poll_due: Option<Instant>,
    pub poll_check_in_progress: bool,
    pub tick_token: Option<u64>,
    pub tick_timer: Option<AbortHandle>,
    pub running: HashMap<String, RunningEntry>,
    /// Bookkeeping only (never read for gating), like Elixir.
    pub completed: HashSet<String>,
    pub claimed: HashSet<String>,
    pub blocked: HashMap<String, BlockedEntry>,
    pub retry_attempts: HashMap<String, RetryEntry>,
    pub codex_totals: CodexTotals,
    pub codex_rate_limits: Option<Value>,
}

impl State {
    pub(crate) fn new(settings: &Settings) -> Self {
        Self {
            poll_interval_ms: settings.polling.interval_ms,
            max_concurrent_agents: settings.agent.max_concurrent_agents,
            ..Self::default()
        }
    }

    /// `refresh_runtime_config/1`.
    pub(crate) fn refresh_runtime_config(&mut self, settings: &Settings) {
        self.poll_interval_ms = settings.polling.interval_ms;
        self.max_concurrent_agents = settings.agent.max_concurrent_agents;
    }

    /// `available_slots/1`: global capacity left (only running entries hold slots).
    pub(crate) fn available_slots(&self) -> usize {
        usize::try_from(self.max_concurrent_agents)
            .unwrap_or(usize::MAX)
            .saturating_sub(self.running.len())
    }

    /// `state_slots_available?/2`: per-state cap (else the global cap) versus running entries whose last
    /// refreshed state normalizes to the same value.
    pub(crate) fn state_slots_available(&self, issue: &Issue, settings: &Settings) -> bool {
        let Some(state) = issue.state.as_deref() else {
            return false;
        };
        let limit = settings.max_concurrent_agents_for_state(Some(state));
        let wanted = normalize_state(state);
        let used = self
            .running
            .values()
            .filter(|entry| {
                entry
                    .issue
                    .state
                    .as_deref()
                    .is_some_and(|s| normalize_state(s) == wanted)
            })
            .count();
        usize::try_from(limit).unwrap_or(usize::MAX) > used
    }

    fn running_count_for_host(&self, host: &str) -> usize {
        self.running
            .values()
            .filter(|entry| entry.worker_host.as_deref() == Some(host))
            .count()
    }

    /// `select_worker_host/2`: local when no hosts; otherwise the preferred host if it has capacity,
    /// else the least-loaded host with capacity (ties: configuration order).
    pub(crate) fn select_worker_host(
        &self,
        settings: &Settings,
        preferred: Option<&str>,
    ) -> HostChoice {
        let hosts = worker_hosts(settings);
        if hosts.is_empty() {
            return HostChoice::Local;
        }
        let cap = settings
            .worker
            .max_concurrent_agents_per_host
            .filter(|cap| *cap > 0)
            .and_then(|cap| usize::try_from(cap).ok());
        let available: Vec<(usize, &String)> = hosts
            .iter()
            .map(|host| (self.running_count_for_host(host), host))
            .filter(|(count, _)| cap.is_none_or(|cap| *count < cap))
            .collect();
        if available.is_empty() {
            return HostChoice::NoCapacity;
        }
        if let Some(preferred) = preferred.filter(|p| !p.is_empty())
            && available.iter().any(|(_, host)| host.as_str() == preferred)
        {
            return HostChoice::Host(preferred.to_owned());
        }
        available
            .iter()
            .enumerate()
            .min_by_key(|(index, (count, _))| (*count, *index))
            .map_or(HostChoice::NoCapacity, |(_, (_, host))| {
                HostChoice::Host((*host).clone())
            })
    }

    /// `should_dispatch_issue?/4`.
    pub(crate) fn should_dispatch(
        &self,
        issue: &Issue,
        settings: &Settings,
        sets: &StateSets,
    ) -> bool {
        let Some(id) = issue.id.as_deref() else {
            return false;
        };
        candidate_issue(issue, sets, settings)
            && !self.claimed.contains(id)
            && !self.running.contains_key(id)
            && !self.blocked.contains_key(id)
            && self.available_slots() > 0
            && self.state_slots_available(issue, settings)
            && self.select_worker_host(settings, None) != HostChoice::NoCapacity
    }
}

/// `next_retry_attempt_from_running/1`: `retry_attempt + 1` for a retried run, else `None` (which
/// makes the retry attempt 1).
pub(crate) fn next_retry_attempt_from_running(entry: &RunningEntry) -> Option<u32> {
    (entry.retry_attempt > 0).then(|| entry.retry_attempt + 1)
}

fn message_is_mcp_elicitation(message: Option<&CodexMessage>) -> bool {
    message.and_then(CodexMessage::method) == Some(MCP_ELICITATION_METHOD)
}

/// `input_required_blocker?/1` on the entry's last Codex signal.
pub(crate) fn entry_is_blocker(
    last_event: Option<CodexEventKind>,
    last_message: Option<&CodexMessage>,
) -> bool {
    last_event.and_then(CodexEventKind::blocker).is_some()
        || message_is_mcp_elicitation(last_message)
}

/// `blocker_error/2`.
pub(crate) fn blocker_error(
    last_event: Option<CodexEventKind>,
    last_message: Option<&CodexMessage>,
    fallback: &str,
) -> String {
    if let Some(blocker) = last_event.and_then(CodexEventKind::blocker) {
        return blocker.message().to_owned();
    }
    if message_is_mcp_elicitation(last_message) {
        return MCP_ELICITATION_BLOCKER.to_owned();
    }
    fallback.to_owned()
}

/// Details of session-level events (which carry no stream message) kept as `last_codex_message`, so
/// they humanize as `session started (<id>)` / `turn ended with error: <reason>` (Elixir stored `nil`
/// and rendered `... error: nil`).
fn session_details(event: &CodexEvent) -> Option<Value> {
    match &event.data {
        CodexEventData::SessionStarted {
            session_id,
            thread_id,
            turn_id,
        } => Some(serde_json::json!({
            "session_id": session_id, "thread_id": thread_id, "turn_id": turn_id,
        })),
        CodexEventData::StartupFailed { reason } => {
            Some(serde_json::json!({"reason": reason.to_string()}))
        }
        CodexEventData::TurnEndedWithError { session_id, reason } => Some(serde_json::json!({
            "session_id": session_id, "reason": reason.to_string(),
        })),
        _ => None,
    }
}

impl RunningEntry {
    /// `integrate_codex_update/2`: merges one event; returns the token delta and whether a new turn
    /// started.
    pub(crate) fn integrate(&mut self, event: &CodexEvent, now: Instant) -> (TokenCounts, bool) {
        self.last_codex_timestamp = Some(event.timestamp);
        self.last_codex_message = Some(CodexMessage {
            event: event.kind(),
            message: event.message_value().or_else(|| session_details(event)),
            timestamp: event.timestamp,
        });
        let mut new_turn = false;
        if let Some(session_id) = event.session_id() {
            if event.kind() == CodexEventKind::SessionStarted
                && self.session_id.as_deref() != Some(session_id)
            {
                self.turn_count += 1;
                new_turn = true;
            }
            self.session_id = Some(session_id.to_owned());
        }
        self.last_codex_event = Some(event.kind());
        if let Some(pid) = &event.codex_app_server_pid {
            self.codex_app_server_pid = Some(pid.clone());
        }
        self.last_activity = now;
        let delta = self.tokens.apply(event.token_usage.as_ref());
        (delta, new_turn)
    }

    pub(crate) fn is_blocker(&self) -> bool {
        entry_is_blocker(self.last_codex_event, self.last_codex_message.as_ref())
    }
}

impl CodexTotals {
    pub(crate) fn add_tokens(&mut self, delta: TokenCounts) {
        self.input_tokens = self.input_tokens.saturating_add(delta.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(delta.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(delta.total_tokens);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn issue(identifier: &str, priority: Option<i64>, created: Option<(i32, u32, u32)>) -> Issue {
        Issue {
            id: Some(format!("id-{identifier}")),
            identifier: Some(identifier.into()),
            priority,
            created_at: created.map(|(y, m, d)| Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()),
            ..Issue::default()
        }
    }

    #[test]
    fn sorts_dispatch_candidates_by_priority_then_oldest_created_at() {
        let sorted = sort_issues_for_dispatch(vec![
            issue("MT-200", Some(2), Some((2025, 12, 1))),
            issue("MT-201", Some(1), Some((2026, 1, 2))),
            issue("MT-199", Some(1), Some((2026, 1, 1))),
        ]);
        let ids: Vec<_> = sorted
            .iter()
            .map(|i| i.identifier.clone().unwrap())
            .collect();
        assert_eq!(ids, ["MT-199", "MT-201", "MT-200"]);
    }

    #[test]
    fn non_standard_priorities_and_missing_dates_sort_last() {
        let sorted = sort_issues_for_dispatch(vec![
            issue("B", Some(0), None),
            issue("A", None, None),
            issue("C", Some(9), Some((2020, 1, 1))),
            issue("D", Some(4), None),
        ]);
        let ids: Vec<_> = sorted
            .iter()
            .map(|i| i.identifier.clone().unwrap())
            .collect();
        assert_eq!(ids, ["D", "C", "A", "B"]);
    }

    #[test]
    fn retry_delays_follow_the_elixir_formula() {
        let agent = AgentSettings::default();
        let ms = |a, t| retry_delay(a, t, &agent).as_millis();
        assert_eq!(ms(1, DelayType::Continuation), 1_000);
        assert_eq!(ms(2, DelayType::Continuation), 20_000);
        assert_eq!(ms(1, DelayType::Failure), 10_000);
        assert_eq!(ms(2, DelayType::Failure), 20_000);
        assert_eq!(ms(3, DelayType::Failure), 40_000);
        assert_eq!(ms(5, DelayType::Failure), 160_000);
        assert_eq!(ms(6, DelayType::Failure), 300_000);
        assert_eq!(ms(500, DelayType::Failure), 300_000);
        let uncapped = AgentSettings {
            max_retry_backoff_ms: u64::MAX,
            continuation_delay_ms: 15_000,
            ..AgentSettings::default()
        };
        assert_eq!(
            retry_delay(40, DelayType::Failure, &uncapped).as_millis(),
            10_240_000
        );
        assert_eq!(
            retry_delay(1, DelayType::Continuation, &uncapped).as_millis(),
            15_000
        );
    }

    #[test]
    fn blocker_errors_prefer_event_then_mcp_then_fallback() {
        let mcp = CodexMessage {
            event: CodexEventKind::Notification,
            message: Some(serde_json::json!({"method": MCP_ELICITATION_METHOD})),
            timestamp: Utc::now(),
        };
        assert!(entry_is_blocker(None, Some(&mcp)));
        assert_eq!(
            blocker_error(None, Some(&mcp), "x"),
            MCP_ELICITATION_BLOCKER
        );
        assert_eq!(
            blocker_error(Some(CodexEventKind::TurnInputRequired), Some(&mcp), "x"),
            "codex turn requires operator input"
        );
        assert_eq!(
            blocker_error(Some(CodexEventKind::ApprovalRequired), None, "x"),
            "codex turn requires approval"
        );
        assert!(!entry_is_blocker(Some(CodexEventKind::Notification), None));
        assert_eq!(blocker_error(None, None, "fallback"), "fallback");
    }
}
