//! Terminal status dashboard (Elixir `StatusDashboard`, blueprint E.5).
//!
//! * [`format`]: pure frame rendering (golden-fixture exact).
//! * [`tps`]: throughput and sparkline maths.
//! * [`Scheduler`]: the render coalescing state machine (E.5.2) — at most one frame per
//!   `render_interval_ms`, latest content wins, identical frames are never rewritten, and a frame is
//!   re-rendered at least every second so the throughput figure stays fresh.
//! * [`run`]: the task driving it from orchestrator change notifications, a `refresh_ms` tick and a
//!   flush timer.
//! * [`RuntimeSource`]: frames from the live runtime and workflow settings.
//!
//! Delta from Elixir (E.9 #6): the dashboard only runs when stdout is a terminal; the binary checks
//! that before spawning it.

pub mod format;
pub mod tps;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use symphony_codex::CodexEventKind;
use symphony_core::WorkflowStore;
use symphony_runtime::{RunningSnapshot, RuntimeHandle, Snapshot, humanize_codex_message};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use format::{FrameContext, FrameData};
use format::{Polling, RetryRow, RunningRow, Totals, format_snapshot_content};

/// Minimum interval between two renders of an unchanged snapshot (`@minimum_idle_rerender_ms`).
pub const MINIMUM_IDLE_RERENDER_MS: i64 = 1_000;

/// Render coalescing state (`StatusDashboard` struct fields, times in monotonic ms).
#[derive(Debug, Default)]
pub struct Scheduler {
    token_samples: Vec<tps::Sample>,
    last_tps_second: Option<i64>,
    last_tps_value: Option<f64>,
    last_rendered_content: Option<String>,
    last_rendered_at_ms: Option<i64>,
    pending_content: Option<String>,
    flush_at_ms: Option<i64>,
    fingerprint: Option<(Option<FrameData>, FrameContext)>,
}

impl Scheduler {
    /// A fresh scheduler (nothing rendered yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// When the pending frame must be flushed, if one is pending.
    pub fn flush_at_ms(&self) -> Option<i64> {
        self.flush_at_ms
    }

    /// The last frame written.
    pub fn last_rendered(&self) -> Option<&str> {
        self.last_rendered_content.as_deref()
    }

    /// `maybe_render/1`: folds a new snapshot (`None` = unavailable) in and returns the frame to
    /// write now, if any. A frame that cannot be written yet becomes pending (see
    /// [`Scheduler::flush_at_ms`] and [`Scheduler::on_flush`]).
    pub fn on_snapshot(
        &mut self,
        now_ms: i64,
        data: Option<FrameData>,
        ctx: FrameContext,
        render_interval_ms: i64,
    ) -> Option<String> {
        let current_tokens = match &data {
            Some(data) => {
                let total = i64::try_from(data.totals.total_tokens).unwrap_or(i64::MAX);
                self.token_samples = tps::update_token_samples(&self.token_samples, now_ms, total);
                total
            }
            None => {
                self.token_samples = tps::prune_samples(&self.token_samples, now_ms);
                0
            }
        };
        let (second, tps) = tps::throttled_tps(
            self.last_tps_second,
            self.last_tps_value,
            now_ms,
            &self.token_samples,
            current_tokens,
        );
        self.last_tps_second = Some(second);
        self.last_tps_value = Some(tps);

        let fingerprint = (data, ctx);
        let changed = self.fingerprint.as_ref() != Some(&fingerprint);
        let periodic = self
            .last_rendered_at_ms
            .is_none_or(|at| now_ms - at >= MINIMUM_IDLE_RERENDER_MS);
        if !changed && !periodic {
            return None;
        }
        let content = format_snapshot_content(fingerprint.0.as_ref(), tps, &fingerprint.1);
        self.fingerprint = Some(fingerprint);
        self.enqueue(content, now_ms, render_interval_ms)
    }

    fn enqueue(&mut self, content: String, now_ms: i64, render_interval_ms: i64) -> Option<String> {
        if self.last_rendered_content.as_deref() == Some(content.as_str()) {
            return None;
        }
        let render_now = match self.last_rendered_at_ms {
            None => self.flush_at_ms.is_none(),
            Some(at) => now_ms - at >= render_interval_ms,
        };
        if render_now {
            self.rendered(&content, now_ms);
            return Some(content);
        }
        self.pending_content = Some(content);
        if self.flush_at_ms.is_none() {
            let delay = match self.last_rendered_at_ms {
                None => 1,
                Some(at) => (render_interval_ms - (now_ms - at)).max(1),
            };
            self.flush_at_ms = Some(now_ms + delay);
        }
        None
    }

    /// The flush timer fired: returns the pending frame, if any.
    pub fn on_flush(&mut self, now_ms: i64) -> Option<String> {
        self.flush_at_ms = None;
        let content = self.pending_content.take()?;
        self.rendered(&content, now_ms);
        Some(content)
    }

    fn rendered(&mut self, content: &str, now_ms: i64) {
        self.last_rendered_content = Some(content.to_owned());
        self.last_rendered_at_ms = Some(now_ms);
        self.pending_content = None;
        self.flush_at_ms = None;
    }
}

/// Dashboard knobs (`observability.*`), re-read on every tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DashboardConfig {
    /// `observability.dashboard_enabled`.
    pub enabled: bool,
    /// `observability.refresh_ms`.
    pub refresh_ms: u64,
    /// `observability.render_interval_ms`.
    pub render_interval_ms: u64,
}

/// Where the dashboard gets its data.
#[async_trait]
pub trait DashboardSource: Send + Sync {
    /// The current snapshot (`None` when the orchestrator is unavailable) and frame context.
    async fn frame(&self) -> (Option<FrameData>, FrameContext);
    /// Change notifications (the orchestrator generation counter).
    fn changes(&self) -> watch::Receiver<u64>;
    /// Current dashboard configuration.
    fn config(&self) -> DashboardConfig;
}

/// Runs the dashboard until `cancel` fires or the configuration disables it, writing frames with
/// `render`.
pub async fn run(
    source: Arc<dyn DashboardSource>,
    mut render: Box<dyn FnMut(&str) + Send>,
    cancel: CancellationToken,
) {
    let started = Instant::now();
    let now_ms = || i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let mut scheduler = Scheduler::new();
    let mut changes = source.changes();
    let mut config = source.config();
    let mut next_tick = Instant::now();
    let mut changes_open = true;
    while config.enabled {
        let flush_at = scheduler
            .flush_at_ms()
            .map(|ms| started + Duration::from_millis(u64::try_from(ms).unwrap_or(0)));
        enum Wake {
            Snapshot,
            Flush,
        }
        let wake = tokio::select! {
            () = cancel.cancelled() => break,
            () = tokio::time::sleep_until(next_tick) => {
                next_tick = Instant::now() + Duration::from_millis(config.refresh_ms.max(1));
                Wake::Snapshot
            }
            changed = changes.changed(), if changes_open => {
                if changed.is_err() {
                    changes_open = false;
                }
                Wake::Snapshot
            }
            () = async {
                match flush_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => Wake::Flush,
        };
        config = source.config();
        let frame = match wake {
            Wake::Flush => scheduler.on_flush(now_ms()),
            Wake::Snapshot => {
                let (data, ctx) = source.frame().await;
                let interval = i64::try_from(config.render_interval_ms).unwrap_or(i64::MAX);
                scheduler.on_snapshot(now_ms(), data, ctx, interval)
            }
        };
        if let Some(frame) = frame {
            render(&frame);
        }
    }
}

/// Writes a frame to stdout (`render_to_terminal/1`).
pub fn render_to_terminal(content: &str) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    if let Err(err) = stdout
        .write_all(format::terminal_bytes(content).as_bytes())
        .and_then(|()| stdout.flush())
    {
        tracing::warn!("Failed rendering terminal dashboard frame: {err}");
    }
}

/// The live terminal width (`:io.columns/0`, then `COLUMNS`).
pub fn current_terminal_columns() -> usize {
    format::terminal_columns(tty_columns(), std::env::var("COLUMNS").ok().as_deref())
}

#[cfg(unix)]
fn tty_columns() -> Option<u16> {
    rustix::termios::tcgetwinsize(std::io::stdout())
        .ok()
        .map(|size| size.ws_col)
}

#[cfg(not(unix))]
fn tty_columns() -> Option<u16> {
    None
}

/// The status-colour key of a running row: the event name, except that wrapper notifications use
/// their `codex/event/*` method so `token_count` (yellow) and `task_started` (green) light up.
/// (Elixir compared strings against atoms, so live rows were nearly always blue.)
pub fn status_event_key(row: &RunningSnapshot) -> Option<String> {
    let event = row.last_codex_event?;
    if event == CodexEventKind::Notification
        && let Some(method) = row.last_codex_message.as_ref().and_then(|m| m.method())
        && method.starts_with("codex/event/")
    {
        return Some(method.to_owned());
    }
    Some(event.as_str().to_owned())
}

/// Converts a runtime snapshot into frame data.
pub fn frame_data(snapshot: &Snapshot) -> FrameData {
    FrameData {
        running: snapshot
            .running
            .iter()
            .map(|row| RunningRow {
                identifier: Some(row.identifier.clone()),
                state: row.state.clone(),
                session_id: row.session_id.clone(),
                codex_app_server_pid: row.codex_app_server_pid.clone(),
                codex_total_tokens: row.codex_total_tokens,
                runtime_seconds: row.runtime_seconds,
                turn_count: row.turn_count,
                last_codex_event: status_event_key(row),
                last_message: humanize_codex_message(row.last_codex_message.as_ref()),
            })
            .collect(),
        retrying: snapshot
            .retrying
            .iter()
            .map(|row| RetryRow {
                issue_id: Some(row.issue_id.clone()),
                identifier: Some(row.identifier.clone()),
                attempt: row.attempt,
                due_in_ms: row.due_in_ms,
                error: row.error.clone(),
            })
            .collect(),
        totals: Totals {
            input_tokens: snapshot.codex_totals.input_tokens,
            output_tokens: snapshot.codex_totals.output_tokens,
            total_tokens: snapshot.codex_totals.total_tokens,
            seconds_running: snapshot.codex_totals.seconds_running,
        },
        rate_limits: snapshot.rate_limits.clone(),
        polling: Some(Polling {
            checking: snapshot.polling.checking,
            next_poll_in_ms: snapshot.polling.next_poll_in_ms,
        }),
    }
}

/// Live [`DashboardSource`]: snapshots from the runtime, context from the workflow settings.
pub struct RuntimeSource {
    /// Orchestrator client.
    pub handle: RuntimeHandle,
    /// Workflow settings (re-read per frame).
    pub workflow: Arc<WorkflowStore>,
    /// Effective HTTP host (CLI/env override or `server.host`).
    pub host: String,
    /// Configured port (CLI/env override or `server.port`).
    pub configured_port: Option<u16>,
    /// Port actually bound by the HTTP server.
    pub bound_port: Option<u16>,
}

#[async_trait]
impl DashboardSource for RuntimeSource {
    async fn frame(&self) -> (Option<FrameData>, FrameContext) {
        let data = self.handle.snapshot().await.ok().map(|s| frame_data(&s));
        let settings = self.workflow.settings();
        let ctx = FrameContext {
            max_concurrent_agents: settings.agent.max_concurrent_agents,
            tracker_kind: settings.tracker.kind.clone(),
            project_slug: settings.tracker.project_slug.clone(),
            dashboard_url: format::dashboard_url(&self.host, self.configured_port, self.bound_port),
            columns: current_terminal_columns(),
        };
        (data, ctx)
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.handle.subscribe()
    }

    fn config(&self) -> DashboardConfig {
        let observability = self.workflow.settings().observability.clone();
        DashboardConfig {
            enabled: observability.dashboard_enabled,
            refresh_ms: observability.refresh_ms,
            render_interval_ms: observability.render_interval_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;
    use symphony_runtime::CodexMessage;

    use super::*;

    fn ctx() -> FrameContext {
        FrameContext {
            max_concurrent_agents: 10,
            tracker_kind: Some("linear".into()),
            project_slug: Some("project".into()),
            dashboard_url: None,
            columns: 115,
        }
    }

    fn data(tokens: u64) -> FrameData {
        FrameData {
            totals: Totals {
                total_tokens: tokens,
                ..Totals::default()
            },
            ..FrameData::default()
        }
    }

    #[test]
    fn coalesces_rapid_updates_to_one_render_per_interval() {
        let mut s = Scheduler::new();
        assert!(s.on_snapshot(0, Some(data(1)), ctx(), 16).is_some());
        // Unchanged data within a second: nothing.
        assert!(s.on_snapshot(5, Some(data(1)), ctx(), 16).is_none());
        // Two rapid changes: deferred, latest wins, one flush.
        assert!(s.on_snapshot(6, Some(data(2)), ctx(), 16).is_none());
        assert_eq!(s.flush_at_ms(), Some(16));
        assert!(s.on_snapshot(7, Some(data(3)), ctx(), 16).is_none());
        assert_eq!(s.flush_at_ms(), Some(16));
        let flushed = s.on_flush(16).expect("pending frame");
        assert!(format::tests::strip(&flushed).contains("total 3"));
        assert!(s.on_flush(17).is_none());
        // After the interval, a change renders immediately.
        assert!(s.on_snapshot(40, Some(data(4)), ctx(), 16).is_some());
    }

    #[test]
    fn identical_frames_are_never_rewritten_but_unavailable_frames_render() {
        let mut s = Scheduler::new();
        let first = s.on_snapshot(0, None, ctx(), 16).expect("first frame");
        assert!(format::tests::strip(&first).contains("Orchestrator snapshot unavailable"));
        // A periodic re-render with identical content is skipped.
        assert!(s.on_snapshot(2_000, None, ctx(), 16).is_none());
        assert_eq!(s.last_rendered(), Some(first.as_str()));
        // A context change (e.g. the server bound its port) re-renders.
        let mut with_url = ctx();
        with_url.dashboard_url = Some("http://127.0.0.1:4000/".into());
        assert!(s.on_snapshot(2_100, None, with_url, 16).is_some());
    }

    #[test]
    fn first_render_waits_for_a_pending_flush() {
        let mut s = Scheduler::new();
        s.flush_at_ms = Some(1);
        s.pending_content = Some("x".into());
        assert!(s.on_snapshot(0, Some(data(1)), ctx(), 16).is_none());
        assert!(s.on_flush(1).is_some());
    }

    fn running_row(event: Option<CodexEventKind>, method: Option<&str>) -> RunningSnapshot {
        RunningSnapshot {
            issue_id: "i".into(),
            identifier: "MT-1".into(),
            issue_url: None,
            state: Some("In Progress".into()),
            worker_host: None,
            workspace_path: None,
            session_id: None,
            codex_app_server_pid: None,
            codex_input_tokens: 0,
            codex_output_tokens: 0,
            codex_total_tokens: 0,
            turn_count: 0,
            retry_attempt: 0,
            started_at: chrono::Utc::now(),
            last_codex_timestamp: None,
            last_codex_message: method.map(|m| CodexMessage {
                event: event.unwrap_or(CodexEventKind::Notification),
                message: Some(json!({"method": m})),
                timestamp: chrono::Utc::now(),
            }),
            last_codex_event: event,
            runtime_seconds: 0,
        }
    }

    #[test]
    fn status_keys_use_wrapper_methods_for_notifications() {
        assert_eq!(status_event_key(&running_row(None, None)), None);
        assert_eq!(
            status_event_key(&running_row(Some(CodexEventKind::TurnCompleted), None)).as_deref(),
            Some("turn_completed")
        );
        assert_eq!(
            status_event_key(&running_row(
                Some(CodexEventKind::Notification),
                Some("codex/event/token_count")
            ))
            .as_deref(),
            Some("codex/event/token_count")
        );
        assert_eq!(
            status_event_key(&running_row(
                Some(CodexEventKind::Notification),
                Some("turn/started")
            ))
            .as_deref(),
            Some("notification")
        );
    }

    struct FakeSource {
        frames: Mutex<u64>,
        changes: watch::Receiver<u64>,
    }

    #[async_trait]
    impl DashboardSource for FakeSource {
        async fn frame(&self) -> (Option<FrameData>, FrameContext) {
            let tokens = *self.changes.borrow();
            *self.frames.lock().unwrap() += 1;
            (Some(data(tokens)), ctx())
        }
        fn changes(&self) -> watch::Receiver<u64> {
            self.changes.clone()
        }
        fn config(&self) -> DashboardConfig {
            DashboardConfig {
                enabled: true,
                refresh_ms: 60_000,
                render_interval_ms: 16,
            }
        }
    }

    async fn recv(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
        ms: u64,
    ) -> Result<Option<String>, tokio::time::error::Elapsed> {
        tokio::time::timeout(Duration::from_millis(ms), rx.recv()).await
    }

    #[tokio::test]
    async fn run_renders_on_notify_and_coalesces_bursts() {
        let (tx, rx) = watch::channel(0_u64);
        let source = Arc::new(FakeSource {
            frames: Mutex::new(0),
            changes: rx,
        });
        let (frames_tx, mut frames_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            source.clone(),
            Box::new(move |frame: &str| {
                let _ = frames_tx.send(frame.to_owned());
            }),
            cancel.clone(),
        ));
        // The initial tick renders immediately.
        assert!(recv(&mut frames_rx, 200).await.unwrap().is_some());
        tx.send_replace(1);
        tx.send_replace(2);
        let second = recv(&mut frames_rx, 200).await.unwrap().unwrap();
        assert!(
            format::tests::strip(&second).contains("total 2"),
            "{second}"
        );
        // Exactly one frame for this burst.
        assert!(recv(&mut frames_rx, 60).await.is_err());
        cancel.cancel();
        task.await.unwrap();
    }
}
