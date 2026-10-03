//! `GET /api/v1/events`: Server-Sent Events stream of state snapshots.
//!
//! Design (replaces the Elixir PubSub + LiveView push, which re-fetched a full snapshot per
//! browser for every Codex event):
//!
//! * One **hub** task per server watches the control plane's generation counter, debounces
//!   changes (leading + trailing edge, `sse_debounce`), takes **one** snapshot and publishes the
//!   encoded JSON in a `watch` channel. Orchestrator load is independent of the number of
//!   clients, and nothing is snapshotted while no client is connected.
//! * Each **client stream** is pull-based: it sends `retry: 3000`, then its own fresh snapshot
//!   (also on reconnect with `Last-Event-ID`; nothing is replayed), then hub frames and
//!   heartbeats. A slow client is simply polled less often by hyper; because the hub channel only
//!   keeps the latest frame, it skips intermediate snapshots and always receives the newest one.
//!   Memory per client is constant.
//! * Every stream ends when the server's shutdown token is cancelled, so graceful shutdown never
//!   waits on open dashboards.

use std::convert::Infallible;
use std::sync::{Arc, Once};

use axum::extract::State;
use axum::http::HeaderValue;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures::Stream;
use tokio::sync::{OwnedSemaphorePermit, watch};
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::app::AppState;
use crate::config::SSE_RETRY;
use crate::error::ApiError;
use crate::presenter;
use crate::view::{Heartbeat, StatePayload};

/// One encoded snapshot published by the hub.
#[derive(Debug)]
pub(crate) struct Frame {
    generation: u64,
    data: String,
}

/// Shared snapshot producer (see the module docs).
pub(crate) struct SseHub {
    frames: Arc<watch::Sender<Option<Arc<Frame>>>>,
    started: Once,
}

impl SseHub {
    pub(crate) fn new() -> Self {
        SseHub {
            frames: Arc::new(watch::Sender::new(None)),
            started: Once::new(),
        }
    }

    /// Start the hub task on first use.
    fn ensure_running(&self, app: &AppState) {
        self.started.call_once(|| {
            // Subscribe now, not when the task first runs: a change between this call and the
            // client's initial snapshot must not be lost.
            let changes = app.control.changes();
            tokio::spawn(run_hub(app.clone(), changes, Arc::clone(&self.frames)));
        });
    }
}

async fn run_hub(
    app: AppState,
    mut changes: watch::Receiver<u64>,
    frames: Arc<watch::Sender<Option<Arc<Frame>>>>,
) {
    let mut last_sent: Option<Instant> = None;
    loop {
        tokio::select! {
            biased;
            () = app.shutdown.cancelled() => return,
            changed = changes.changed() => {
                if changed.is_err() {
                    tracing::debug!("orchestrator change channel closed; SSE hub stops");
                    return;
                }
            }
        }
        // Leading edge: a change after a quiet period goes out at once. Trailing edge: changes
        // within the window are coalesced into one snapshot when the window ends.
        if let Some(sent) = last_sent {
            tokio::select! {
                biased;
                () = app.shutdown.cancelled() => return,
                () = tokio::time::sleep_until(sent + app.config.sse_debounce) => {}
            }
        }
        let generation = *changes.borrow_and_update();
        if frames.receiver_count() == 0 {
            continue;
        }
        let data = encode(&app.state_payload().await);
        frames.send_replace(Some(Arc::new(Frame { generation, data })));
        last_sent = Some(Instant::now());
    }
}

fn encode(payload: &StatePayload) -> String {
    serde_json::to_string(payload).unwrap_or_else(|err| {
        // Unreachable in practice: every field is a string-keyed JSON value.
        tracing::error!(error = %err, "cannot encode state payload");
        String::from(
            r#"{"error":{"code":"snapshot_unavailable","message":"Snapshot unavailable"}}"#,
        )
    })
}

enum Phase {
    Retry,
    Initial,
    Live,
}

struct Client {
    app: AppState,
    changes: watch::Receiver<u64>,
    frames: watch::Receiver<Option<Arc<Frame>>>,
    frames_open: bool,
    heartbeat: Interval,
    last_generation: u64,
    phase: Phase,
    _slot: Option<OwnedSemaphorePermit>,
}

/// `GET /api/v1/events`.
pub(crate) async fn events(State(app): State<AppState>) -> Response {
    let slot = match &app.sse_slots {
        None => None,
        Some(slots) => match Arc::clone(slots).try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                tracing::warn!(
                    max = app.config.max_sse_clients,
                    "rejecting SSE client: too many streams"
                );
                return ApiError::RequestFailed(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "Too many live event streams".to_owned(),
                )
                .into_response();
            }
        },
    };
    app.hub.ensure_running(&app);
    // Subscribe before the initial snapshot so no later frame can be missed.
    let frames = app.hub.frames.subscribe();
    let period = app.config.sse_heartbeat;
    let mut heartbeat = tokio::time::interval_at(Instant::now() + period, period);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let client = Client {
        changes: app.control.changes(),
        app,
        frames,
        frames_open: true,
        heartbeat,
        last_generation: 0,
        phase: Phase::Retry,
        _slot: slot,
    };

    let mut response = Sse::new(stream(client)).into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

fn stream(client: Client) -> impl Stream<Item = Result<Event, Infallible>> {
    futures::stream::unfold(client, |mut client| async move {
        let event = next_event(&mut client).await?;
        Some((Ok(event), client))
    })
}

async fn next_event(client: &mut Client) -> Option<Event> {
    match client.phase {
        Phase::Retry => {
            client.phase = Phase::Initial;
            Some(Event::default().retry(SSE_RETRY))
        }
        Phase::Initial => {
            client.phase = Phase::Live;
            let generation = *client.changes.borrow();
            let payload = client.app.state_payload().await;
            client.last_generation = generation;
            Some(snapshot_event(generation, &encode(&payload)))
        }
        Phase::Live => loop {
            tokio::select! {
                biased;
                () = client.app.shutdown.cancelled() => return None,
                _ = client.heartbeat.tick() => return Some(heartbeat_event(client.last_generation)),
                changed = client.frames.changed(), if client.frames_open => {
                    if changed.is_err() {
                        client.frames_open = false;
                        continue;
                    }
                    let frame = client.frames.borrow_and_update().clone();
                    if let Some(frame) = frame
                        && frame.generation > client.last_generation
                    {
                        client.last_generation = frame.generation;
                        return Some(snapshot_event(frame.generation, &frame.data));
                    }
                }
            }
        },
    }
}

fn snapshot_event(generation: u64, data: &str) -> Event {
    Event::default()
        .event("snapshot")
        .id(generation.to_string())
        .data(data)
}

fn heartbeat_event(generation: u64) -> Event {
    let heartbeat = Heartbeat {
        at: presenter::truncate_to_seconds(Utc::now()),
        generation,
    };
    let data = serde_json::to_string(&heartbeat).unwrap_or_else(|_| String::from("{}"));
    Event::default().event("heartbeat").data(data)
}
