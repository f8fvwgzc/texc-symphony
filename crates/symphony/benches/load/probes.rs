//! Observers of the loaded process: snapshot latency, SSE frames over a real socket, resident
//! memory, and warnings the runtime and the store log.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use symphony_runtime::RuntimeHandle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

use crate::Res;
use crate::stats::{Summary, ms};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Every guarded value here is a plain sample buffer, valid after a holder panicked.
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ----- log counters ------------------------------------------------------------------------------

/// Counts of WARN/ERROR log lines, by what they report.
#[derive(Debug, Default)]
pub struct LogCounts {
    /// `symphony-store queue is full; dropping the operation` (a submission answered `Busy`).
    pub store_busy: AtomicU64,
    /// Events a run's recorder dropped because its queue was full (summed from the warning's
    /// `dropped` field when the run finishes).
    pub recorder_dropped: AtomicU64,
    pub errors: AtomicU64,
    /// Other warnings and errors, by the first words of their message.
    other: Mutex<BTreeMap<String, u64>>,
}

impl LogCounts {
    /// JSON form.
    pub fn json(&self) -> Value {
        let other: BTreeMap<String, u64> = lock(&self.other).clone();
        json!({
            "store_busy": self.store_busy.load(Ordering::Relaxed),
            "recorder_dropped_events": self.recorder_dropped.load(Ordering::Relaxed),
            "errors": self.errors.load(Ordering::Relaxed),
            "other_warnings": other,
        })
    }

    /// Clears every counter (between scenarios of one process).
    pub fn reset(&self) {
        self.store_busy.store(0, Ordering::Relaxed);
        self.recorder_dropped.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
        lock(&self.other).clear();
    }
}

#[derive(Default)]
struct WarningFields {
    message: String,
    dropped: Option<u64>,
}

impl Visit for WarningFields {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "dropped" {
            self.dropped = Some(value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

struct CountingLayer(&'static LogCounts);

impl<S: Subscriber> Layer<S> for CountingLayer {
    // INFO and above, like a production log level: DEBUG and TRACE call sites stay disabled.
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        *metadata.level() <= Level::INFO
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::INFO)
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let level = *event.metadata().level();
        if level > Level::WARN {
            return;
        }
        let mut fields = WarningFields::default();
        event.record(&mut fields);
        if level == Level::ERROR {
            self.0.errors.fetch_add(1, Ordering::Relaxed);
        }
        if fields.message.contains("queue is full; dropping") {
            self.0.store_busy.fetch_add(1, Ordering::Relaxed);
        } else if let Some(dropped) = fields.dropped {
            self.0
                .recorder_dropped
                .fetch_add(dropped, Ordering::Relaxed);
        } else {
            // Ids and numbers vary per line; the first plain words name the kind of warning.
            let key: String = fields
                .message
                .split_whitespace()
                .filter(|word| !word.contains('=') && !word.bytes().any(|b| b.is_ascii_digit()))
                .take(4)
                .collect::<Vec<_>>()
                .join(" ");
            *lock(&self.0.other).entry(key).or_insert(0) += 1;
        }
    }
}

/// Installs the process-wide log counter (once) and returns its counters. Nothing is formatted
/// or written for INFO lines, so the cost of production logging is not part of any measurement.
pub fn log_counts() -> &'static LogCounts {
    static COUNTS: OnceLock<LogCounts> = OnceLock::new();
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    let counts = COUNTS.get_or_init(LogCounts::default);
    if !INSTALLED.swap(true, Ordering::SeqCst) {
        let subscriber = Registry::default().with(CountingLayer(counts));
        // Fails only when a subscriber is already set, which this function alone does.
        let _ = tracing::subscriber::set_global_default(subscriber);
    }
    counts
}

// ----- generation timeline -----------------------------------------------------------------------

/// How often the generation counter is sampled.
const TIMELINE_PERIOD: Duration = Duration::from_millis(2);
/// How much history is kept.
const TIMELINE_WINDOW: Duration = Duration::from_secs(30);

/// When the orchestrator reached each generation (sampled), so a frame's age can be told from its
/// id.
#[derive(Debug)]
pub struct GenTimeline {
    samples: Arc<Mutex<VecDeque<(Instant, u64)>>>,
    task: JoinHandle<()>,
}

impl GenTimeline {
    pub fn start(generation: watch::Receiver<u64>) -> Self {
        let samples = Arc::new(Mutex::new(VecDeque::new()));
        let shared = Arc::clone(&samples);
        let task = tokio::spawn(async move {
            let mut last = None;
            loop {
                tokio::time::sleep(TIMELINE_PERIOD).await;
                let current = *generation.borrow();
                if last == Some(current) {
                    continue;
                }
                last = Some(current);
                let now = Instant::now();
                let mut samples = lock(&shared);
                samples.push_back((now, current));
                while samples
                    .front()
                    .is_some_and(|(at, _)| now.duration_since(*at) > TIMELINE_WINDOW)
                {
                    samples.pop_front();
                }
            }
        });
        Self { samples, task }
    }

    /// When `generation` was first seen: `None` when that is older than the window, now when the
    /// sampler has not seen it yet.
    fn reached_at(samples: &VecDeque<(Instant, u64)>, generation: u64) -> Option<Instant> {
        let index = samples.partition_point(|(_, seen)| *seen < generation);
        if index == 0 && samples.front().is_some_and(|(_, seen)| *seen > generation) {
            return None;
        }
        Some(samples.get(index).map_or_else(Instant::now, |(at, _)| *at))
    }
}

impl Drop for GenTimeline {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// ----- snapshot probe ----------------------------------------------------------------------------

/// One `RuntimeHandle::snapshot` call.
#[derive(Debug, Clone, Copy)]
pub struct SnapshotSample {
    pub at: Instant,
    pub latency: Duration,
    pub ok: bool,
    pub running: usize,
    /// Age of the newest "last integrated event" among the running entries, in ms: with many
    /// runs emitting, how far the orchestrator is behind the event streams.
    pub queue_delay_ms: f64,
}

/// Calls `snapshot()` at a fixed period and keeps every sample.
#[derive(Debug)]
pub struct SnapshotProbe {
    samples: Arc<Mutex<Vec<SnapshotSample>>>,
    task: JoinHandle<()>,
}

impl SnapshotProbe {
    pub fn start(handle: RuntimeHandle, period: Duration) -> Self {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&samples);
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let at = Instant::now();
                let result = handle.snapshot().await;
                let latency = at.elapsed();
                let sample = match result {
                    Ok(snapshot) => {
                        let newest = snapshot
                            .running
                            .iter()
                            .filter_map(|row| row.last_codex_timestamp)
                            .max();
                        let delay = newest.map_or(0.0, |newest| {
                            (snapshot.generated_at - newest).to_std().map_or(0.0, ms)
                        });
                        SnapshotSample {
                            at,
                            latency,
                            ok: true,
                            running: snapshot.running.len(),
                            queue_delay_ms: delay,
                        }
                    }
                    Err(_) => SnapshotSample {
                        at,
                        latency,
                        ok: false,
                        running: 0,
                        queue_delay_ms: 0.0,
                    },
                };
                lock(&shared).push(sample);
            }
        });
        Self { samples, task }
    }

    /// Samples taken in `[from, to)`.
    pub fn window(&self, from: Instant, to: Instant) -> Vec<SnapshotSample> {
        lock(&self.samples)
            .iter()
            .filter(|s| s.at >= from && s.at < to)
            .copied()
            .collect()
    }
}

impl Drop for SnapshotProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Statistics of a set of snapshot samples.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnapshotStats {
    pub latency: Summary,
    pub failed: usize,
    /// See [`SnapshotSample::queue_delay_ms`].
    pub queue_delay: Summary,
    pub mean_running: f64,
}

impl SnapshotStats {
    pub fn of(samples: &[SnapshotSample]) -> Self {
        let ok: Vec<&SnapshotSample> = samples.iter().filter(|s| s.ok).collect();
        let latencies: Vec<Duration> = samples.iter().map(|s| s.latency).collect();
        let delays: Vec<f64> = ok.iter().map(|s| s.queue_delay_ms).collect();
        let mean_running = if ok.is_empty() {
            0.0
        } else {
            ok.iter().map(|s| s.running as f64).sum::<f64>() / ok.len() as f64
        };
        Self {
            latency: Summary::of(&latencies),
            failed: samples.len() - ok.len(),
            queue_delay: Summary::of_ms(&delays),
            mean_running,
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "latency": self.latency.json(),
            "failed": self.failed,
            "event_queue_delay": self.queue_delay.json(),
            "mean_running": crate::stats::round3(self.mean_running),
        })
    }
}

// ----- SSE probe ---------------------------------------------------------------------------------

/// One `snapshot` frame read from `GET /api/v1/events`.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub at: Instant,
    pub bytes: usize,
    /// Time between the orchestrator reaching the frame's generation and the frame being read.
    pub delivery: Option<Duration>,
}

/// A dashboard stand-in: one HTTP client reading the live event stream.
#[derive(Debug)]
pub struct SseProbe {
    frames: Arc<Mutex<Vec<Frame>>>,
    error: Arc<Mutex<Option<String>>>,
    task: JoinHandle<()>,
}

impl SseProbe {
    pub fn start(url: String, timeline: &GenTimeline) -> Self {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let error = Arc::new(Mutex::new(None));
        let (shared, failure) = (Arc::clone(&frames), Arc::clone(&error));
        let samples = Arc::clone(&timeline.samples);
        let task = tokio::spawn(async move {
            if let Err(err) = read_stream(&url, &shared, &samples).await {
                *lock(&failure) = Some(err.to_string());
            }
        });
        Self {
            frames,
            error,
            task,
        }
    }

    /// Frames read in `[from, to)`.
    pub fn window(&self, from: Instant, to: Instant) -> Vec<Frame> {
        lock(&self.frames)
            .iter()
            .filter(|f| f.at >= from && f.at < to)
            .copied()
            .collect()
    }

    /// Why the stream ended early, if it did.
    pub fn error(&self) -> Option<String> {
        lock(&self.error).clone()
    }
}

impl Drop for SseProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_stream(
    url: &str,
    frames: &Mutex<Vec<Frame>>,
    timeline: &Mutex<VecDeque<(Instant, u64)>>,
) -> Res<()> {
    let client = reqwest::Client::builder().build()?;
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut buffer: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        buffer.extend_from_slice(&chunk);
        // Events end with a blank line.
        while let Some(end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
            let block: Vec<u8> = buffer.drain(..end + 2).collect();
            let at = Instant::now();
            let text = String::from_utf8_lossy(&block);
            let mut is_snapshot = false;
            let mut generation = None;
            for line in text.lines() {
                if line == "event: snapshot" {
                    is_snapshot = true;
                } else if let Some(id) = line.strip_prefix("id: ") {
                    generation = id.trim().parse::<u64>().ok();
                }
            }
            if !is_snapshot {
                continue;
            }
            let delivery = generation.and_then(|generation| {
                GenTimeline::reached_at(&lock(timeline), generation)
                    .map(|reached| at.saturating_duration_since(reached))
            });
            lock(frames).push(Frame {
                at,
                bytes: block.len(),
                delivery,
            });
        }
    }
    Err("the event stream ended".into())
}

/// Statistics of a set of frames.
#[derive(Debug, Clone, Copy, Default)]
pub struct SseStats {
    pub frames: usize,
    /// Time between two consecutive frames.
    pub gap: Summary,
    pub delivery: Summary,
    pub mean_bytes: f64,
    pub max_bytes: usize,
}

impl SseStats {
    pub fn of(frames: &[Frame]) -> Self {
        let gaps: Vec<Duration> = frames
            .windows(2)
            .map(|pair| pair[1].at.duration_since(pair[0].at))
            .collect();
        let deliveries: Vec<Duration> = frames.iter().filter_map(|f| f.delivery).collect();
        let total: usize = frames.iter().map(|f| f.bytes).sum();
        Self {
            frames: frames.len(),
            gap: Summary::of(&gaps),
            delivery: Summary::of(&deliveries),
            mean_bytes: if frames.is_empty() {
                0.0
            } else {
                total as f64 / frames.len() as f64
            },
            max_bytes: frames.iter().map(|f| f.bytes).max().unwrap_or(0),
        }
    }

    pub fn json(&self) -> Value {
        json!({
            "frames": self.frames,
            "gap": self.gap.json(),
            "delivery": self.delivery.json(),
            "mean_frame_bytes": self.mean_bytes.round(),
            "max_frame_bytes": self.max_bytes,
        })
    }
}

// ----- resident memory ---------------------------------------------------------------------------

/// Resident set size of this process in bytes, read with `ps` (no `unsafe`, same on macOS and
/// Linux).
pub async fn rss_bytes() -> Option<u64> {
    let output = tokio::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .await
        .ok()?;
    let kib: u64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .ok()?;
    Some(kib * 1024)
}

/// Samples resident memory at a fixed period.
#[derive(Debug)]
pub struct RssProbe {
    samples: Arc<Mutex<Vec<(Instant, u64)>>>,
    task: JoinHandle<()>,
}

impl RssProbe {
    pub fn start(period: Duration) -> Self {
        let samples = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&samples);
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            loop {
                ticker.tick().await;
                if let Some(bytes) = rss_bytes().await {
                    lock(&shared).push((Instant::now(), bytes));
                }
            }
        });
        Self { samples, task }
    }

    /// Samples taken in `[from, to)`.
    pub fn window(&self, from: Instant, to: Instant) -> Vec<(Instant, u64)> {
        lock(&self.samples)
            .iter()
            .filter(|(at, _)| *at >= from && *at < to)
            .copied()
            .collect()
    }

    /// The latest sample.
    pub fn last(&self) -> Option<u64> {
        lock(&self.samples).last().map(|(_, bytes)| *bytes)
    }
}

impl Drop for RssProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}
