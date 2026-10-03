//! Pure frame rendering for the terminal status dashboard (Elixir `StatusDashboard`, E.5.5–E.5.8).
//!
//! Output is byte-for-byte the Elixir frame (checked against the golden fixtures in
//! `tests/fixtures/status_dashboard_snapshots/`), with raw ANSI SGR sequences rather than a styling
//! crate whose escape sequences would differ. Deliberate deltas (E.9): one line per retry entry even
//! when the error contains `", "`, and JSON instead of Elixir `inspect/2` for unrecognised
//! rate-limit shapes.

use serde_json::Value;
use symphony_runtime::humanize::format_count;
use unicode_segmentation::UnicodeSegmentation;

/// `IO.ANSI.reset/0`.
pub const RESET: &str = "\u{1b}[0m";
/// `IO.ANSI.bright/0` (bold).
pub const BOLD: &str = "\u{1b}[1m";
/// `IO.ANSI.faint/0`.
pub const DIM: &str = "\u{1b}[2m";
/// `IO.ANSI.red/0`.
pub const RED: &str = "\u{1b}[31m";
/// `IO.ANSI.green/0`.
pub const GREEN: &str = "\u{1b}[32m";
/// `IO.ANSI.yellow/0` (also the Elixir "orange").
pub const YELLOW: &str = "\u{1b}[33m";
/// `IO.ANSI.blue/0`.
pub const BLUE: &str = "\u{1b}[34m";
/// `IO.ANSI.magenta/0`.
pub const MAGENTA: &str = "\u{1b}[35m";
/// `IO.ANSI.cyan/0`.
pub const CYAN: &str = "\u{1b}[36m";
/// `IO.ANSI.light_black/0` (gray).
pub const GRAY: &str = "\u{1b}[90m";

const ID_WIDTH: usize = 8;
const STAGE_WIDTH: usize = 14;
const PID_WIDTH: usize = 8;
const AGE_WIDTH: usize = 12;
const TOKENS_WIDTH: usize = 10;
const SESSION_WIDTH: usize = 14;
const EVENT_DEFAULT_WIDTH: usize = 44;
const EVENT_MIN_WIDTH: usize = 12;
const ROW_CHROME_WIDTH: usize = 10;
/// Columns assumed when `COLUMNS` is set but unusable (Elixir `@default_terminal_columns`).
pub const INVALID_COLUMNS_FALLBACK: usize = 115;
const FIXED_RUNNING_WIDTH: usize =
    ID_WIDTH + STAGE_WIDTH + PID_WIDTH + AGE_WIDTH + TOKENS_WIDTH + SESSION_WIDTH;

/// One row of the running table.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RunningRow {
    /// Issue identifier (`unknown` when absent).
    pub identifier: Option<String>,
    /// Tracker state (`unknown` when absent).
    pub state: Option<String>,
    /// Codex session id.
    pub session_id: Option<String>,
    /// App-server OS pid.
    pub codex_app_server_pid: Option<String>,
    /// Tokens used by this run.
    pub codex_total_tokens: u64,
    /// Seconds since dispatch.
    pub runtime_seconds: u64,
    /// Turns started.
    pub turn_count: u32,
    /// Key choosing the status colour (`turn_completed`, `codex/event/token_count`, ...).
    pub last_codex_event: Option<String>,
    /// The humanized last Codex message (EVENT column).
    pub last_message: String,
}

/// One row of the backoff queue.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RetryRow {
    /// Tracker id (shown when the identifier is missing).
    pub issue_id: Option<String>,
    /// Issue identifier.
    pub identifier: Option<String>,
    /// Attempt number.
    pub attempt: u32,
    /// Milliseconds until the retry.
    pub due_in_ms: u64,
    /// Error that caused the retry.
    pub error: Option<String>,
}

/// Aggregate token and runtime counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Totals {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Total tokens.
    pub total_tokens: u64,
    /// Seconds of ended sessions.
    pub seconds_running: u64,
}

/// Poll status for the `Next refresh` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Polling {
    /// A poll cycle is running.
    pub checking: bool,
    /// Milliseconds until the next poll.
    pub next_poll_in_ms: Option<u64>,
}

/// The snapshot data a frame shows (also the render fingerprint).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FrameData {
    /// Running agents.
    pub running: Vec<RunningRow>,
    /// Retry queue.
    pub retrying: Vec<RetryRow>,
    /// Totals.
    pub totals: Totals,
    /// Latest Codex rate-limit payload.
    pub rate_limits: Option<Value>,
    /// Poll status (`None`: `Next refresh: n/a`).
    pub polling: Option<Polling>,
}

/// Configuration-derived parts of a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameContext {
    /// `agent.max_concurrent_agents`.
    pub max_concurrent_agents: u32,
    /// `tracker.kind`.
    pub tracker_kind: Option<String>,
    /// `tracker.project_slug`.
    pub project_slug: Option<String>,
    /// `Dashboard:` URL (see [`dashboard_url`]).
    pub dashboard_url: Option<String>,
    /// Terminal width (see [`terminal_columns`]).
    pub columns: usize,
}

/// `colorize/2`: `code <> text <> reset`.
pub fn colorize(text: &str, code: &str) -> String {
    format!("{code}{text}{RESET}")
}

/// The full frame for a snapshot (`Some`) or for an unavailable orchestrator (`None`).
pub fn format_snapshot_content(data: Option<&FrameData>, tps: f64, ctx: &FrameContext) -> String {
    let Some(data) = data else {
        let mut lines = vec![
            colorize("╭─ SYMPHONY STATUS", BOLD),
            colorize("│ Orchestrator snapshot unavailable", RED),
            throughput_line(tps),
        ];
        lines.extend(project_link_lines(ctx));
        lines.push(refresh_line(None));
        lines.push(closing_border().to_owned());
        return lines.join("\n");
    };
    let event_width = running_event_width(ctx.columns);
    let mut lines = vec![
        colorize("╭─ SYMPHONY STATUS", BOLD),
        format!(
            "{}{}{}{}",
            colorize("│ Agents: ", BOLD),
            colorize(&data.running.len().to_string(), GREEN),
            colorize("/", GRAY),
            colorize(&ctx.max_concurrent_agents.to_string(), GRAY)
        ),
        throughput_line(tps),
        format!(
            "{}{}",
            colorize("│ Runtime: ", BOLD),
            colorize(
                &format_runtime_seconds(data.totals.seconds_running),
                MAGENTA
            )
        ),
        format!(
            "{}{}{}{}{}{}",
            colorize("│ Tokens: ", BOLD),
            colorize(&format!("in {}", count(data.totals.input_tokens)), YELLOW),
            colorize(" | ", GRAY),
            colorize(&format!("out {}", count(data.totals.output_tokens)), YELLOW),
            colorize(" | ", GRAY),
            colorize(
                &format!("total {}", count(data.totals.total_tokens)),
                YELLOW
            ),
        ),
        format!(
            "{}{}",
            colorize("│ Rate Limits: ", BOLD),
            format_rate_limits(data.rate_limits.as_ref())
        ),
    ];
    lines.extend(project_link_lines(ctx));
    lines.push(refresh_line(data.polling));
    lines.push(colorize("├─ Running", BOLD));
    lines.push("│".to_owned());
    lines.push(running_table_header_row(event_width));
    lines.push(running_table_separator_row(event_width));
    if data.running.is_empty() {
        lines.push(format!("│  {}", colorize("No active agents", GRAY)));
        lines.push("│".to_owned());
    } else {
        let mut running: Vec<&RunningRow> = data.running.iter().collect();
        running.sort_by(|a, b| a.identifier.cmp(&b.identifier));
        lines.extend(
            running
                .into_iter()
                .map(|row| format_running_summary(row, event_width)),
        );
        lines.push("│".to_owned());
    }
    lines.push(colorize("├─ Backoff queue", BOLD));
    lines.push("│".to_owned());
    if data.retrying.is_empty() {
        lines.push(format!("│  {}", colorize("No queued retries", GRAY)));
    } else {
        let mut retrying: Vec<&RetryRow> = data.retrying.iter().collect();
        retrying.sort_by_key(|row| row.due_in_ms);
        lines.extend(retrying.into_iter().map(format_retry_summary));
    }
    lines.push(closing_border().to_owned());
    lines.join("\n")
}

/// The frame written when the application stops (`render_offline_status/0`).
pub fn offline_frame() -> String {
    [
        colorize("╭─ SYMPHONY STATUS", BOLD),
        colorize("│ app_status=offline", RED),
        closing_border().to_owned(),
    ]
    .join("\n")
}

/// `render_to_terminal/1`: cursor home, clear screen, frame, newline.
pub fn terminal_bytes(content: &str) -> String {
    format!("\u{1b}[H\u{1b}[2J{content}\n")
}

fn closing_border() -> &'static str {
    "╰─"
}

fn count(value: u64) -> String {
    format_count(i64::try_from(value).unwrap_or(i64::MAX))
}

fn throughput_line(tps: f64) -> String {
    format!(
        "{}{}",
        colorize("│ Throughput: ", BOLD),
        colorize(&format!("{} tps", format_tps(tps)), CYAN)
    )
}

fn project_link_lines(ctx: &FrameContext) -> Vec<String> {
    let project = match (ctx.tracker_kind.as_deref(), ctx.project_slug.as_deref()) {
        (Some("linear"), Some(slug)) if !slug.is_empty() => {
            colorize(&format!("https://linear.app/project/{slug}/issues"), CYAN)
        }
        _ => colorize("n/a", GRAY),
    };
    let mut lines = vec![format!("{}{project}", colorize("│ Project: ", BOLD))];
    if let Some(url) = &ctx.dashboard_url {
        lines.push(format!(
            "{}{}",
            colorize("│ Dashboard: ", BOLD),
            colorize(url, CYAN)
        ));
    }
    lines
}

fn refresh_line(polling: Option<Polling>) -> String {
    let value = match polling {
        Some(Polling { checking: true, .. }) => colorize("checking now…", CYAN),
        Some(Polling {
            next_poll_in_ms: Some(ms),
            ..
        }) => colorize(&format!("{}s", ms.div_ceil(1000)), CYAN),
        _ => colorize("n/a", GRAY),
    };
    format!("{}{value}", colorize("│ Next refresh: ", BOLD))
}

/// `dashboard_url/3`: `http://<host>:<port>/` from the configured and bound ports, `None` when no
/// port is configured or the port is not known yet (`0` before binding).
pub fn dashboard_url(
    host: &str,
    configured_port: Option<u16>,
    bound_port: Option<u16>,
) -> Option<String> {
    configured_port?;
    let port = bound_port.or(configured_port).filter(|port| *port > 0)?;
    Some(format!("http://{}:{port}/", dashboard_url_host(host)))
}

fn dashboard_url_host(host: &str) -> String {
    let host = host.trim();
    if matches!(host, "0.0.0.0" | "::" | "[::]" | "") {
        "127.0.0.1".to_owned()
    } else if host.starts_with('[') && host.ends_with(']') {
        host.to_owned()
    } else if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// `running_event_width/1`: `max(12, columns - 66 - 10)`.
pub fn running_event_width(columns: usize) -> usize {
    columns
        .saturating_sub(FIXED_RUNNING_WIDTH + ROW_CHROME_WIDTH)
        .max(EVENT_MIN_WIDTH)
}

/// Terminal width: the tty's columns when known and positive, else `COLUMNS` (positive integer),
/// else 120 when `COLUMNS` is unset and 115 when it is set but unusable.
pub fn terminal_columns(tty_columns: Option<u16>, columns_env: Option<&str>) -> usize {
    if let Some(columns) = tty_columns.filter(|c| *c > 0) {
        return usize::from(columns);
    }
    match columns_env {
        None => FIXED_RUNNING_WIDTH + ROW_CHROME_WIDTH + EVENT_DEFAULT_WIDTH,
        Some(value) => match value.trim().parse::<usize>() {
            Ok(columns) if columns > 0 => columns,
            _ => INVALID_COLUMNS_FALLBACK,
        },
    }
}

fn running_table_header_row(event_width: usize) -> String {
    let header = [
        format_cell("ID", ID_WIDTH, Align::Left),
        format_cell("STAGE", STAGE_WIDTH, Align::Left),
        format_cell("PID", PID_WIDTH, Align::Left),
        format_cell("AGE / TURN", AGE_WIDTH, Align::Left),
        format_cell("TOKENS", TOKENS_WIDTH, Align::Left),
        format_cell("SESSION", SESSION_WIDTH, Align::Left),
        format_cell("EVENT", event_width, Align::Left),
    ]
    .join(" ");
    format!("│   {}", colorize(&header, GRAY))
}

fn running_table_separator_row(event_width: usize) -> String {
    let width = FIXED_RUNNING_WIDTH + event_width + 6;
    format!("│   {}", colorize(&"─".repeat(width), GRAY))
}

/// Status colour of a running row (`format_running_summary/2`).
pub fn status_color(event: Option<&str>) -> &'static str {
    match event {
        Some("codex/event/token_count") => YELLOW,
        Some("codex/event/task_started") => GREEN,
        Some("turn_completed") => MAGENTA,
        _ => BLUE,
    }
}

/// One running row (`format_running_summary/2`); its visible width is `76 + event_width`.
pub fn format_running_summary(row: &RunningRow, event_width: usize) -> String {
    let issue = format_cell(
        row.identifier.as_deref().unwrap_or("unknown"),
        ID_WIDTH,
        Align::Left,
    );
    let stage = format_cell(
        row.state.as_deref().unwrap_or("unknown"),
        STAGE_WIDTH,
        Align::Left,
    );
    let session = format_cell(
        &compact_session_id(row.session_id.as_deref()),
        SESSION_WIDTH,
        Align::Left,
    );
    let pid = format_cell(
        row.codex_app_server_pid.as_deref().unwrap_or("n/a"),
        PID_WIDTH,
        Align::Left,
    );
    let age = format_cell(
        &format_runtime_and_turns(row.runtime_seconds, row.turn_count),
        AGE_WIDTH,
        Align::Left,
    );
    let tokens = format_cell(&count(row.codex_total_tokens), TOKENS_WIDTH, Align::Right);
    let event = format_cell(&row.last_message, event_width, Align::Left);
    let color = status_color(row.last_codex_event.as_deref());
    [
        "│ ".to_owned(),
        colorize("●", color),
        " ".to_owned(),
        colorize(&issue, CYAN),
        " ".to_owned(),
        colorize(&stage, color),
        " ".to_owned(),
        colorize(&pid, YELLOW),
        " ".to_owned(),
        colorize(&age, MAGENTA),
        " ".to_owned(),
        colorize(&tokens, YELLOW),
        " ".to_owned(),
        colorize(&session, CYAN),
        " ".to_owned(),
        colorize(&event, color),
    ]
    .concat()
}

/// One backoff-queue row (`format_retry_summary/1`).
pub fn format_retry_summary(row: &RetryRow) -> String {
    let identifier = row
        .identifier
        .as_deref()
        .or(row.issue_id.as_deref())
        .unwrap_or("unknown");
    format!(
        "│  {} {} {}{}{}{}",
        colorize("↻", YELLOW),
        colorize(identifier, RED),
        colorize(&format!("attempt={}", row.attempt), YELLOW),
        colorize(" in ", DIM),
        colorize(&next_in_words(row.due_in_ms), CYAN),
        format_retry_error(row.error.as_deref())
    )
}

/// `next_in_words/1`: `1250` → `1.250s`.
pub fn next_in_words(due_in_ms: u64) -> String {
    format!("{}.{:03}s", due_in_ms / 1000, due_in_ms % 1000)
}

fn format_retry_error(error: Option<&str>) -> String {
    let Some(error) = error else {
        return String::new();
    };
    let replaced = error
        .replace("\\r\\n", " ")
        .replace("\\r", " ")
        .replace("\\n", " ")
        .replace("\r\n", " ")
        .replace(['\r', '\n'], " ");
    let sanitized = collapse_whitespace(&replaced);
    let sanitized = sanitized.trim();
    if sanitized.is_empty() {
        String::new()
    } else {
        format!(
            " {}",
            colorize(&format!("error={}", truncate(sanitized, 96)), DIM)
        )
    }
}

/// `format_runtime_seconds/1`: `4321` → `72m 1s` (minutes unbounded).
pub fn format_runtime_seconds(seconds: u64) -> String {
    format!("{}m {}s", seconds / 60, seconds % 60)
}

fn format_runtime_and_turns(seconds: u64, turns: u32) -> String {
    if turns > 0 {
        format!("{} / {turns}", format_runtime_seconds(seconds))
    } else {
        format_runtime_seconds(seconds)
    }
}

/// `format_tps/1`: truncated and grouped (`1842.7` → `1,842`).
pub fn format_tps(value: f64) -> String {
    // Saturating float-to-int conversion; NaN becomes 0.
    format_count(value.trunc() as i64)
}

/// Horizontal alignment of [`format_cell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Pad on the right.
    Left,
    /// Pad on the left.
    Right,
}

/// Elixir `\s` without the unicode flag.
fn is_regex_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r')
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if is_regex_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `format_cell/3`: single-line, whitespace collapsed, trimmed, truncated to `width` (byte-length
/// check, grapheme slicing, like Elixir) and padded to `width` graphemes.
pub fn format_cell(value: &str, width: usize, align: Align) -> String {
    let collapsed = collapse_whitespace(&value.replace('\n', " "));
    let value = collapsed.trim();
    let value = if value.len() <= width {
        value.to_owned()
    } else {
        let mut out: String = value
            .graphemes(true)
            .take(width.saturating_sub(3))
            .collect();
        out.push_str("...");
        out
    };
    let pad = " ".repeat(width.saturating_sub(value.graphemes(true).count()));
    match align {
        Align::Left => format!("{value}{pad}"),
        Align::Right => format!("{pad}{value}"),
    }
}

/// `truncate/2`: when longer than `max` bytes, the first `max` graphemes plus `...`.
pub fn truncate(value: &str, max: usize) -> String {
    if value.len() > max {
        let mut out: String = value.graphemes(true).take(max).collect();
        out.push_str("...");
        out
    } else {
        value.to_owned()
    }
}

/// `compact_session_id/1`: `thread-1234567890` → `thre...567890`; `None` → `n/a`.
pub fn compact_session_id(session_id: Option<&str>) -> String {
    let Some(id) = session_id else {
        return "n/a".to_owned();
    };
    let graphemes: Vec<&str> = id.graphemes(true).collect();
    if graphemes.len() > 10 {
        format!(
            "{}...{}",
            graphemes[..4].concat(),
            graphemes[graphemes.len() - 6..].concat()
        )
    } else {
        id.to_owned()
    }
}

/// `map_value/2`: the first truthy (not null, not false) value among `keys`.
fn map_value<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let map = value.as_object()?;
    keys.iter()
        .filter_map(|key| map.get(*key))
        .find(|v| !matches!(v, Value::Null | Value::Bool(false)))
}

/// `to_string/1` of a JSON scalar (JSON text for compound values).
fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `Rate Limits:` value (`format_rate_limits/1`).
pub fn format_rate_limits(rate_limits: Option<&Value>) -> String {
    match rate_limits {
        None | Some(Value::Null) => colorize("unavailable", GRAY),
        Some(limits @ Value::Object(_)) => {
            let limit_id = map_value(limits, &["limit_id", "limit_name"])
                .map_or_else(|| "unknown".to_owned(), value_text);
            let primary = format_bucket(map_value(limits, &["primary"]));
            let secondary = format_bucket(map_value(limits, &["secondary"]));
            let credits = format_credits(map_value(limits, &["credits"]));
            [
                colorize(&limit_id, YELLOW),
                colorize(" | ", GRAY),
                colorize(&format!("primary {primary}"), CYAN),
                colorize(" | ", GRAY),
                colorize(&format!("secondary {secondary}"), CYAN),
                colorize(" | ", GRAY),
                colorize(&credits, GREEN),
            ]
            .concat()
        }
        Some(other) => colorize(&truncate(&other.to_string(), 80), GRAY),
    }
}

fn integer(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64)
}

fn format_bucket(bucket: Option<&Value>) -> String {
    let Some(bucket) = bucket else {
        return "n/a".to_owned();
    };
    let Value::Object(map) = bucket else {
        return value_text(bucket);
    };
    let remaining = integer(map_value(bucket, &["remaining"]));
    let limit = integer(map_value(bucket, &["limit"]));
    let base = match (remaining, limit) {
        (Some(remaining), Some(limit)) => {
            format!("{}/{}", format_count(remaining), format_count(limit))
        }
        (Some(remaining), None) => format!("remaining {}", format_count(remaining)),
        (None, Some(limit)) => format!("limit {}", format_count(limit)),
        (None, None) if map.is_empty() => "n/a".to_owned(),
        (None, None) => truncate(&bucket.to_string(), 40),
    };
    let reset = map_value(
        bucket,
        &[
            "reset_in_seconds",
            "resetInSeconds",
            "reset_at",
            "resetAt",
            "resets_at",
            "resetsAt",
        ],
    );
    match reset {
        None => base,
        Some(value) => {
            let text = match value.as_i64() {
                Some(seconds) => format!("{}s", format_count(seconds)),
                None => value_text(value),
            };
            format!("{base} reset {text}")
        }
    }
}

fn format_credits(credits: Option<&Value>) -> String {
    let Some(credits) = credits else {
        return "credits n/a".to_owned();
    };
    if !credits.is_object() {
        return format!("credits {}", value_text(credits));
    }
    let unlimited = map_value(credits, &["unlimited"]) == Some(&Value::Bool(true));
    let has_credits = map_value(credits, &["has_credits"]) == Some(&Value::Bool(true));
    let balance = map_value(credits, &["balance"]).filter(|v| v.is_number());
    if unlimited {
        "credits unlimited".to_owned()
    } else if let (true, Some(balance)) = (has_credits, balance) {
        format!("credits {}", format_number(balance))
    } else if has_credits {
        "credits available".to_owned()
    } else {
        "credits none".to_owned()
    }
}

/// Integers are grouped; floats are rounded to exactly two decimals and not grouped.
fn format_number(value: &Value) -> String {
    match value.as_i64() {
        Some(integer) => format_count(integer),
        None => {
            let float = value.as_f64().unwrap_or_default();
            format!("{:.2}", (float * 100.0).round() / 100.0)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn dashboard_url_prefers_the_bound_port_and_normalizes_wildcard_hosts() {
        assert_eq!(
            dashboard_url("0.0.0.0", Some(0), Some(43_123)).as_deref(),
            Some("http://127.0.0.1:43123/")
        );
        assert_eq!(
            dashboard_url("::1", Some(4000), None).as_deref(),
            Some("http://[::1]:4000/")
        );
        assert_eq!(
            dashboard_url("[::1]", Some(4000), None).as_deref(),
            Some("http://[::1]:4000/")
        );
        assert_eq!(
            dashboard_url(" ", Some(4000), None).as_deref(),
            Some("http://127.0.0.1:4000/")
        );
        assert_eq!(
            dashboard_url("example.com", Some(80), None).as_deref(),
            Some("http://example.com:80/")
        );
        assert_eq!(dashboard_url("127.0.0.1", None, Some(4000)), None);
        assert_eq!(dashboard_url("127.0.0.1", Some(0), None), None);
    }

    #[test]
    fn terminal_columns_fallbacks() {
        assert_eq!(terminal_columns(Some(140), Some("80")), 140);
        assert_eq!(terminal_columns(Some(0), Some(" 80 ")), 80);
        assert_eq!(terminal_columns(None, None), 120);
        assert_eq!(terminal_columns(None, Some("wide")), 115);
        assert_eq!(terminal_columns(None, Some("0")), 115);
        assert_eq!(running_event_width(115), 39);
        assert_eq!(running_event_width(20), 12);
    }

    #[test]
    fn cells_truncate_and_pad() {
        assert_eq!(format_cell("MT-1", 8, Align::Left), "MT-1    ");
        assert_eq!(format_cell("12", 5, Align::Right), "   12");
        assert_eq!(format_cell("a\n b\t\tc", 8, Align::Left), "a b c   ");
        assert_eq!(format_cell("abcdefghijk", 8, Align::Left), "abcde...");
        // Byte length decides truncation; graphemes are kept whole.
        assert_eq!(format_cell("ééééé", 8, Align::Left), "ééééé...");
        assert_eq!(
            compact_session_id(Some("thread-1234567890")),
            "thre...567890"
        );
        assert_eq!(compact_session_id(Some("short")), "short");
        assert_eq!(compact_session_id(None), "n/a");
        assert_eq!(truncate("abcdef", 3), "abc...");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn number_helpers() {
        assert_eq!(format_tps(1_842.7), "1,842");
        assert_eq!(format_tps(0.0), "0");
        assert_eq!(format_tps(f64::NAN), "0");
        assert_eq!(format_runtime_seconds(4_321), "72m 1s");
        assert_eq!(next_in_words(1_250), "1.250s");
        assert_eq!(next_in_words(11_000), "11.000s");
        assert_eq!(format_runtime_and_turns(75, 0), "1m 15s");
        assert_eq!(format_runtime_and_turns(75, 7), "1m 15s / 7");
    }

    #[test]
    fn rate_limit_shapes() {
        let plain = |v: Option<&Value>| strip(&format_rate_limits(v));
        assert_eq!(plain(None), "unavailable");
        assert_eq!(
            plain(Some(
                &json!({"limit_name": "tier", "primary": {"remaining": 5}, "secondary": {"limit": 9, "resetsAt": "soon"}, "credits": {"has_credits": true}})
            )),
            "tier | primary remaining 5 | secondary limit 9 reset soon | credits available"
        );
        assert_eq!(
            plain(Some(
                &json!({"primary": {}, "secondary": {"weird": true}, "credits": "x"})
            )),
            "unknown | primary n/a | secondary {\"weird\":true} | credits x"
        );
        assert_eq!(
            plain(Some(
                &json!({"credits": {"has_credits": true, "balance": 12345}})
            )),
            "unknown | primary n/a | secondary n/a | credits 12,345"
        );
        assert_eq!(plain(Some(&json!("odd"))), "\"odd\"");
    }

    #[test]
    fn retry_rows_collapse_escaped_and_real_newlines_on_one_line() {
        let row = format_retry_summary(&RetryRow {
            issue_id: Some("i-980".into()),
            identifier: Some("MT-980".into()),
            attempt: 1,
            due_in_ms: 1_500,
            error: Some("error with \\nnewline, and, commas\r\nhere".into()),
        });
        let plain = strip(&row);
        assert!(!plain.contains('\n'));
        assert!(
            plain.contains("error=error with newline, and, commas here"),
            "{plain}"
        );
        assert!(!plain.contains("\\n"));
        let no_identifier = format_retry_summary(&RetryRow {
            issue_id: Some("i-1".into()),
            identifier: None,
            attempt: 0,
            due_in_ms: 0,
            error: Some("   ".into()),
        });
        assert_eq!(strip(&no_identifier), "│  ↻ i-1 attempt=0 in 0.000s");
    }

    #[test]
    fn offline_frame_has_no_timestamp() {
        let frame = strip(&offline_frame());
        assert!(frame.contains("app_status=offline"));
        assert!(!frame.contains("Timestamp:"));
        assert_eq!(terminal_bytes("x"), "\u{1b}[H\u{1b}[2Jx\n");
    }

    pub(crate) fn strip(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next == 'm' {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }
}
