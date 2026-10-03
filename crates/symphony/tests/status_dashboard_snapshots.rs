//! Golden frames of the terminal status dashboard (Elixir `status_dashboard_snapshot_test.exs` and
//! the frame-level tests of `orchestrator_status_test.exs`).
//!
//! Fixtures are the Elixir ones, verbatim: `<name>.snapshot.txt` is the raw frame with every ESC
//! byte written as the two characters `\e`; `<name>.evidence.md` is the ANSI-stripped frame in a
//! `text` fence. `UPDATE_SNAPSHOTS=1` rewrites them.

use std::path::PathBuf;

use serde_json::{Value, json};
use symphony_cli::dashboard::format::{
    FrameContext, FrameData, Polling, RetryRow, RunningRow, Totals, format_running_summary,
    format_snapshot_content, running_event_width,
};
use symphony_codex::CodexEventKind;
use symphony_runtime::humanize::humanize_event_message;

const TERMINAL_COLUMNS: usize = 115;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/status_dashboard_snapshots")
        .join(name)
}

fn normalize(content: &str) -> String {
    format!("{}\n", content.replace("\r\n", "\n").trim_end_matches('\n'))
}

fn strip_ansi(content: &str) -> String {
    let mut out = String::new();
    let mut rest = content;
    while let Some(start) = rest.find("\u{1b}[") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail
            .find(|c: char| !(c.is_ascii_digit() || c == ';'))
            .unwrap_or(tail.len());
        if tail[end..].starts_with('m') {
            rest = &tail[end + 1..];
        } else {
            out.push_str("\u{1b}[");
            rest = tail;
        }
    }
    out.push_str(rest);
    out
}

fn update_snapshots() -> bool {
    std::env::var("UPDATE_SNAPSHOTS")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn assert_snapshot(relative: &str, content: &str) {
    let path = fixture_path(relative);
    let normalized = normalize(content);
    if update_snapshots() {
        std::fs::write(&path, &normalized).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "Missing snapshot fixture `{relative}`. Run `UPDATE_SNAPSHOTS=1 cargo test -p symphony --test status_dashboard_snapshots` to create or update fixtures."
        )
    });
    assert_eq!(
        normalized, expected,
        "Snapshot mismatch for `{relative}`. Run `UPDATE_SNAPSHOTS=1 cargo test -p symphony --test status_dashboard_snapshots` to create or update fixtures."
    );
}

fn assert_dashboard_snapshot(name: &str, raw: &str) {
    assert_snapshot(
        &format!("{name}.snapshot.txt"),
        &raw.replace('\u{1b}', "\\e"),
    );
    let plain = normalize(&strip_ansi(raw));
    assert_snapshot(
        &format!("{name}.evidence.md"),
        &format!("```text\n{}\n```\n", plain.trim_end_matches('\n')),
    );
}

fn ctx(dashboard_url: Option<&str>) -> FrameContext {
    FrameContext {
        max_concurrent_agents: 10,
        tracker_kind: Some("linear".into()),
        project_slug: Some("project".into()),
        dashboard_url: dashboard_url.map(str::to_owned),
        columns: TERMINAL_COLUMNS,
    }
}

fn notification(message: Value) -> String {
    humanize_event_message(Some(CodexEventKind::Notification), Some(&message))
}

fn turn_started() -> String {
    notification(json!({"method": "turn/started", "params": {"turn": {"id": "turn-1"}}}))
}

fn turn_completed(status: &str) -> String {
    notification(json!({"method": "turn/completed", "params": {"turn": {"status": status}}}))
}

fn running(identifier: &str) -> RunningRow {
    RunningRow {
        identifier: Some(identifier.into()),
        state: Some("running".into()),
        session_id: Some("thread-1234567890".into()),
        codex_app_server_pid: Some("4242".into()),
        codex_total_tokens: 0,
        runtime_seconds: 0,
        turn_count: 1,
        last_codex_event: Some("notification".into()),
        last_message: turn_started(),
    }
}

fn retry(identifier: &str, attempt: u32, due_in_ms: u64, error: &str) -> RetryRow {
    RetryRow {
        issue_id: Some("issue-1".into()),
        identifier: Some(identifier.into()),
        attempt,
        due_in_ms,
        error: Some(error.into()),
    }
}

fn idle() -> FrameData {
    FrameData::default()
}

fn render(data: &FrameData, tps: f64) -> String {
    format_snapshot_content(Some(data), tps, &ctx(None))
}

#[test]
fn snapshot_fixture_idle_dashboard() {
    assert_dashboard_snapshot("idle", &render(&idle(), 0.0));
}

#[test]
fn snapshot_fixture_idle_dashboard_with_observability_url() {
    let frame = format_snapshot_content(Some(&idle()), 0.0, &ctx(Some("http://127.0.0.1:4000/")));
    assert_dashboard_snapshot("idle_with_dashboard_url", &frame);
}

#[test]
fn snapshot_fixture_super_busy_dashboard() {
    let data = FrameData {
        running: vec![
            RunningRow {
                codex_total_tokens: 120_450,
                runtime_seconds: 785,
                turn_count: 11,
                last_codex_event: Some("turn_completed".into()),
                last_message: turn_completed("completed"),
                ..running("MT-101")
            },
            RunningRow {
                session_id: Some("thread-abcdef1234567890".into()),
                codex_app_server_pid: Some("5252".into()),
                codex_total_tokens: 89_200,
                runtime_seconds: 412,
                turn_count: 4,
                last_codex_event: Some("codex/event/task_started".into()),
                last_message: notification(json!({
                    "method": "codex/event/exec_command_begin",
                    "params": {"msg": {"command": "mix test --cover"}}
                })),
                ..running("MT-102")
            },
        ],
        retrying: vec![],
        totals: Totals {
            input_tokens: 250_000,
            output_tokens: 18_500,
            total_tokens: 268_500,
            seconds_running: 4_321,
        },
        rate_limits: Some(json!({
            "limit_id": "gpt-5",
            "primary": {"remaining": 12_345, "limit": 20_000, "reset_in_seconds": 30},
            "secondary": {"remaining": 45, "limit": 60, "reset_in_seconds": 12},
            "credits": {"has_credits": true, "balance": 9_876.5}
        })),
        polling: None,
    };
    assert_dashboard_snapshot("super_busy", &render(&data, 1_842.7));
}

#[test]
fn snapshot_fixture_backoff_queue_pressure() {
    let data = FrameData {
        running: vec![RunningRow {
            state: Some("retrying".into()),
            codex_total_tokens: 14_200,
            runtime_seconds: 1_225,
            turn_count: 7,
            last_codex_event: Some("notification".into()),
            last_message: notification(json!({
                "method": "codex/event/agent_message_delta",
                "params": {"msg": {"payload": {"delta": "waiting on rate-limit backoff window"}}}
            })),
            ..running("MT-638")
        }],
        retrying: vec![
            retry("MT-450", 4, 1_250, "rate limit exhausted"),
            retry("MT-451", 2, 3_900, "retrying after API timeout with jitter"),
            retry("MT-452", 6, 8_100, "worker crashed\nrestarting cleanly"),
            retry(
                "MT-453",
                1,
                11_000,
                "fourth queued retry should also render after removing the top-three limit",
            ),
        ],
        totals: Totals {
            input_tokens: 18_000,
            output_tokens: 2_200,
            total_tokens: 20_200,
            seconds_running: 2_700,
        },
        rate_limits: Some(json!({
            "limit_id": "gpt-5",
            "primary": {"remaining": 0, "limit": 20_000, "reset_in_seconds": 95},
            "secondary": {"remaining": 0, "limit": 60, "reset_in_seconds": 45},
            "credits": {"has_credits": false}
        })),
        polling: None,
    };
    assert_dashboard_snapshot("backoff_queue", &render(&data, 15.4));
}

#[test]
fn backoff_queue_row_escapes_escaped_newline_sequences() {
    let data = FrameData {
        retrying: vec![retry("MT-980", 1, 1_500, "error with \\nnewline")],
        ..idle()
    };
    let rendered = render(&data, 0.0);
    let lines: Vec<&str> = rendered
        .split('\n')
        .filter(|l| l.contains("MT-980"))
        .collect();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("error=error with newline"));
    assert!(!lines[0].contains("\\n"));
}

#[test]
fn snapshot_fixture_unlimited_credits_variant() {
    let data = FrameData {
        running: vec![RunningRow {
            codex_total_tokens: 3_200,
            runtime_seconds: 75,
            turn_count: 7,
            last_codex_event: Some("codex/event/token_count".into()),
            last_message: notification(json!({
                "method": "thread/tokenUsage/updated",
                "params": {"tokenUsage": {"total": {"inputTokens": 90, "outputTokens": 12, "totalTokens": 102}}}
            })),
            ..running("MT-777")
        }],
        retrying: vec![],
        totals: Totals {
            input_tokens: 90,
            output_tokens: 12,
            total_tokens: 102,
            seconds_running: 75,
        },
        rate_limits: Some(json!({
            "limit_id": "priority-tier",
            "primary": {"remaining": 100, "limit": 100, "reset_in_seconds": 1},
            "secondary": {"remaining": 500, "limit": 500, "reset_in_seconds": 1},
            "credits": {"unlimited": true}
        })),
        polling: None,
    };
    assert_dashboard_snapshot("credits_unlimited", &render(&data, 42.0));
}

#[test]
fn renders_linear_project_link_without_dashboard_line() {
    let rendered = render(&idle(), 0.0);
    assert!(rendered.contains("https://linear.app/project/project/issues"));
    assert!(!rendered.contains("Dashboard:"));
    let other = FrameContext {
        tracker_kind: Some("github".into()),
        ..ctx(None)
    };
    let plain = strip_ansi(&format_snapshot_content(Some(&idle()), 0.0, &other));
    assert!(plain.contains("│ Project: n/a"));
}

#[test]
fn renders_dashboard_url_on_its_own_line() {
    let rendered =
        format_snapshot_content(Some(&idle()), 0.0, &ctx(Some("http://127.0.0.1:4000/")));
    assert!(rendered.contains("│ Project:"));
    assert!(rendered.contains("│ Dashboard:"));
    assert!(rendered.contains("http://127.0.0.1:4000/"));
}

#[test]
fn renders_next_refresh_countdown_and_checking_marker() {
    let waiting = FrameData {
        polling: Some(Polling {
            checking: false,
            next_poll_in_ms: Some(2_000),
        }),
        ..idle()
    };
    let plain = strip_ansi(&render(&waiting, 0.0));
    assert!(plain.contains("│ Next refresh: 2s"));
    let rounding = FrameData {
        polling: Some(Polling {
            checking: false,
            next_poll_in_ms: Some(1_001),
        }),
        ..idle()
    };
    assert!(strip_ansi(&render(&rounding, 0.0)).contains("│ Next refresh: 2s"));
    let checking = FrameData {
        polling: Some(Polling {
            checking: true,
            next_poll_in_ms: None,
        }),
        ..idle()
    };
    assert!(render(&checking, 0.0).contains("checking now…"));
}

#[test]
fn spacer_line_before_backoff_queue_with_and_without_agents() {
    let plain = strip_ansi(&render(&idle(), 0.0));
    assert!(plain.contains("No active agents\n│\n├─ Backoff queue"));
    let busy = FrameData {
        running: vec![RunningRow {
            last_codex_event: Some("turn_completed".into()),
            last_message: turn_completed("completed"),
            ..running("MT-777")
        }],
        ..idle()
    };
    let plain = strip_ansi(&render(&busy, 0.0));
    let row_end = plain.find("MT-777").unwrap();
    let after = &plain[row_end..];
    let newline = after.find('\n').unwrap();
    assert!(after[newline..].starts_with("\n│\n├─ Backoff queue"));
}

#[test]
fn unstyled_closing_corner_when_retry_queue_is_empty() {
    let rendered = render(&idle(), 0.0);
    assert_eq!(rendered.split('\n').next_back(), Some("╰─"));
}

#[test]
fn unavailable_snapshot_frame() {
    let plain = strip_ansi(&format_snapshot_content(None, 3.0, &ctx(None)));
    assert_eq!(
        plain,
        "╭─ SYMPHONY STATUS\n│ Orchestrator snapshot unavailable\n│ Throughput: 3 tps\n│ Project: https://linear.app/project/project/issues\n│ Next refresh: n/a\n╰─"
    );
}

#[test]
fn renders_last_codex_message_in_event_column() {
    let row = RunningRow {
        identifier: Some("MT-233".into()),
        codex_total_tokens: 12,
        runtime_seconds: 15,
        turn_count: 0,
        last_message: turn_completed("completed"),
        ..running("MT-233")
    };
    let plain = strip_ansi(&format_running_summary(
        &row,
        running_event_width(TERMINAL_COLUMNS),
    ));
    assert_eq!(plain.matches("turn completed (completed)").count(), 1);
    assert!(!plain.contains(" notification "));
}

#[test]
fn strips_ansi_and_control_bytes_from_last_codex_message() {
    let payload = "cmd: \u{1b}[31mRED\u{1b}[0m\u{0} after\nline";
    let row = RunningRow {
        last_message: humanize_event_message(
            Some(CodexEventKind::Notification),
            Some(&Value::String(payload.into())),
        ),
        ..running("MT-898")
    };
    let plain = strip_ansi(&format_running_summary(
        &row,
        running_event_width(TERMINAL_COLUMNS),
    ));
    assert!(plain.contains("cmd: RED after line"), "{plain}");
    assert!(!plain.contains('\u{1b}'));
    assert!(!plain.contains('\u{0}'));
}

#[test]
fn expands_running_row_to_requested_terminal_width() {
    let row = RunningRow {
        codex_total_tokens: 123,
        runtime_seconds: 15,
        last_message: turn_completed("completed"),
        ..running("MT-598")
    };
    let plain = strip_ansi(&format_running_summary(&row, running_event_width(140)));
    assert_eq!(plain.chars().count(), 140);
}
