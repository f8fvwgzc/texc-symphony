//! Spend controls: per-issue and per-day token/cost budgets (`budget:` in WORKFLOW.md) and the
//! operator kill switch.
//!
//! * Usage is counted from the token deltas the orchestrator already integrates; a run's cost
//!   uses the prices captured when it was dispatched ([`Pricing`]).
//! * Per-issue usage starts from the issue's retained run history (loaded once per issue, before
//!   its first dispatch in this process), so a restart does not reset an issue's budget. The day
//!   counters start from the runs that began today (UTC) and reset at UTC midnight.
//! * An issue over its budget is stopped and **blocked** (an operator raises the budget or
//!   finishes the work). An exhausted day stops every running agent and holds dispatch until the
//!   next UTC day; the kill switch does the same until it is released. Stopped agents release
//!   their claims, so they are dispatched again normally once spending is allowed.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use symphony_core::config::BudgetSettings;

/// Store `control` key holding the kill switch reason (absent = dispatching).
pub const PAUSED_CONTROL_KEY: &str = "dispatch_paused";

/// Token prices of one run, fixed at dispatch.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Pricing {
    pub input_usd_per_million_tokens: f64,
    pub output_usd_per_million_tokens: f64,
}

impl Pricing {
    pub(crate) fn from_budget(budget: &BudgetSettings) -> Self {
        Self {
            input_usd_per_million_tokens: budget.input_usd_per_million_tokens,
            output_usd_per_million_tokens: budget.output_usd_per_million_tokens,
        }
    }

    /// Cost in micro-USD of cumulative `input` / `output` token counts.
    pub(crate) fn cost_micro_usd(&self, input: u64, output: u64) -> u64 {
        BudgetSettings {
            input_usd_per_million_tokens: self.input_usd_per_million_tokens,
            output_usd_per_million_tokens: self.output_usd_per_million_tokens,
            ..BudgetSettings::default()
        }
        .cost_micro_usd(input, output)
    }
}

/// Tokens and micro-USD.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub tokens: u64,
    pub cost_micro_usd: u64,
}

impl Usage {
    fn add(&mut self, tokens: u64, cost_micro_usd: u64) {
        self.tokens = self.tokens.saturating_add(tokens);
        self.cost_micro_usd = self.cost_micro_usd.saturating_add(cost_micro_usd);
    }
}

/// Why spending (and therefore dispatching) is not allowed right now.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SpendStop {
    /// The operator engaged the kill switch.
    Paused(String),
    /// Today's token or cost budget is used up.
    DayExhausted(String),
    /// This issue's budget is used up.
    IssueExhausted(String),
}

impl SpendStop {
    /// The reason text shown in the API and logs.
    pub(crate) fn reason(&self) -> &str {
        match self {
            Self::Paused(reason) | Self::DayExhausted(reason) | Self::IssueExhausted(reason) => {
                reason
            }
        }
    }
}

fn usd(micro: u64) -> f64 {
    micro as f64 / 1_000_000.0
}

fn micro(usd: f64) -> u64 {
    (usd * 1_000_000.0).round().clamp(0.0, u64::MAX as f64) as u64
}

/// Orchestrator-owned spend counters.
#[derive(Debug, Default)]
pub(crate) struct Spend {
    /// Kill switch reason; `Some` while dispatch is paused.
    pub paused: Option<String>,
    day: Option<NaiveDate>,
    today: Usage,
    issues: HashMap<String, Usage>,
    /// Issues whose stored history has been merged into `issues`.
    seeded: HashSet<String>,
}

/// Prefixes of the block errors an exhausted issue budget leaves (see [`Spend::issue_stop`]).
const ISSUE_BUDGET_ERRORS: [&str; 2] = [
    "issue token budget exhausted",
    "issue cost budget exhausted",
];

/// `true` when a blocked issue's `error` says its budget ran out (it is released once the
/// budget allows it again).
pub(crate) fn is_issue_budget_error(error: &str) -> bool {
    ISSUE_BUDGET_ERRORS
        .iter()
        .any(|prefix| error.starts_with(prefix))
}

impl Spend {
    /// Starts the day counters at `usage` (the runs that began on `now`'s UTC day).
    pub(crate) fn seed_day(&mut self, now: DateTime<Utc>, usage: Usage) {
        self.day = Some(now.date_naive());
        self.today = usage;
    }

    /// `true` once the issue's stored history has been merged in.
    pub(crate) fn knows_issue(&self, issue_id: &str) -> bool {
        self.seeded.contains(issue_id)
    }

    /// Merges an issue's stored history into its counters. The store also holds the runs this
    /// process recorded, so the larger of the two counts wins instead of adding them.
    pub(crate) fn seed_issue(&mut self, issue_id: &str, usage: Usage) {
        if !self.seeded.insert(issue_id.to_owned()) {
            return;
        }
        let counters = self.issues.entry(issue_id.to_owned()).or_default();
        counters.tokens = counters.tokens.max(usage.tokens);
        counters.cost_micro_usd = counters.cost_micro_usd.max(usage.cost_micro_usd);
    }

    /// Forgets an issue's counters (it left the active set; history is reloaded if it returns).
    pub(crate) fn forget_issue(&mut self, issue_id: &str) {
        self.issues.remove(issue_id);
        self.seeded.remove(issue_id);
    }

    fn roll(&mut self, now: DateTime<Utc>) {
        let day = now.date_naive();
        if self.day != Some(day) {
            self.day = Some(day);
            self.today = Usage::default();
        }
    }

    /// Adds a run's usage delta to its issue and to today.
    pub(crate) fn record(
        &mut self,
        issue_id: &str,
        tokens: u64,
        cost_micro_usd: u64,
        now: DateTime<Utc>,
    ) {
        self.roll(now);
        self.today.add(tokens, cost_micro_usd);
        self.issues
            .entry(issue_id.to_owned())
            .or_default()
            .add(tokens, cost_micro_usd);
    }

    /// Today's usage (after rolling over to `now`'s day).
    pub(crate) fn today(&mut self, now: DateTime<Utc>) -> Usage {
        self.roll(now);
        self.today
    }

    /// An issue's known usage.
    pub(crate) fn issue(&self, issue_id: &str) -> Usage {
        self.issues.get(issue_id).copied().unwrap_or_default()
    }

    /// The global stop (kill switch or exhausted day), if any.
    pub(crate) fn global_stop(
        &mut self,
        budget: &BudgetSettings,
        now: DateTime<Utc>,
    ) -> Option<SpendStop> {
        if let Some(reason) = &self.paused {
            return Some(SpendStop::Paused(format!("dispatch paused: {reason}")));
        }
        let today = self.today(now);
        if let Some(limit) = budget.max_tokens_per_day
            && today.tokens >= limit
        {
            return Some(SpendStop::DayExhausted(format!(
                "daily token budget exhausted: {} of {limit} tokens used today (UTC)",
                today.tokens
            )));
        }
        if let Some(limit) = budget.max_cost_usd_per_day
            && today.cost_micro_usd >= micro(limit)
        {
            return Some(SpendStop::DayExhausted(format!(
                "daily cost budget exhausted: ${:.2} of ${limit:.2} spent today (UTC)",
                usd(today.cost_micro_usd)
            )));
        }
        None
    }

    /// The per-issue stop, if `issue_id` used up its budget.
    pub(crate) fn issue_stop(&self, issue_id: &str, budget: &BudgetSettings) -> Option<SpendStop> {
        let used = self.issue(issue_id);
        if let Some(limit) = budget.max_tokens_per_issue
            && used.tokens >= limit
        {
            return Some(SpendStop::IssueExhausted(format!(
                "issue token budget exhausted: {} of {limit} tokens used",
                used.tokens
            )));
        }
        if let Some(limit) = budget.max_cost_usd_per_issue
            && used.cost_micro_usd >= micro(limit)
        {
            return Some(SpendStop::IssueExhausted(format!(
                "issue cost budget exhausted: ${:.2} of ${limit:.2} spent",
                usd(used.cost_micro_usd)
            )));
        }
        None
    }

    /// Whatever stops `issue_id` from running now (global first).
    pub(crate) fn stop_for(
        &mut self,
        issue_id: &str,
        budget: &BudgetSettings,
        now: DateTime<Utc>,
    ) -> Option<SpendStop> {
        self.global_stop(budget, now)
            .or_else(|| self.issue_stop(issue_id, budget))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn budget() -> BudgetSettings {
        BudgetSettings {
            max_tokens_per_issue: Some(1_000),
            max_tokens_per_day: Some(5_000),
            max_cost_usd_per_issue: Some(1.0),
            max_cost_usd_per_day: Some(3.0),
            input_usd_per_million_tokens: 1_000.0,
            output_usd_per_million_tokens: 2_000.0,
        }
    }

    #[test]
    fn pricing_turns_tokens_into_micro_usd() {
        let pricing = Pricing::from_budget(&budget());
        // 100 input * $1000/M = $0.10, 50 output * $2000/M = $0.10.
        assert_eq!(pricing.cost_micro_usd(100, 50), 200_000);
        assert_eq!(Pricing::default().cost_micro_usd(1_000, 1_000), 0);
    }

    #[test]
    fn issue_budgets_count_history_and_new_usage() {
        let now = Utc::now();
        let mut spend = Spend::default();
        spend.seed_issue(
            "i-1",
            Usage {
                tokens: 900,
                cost_micro_usd: 0,
            },
        );
        assert!(spend.knows_issue("i-1"));
        assert_eq!(spend.stop_for("i-1", &budget(), now), None);
        spend.record("i-1", 100, 10, now);
        let stop = spend.stop_for("i-1", &budget(), now).unwrap();
        assert!(matches!(stop, SpendStop::IssueExhausted(_)), "{stop:?}");
        assert_eq!(
            stop.reason(),
            "issue token budget exhausted: 1000 of 1000 tokens used"
        );
        assert_eq!(spend.stop_for("i-2", &budget(), now), None);
        spend.record("i-2", 1, 1_000_000, now);
        assert_eq!(
            spend
                .issue_stop("i-2", &budget())
                .map(|s| s.reason().to_owned()),
            Some("issue cost budget exhausted: $1.00 of $1.00 spent".to_owned())
        );
        spend.forget_issue("i-1");
        assert!(!spend.knows_issue("i-1"));
    }

    #[test]
    fn history_is_merged_even_after_usage_was_counted() {
        let now = Utc::now();
        let mut spend = Spend::default();
        // Usage counted before any per-issue budget existed (no history loaded yet).
        spend.record("i-1", 10, 0, now);
        assert!(!spend.knows_issue("i-1"));
        // The store holds earlier runs plus this one.
        spend.seed_issue(
            "i-1",
            Usage {
                tokens: 950,
                cost_micro_usd: 7,
            },
        );
        assert!(spend.knows_issue("i-1"));
        assert_eq!(
            spend.issue("i-1"),
            Usage {
                tokens: 950,
                cost_micro_usd: 7
            }
        );
        spend.seed_issue(
            "i-1",
            Usage {
                tokens: 5,
                cost_micro_usd: 0,
            },
        );
        assert_eq!(spend.issue("i-1").tokens, 950, "seeded once");
        assert!(is_issue_budget_error(
            "issue token budget exhausted: 1 of 1 tokens used"
        ));
        assert!(!is_issue_budget_error("codex turn requires approval"));
    }

    #[test]
    fn the_day_budget_resets_at_utc_midnight() {
        let monday = Utc.with_ymd_and_hms(2026, 10, 26, 23, 0, 0).unwrap();
        let tuesday = Utc.with_ymd_and_hms(2026, 10, 27, 0, 0, 1).unwrap();
        let mut spend = Spend::default();
        spend.seed_day(
            monday,
            Usage {
                tokens: 4_000,
                cost_micro_usd: 0,
            },
        );
        spend.record("a", 1_000, 0, monday);
        let stop = spend.global_stop(&budget(), monday).unwrap();
        assert!(matches!(stop, SpendStop::DayExhausted(_)));
        assert_eq!(spend.global_stop(&budget(), tuesday), None);
        assert_eq!(spend.today(tuesday), Usage::default());
        spend.record("b", 0, 3_000_000, tuesday);
        assert_eq!(
            spend
                .global_stop(&budget(), tuesday)
                .map(|s| s.reason().to_owned()),
            Some("daily cost budget exhausted: $3.00 of $3.00 spent today (UTC)".to_owned())
        );
    }

    #[test]
    fn the_kill_switch_wins_and_no_budget_means_no_limit() {
        let now = Utc::now();
        let mut spend = Spend {
            paused: Some("incident".into()),
            ..Spend::default()
        };
        assert_eq!(
            spend.stop_for("x", &BudgetSettings::default(), now),
            Some(SpendStop::Paused("dispatch paused: incident".into()))
        );
        spend.paused = None;
        spend.record("x", u64::MAX, u64::MAX, now);
        assert_eq!(spend.stop_for("x", &BudgetSettings::default(), now), None);
    }
}
