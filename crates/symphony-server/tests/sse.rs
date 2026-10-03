//! `GET /api/v1/events` (replaces the Elixir PubSub + LiveView push; ports
//! `observability_pubsub_test.exs` semantics: updates are delivered, and a missing publisher is
//! harmless).

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{Harness, SseEvent, SseReader, envelope, running_state};
use symphony_server::testing::StaticControlPlane;
use symphony_server::{ServerConfig, StateError};
use symphony_store::Store;
use tokio::time::{Instant, timeout};

fn events_request(last_event_id: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri("/api/v1/events")
        .header("accept", "text/event-stream")
        .header("accept-encoding", "gzip, br");
    if let Some(id) = last_event_id {
        builder = builder.header("last-event-id", id);
    }
    builder.body(Body::empty()).unwrap()
}

async fn connect(harness: &Harness, last_event_id: Option<&str>) -> SseReader {
    let response = harness.request(events_request(last_event_id)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(headers["cache-control"], "no-cache");
    assert_eq!(headers["x-accel-buffering"], "no");
    assert!(
        !headers.contains_key("content-encoding"),
        "SSE must never be compressed"
    );
    SseReader::new(response)
}

/// Read the `retry:` preamble and the initial snapshot.
async fn handshake(reader: &mut SseReader) -> SseEvent {
    let retry = reader.next().await.expect("retry frame");
    assert_eq!(retry.retry, Some(3000));
    assert_eq!(retry.event, None);
    let snapshot = reader.next().await.expect("initial snapshot");
    assert_eq!(snapshot.event.as_deref(), Some("snapshot"));
    snapshot
}

async fn next_within(reader: &mut SseReader, limit: Duration) -> Option<SseEvent> {
    timeout(limit, reader.next()).await.ok().flatten()
}

#[tokio::test(start_paused = true)]
async fn stream_starts_with_retry_and_the_current_snapshot() {
    let harness = Harness::new(StaticControlPlane::new(Ok(running_state(2))));
    harness.control.notify();
    harness.control.notify();
    let mut reader = connect(&harness, None).await;
    let snapshot = handshake(&mut reader).await;
    assert_eq!(snapshot.id.as_deref(), Some("2"), "id is the generation");
    let body = snapshot.json();
    assert_eq!(body["counts"]["running"], 2);
    // Same body as GET /api/v1/state.
    let (_, state) = harness.get("/api/v1/state").await;
    assert_eq!(body["running"], state["running"]);
}

#[tokio::test(start_paused = true)]
async fn changes_are_debounced_on_leading_and_trailing_edges() {
    let harness = Harness::new(StaticControlPlane::new(Ok(running_state(0))));
    let mut reader = connect(&harness, None).await;
    let initial = handshake(&mut reader).await;
    assert_eq!(initial.id.as_deref(), Some("0"));

    // Leading edge: the first change after a quiet period is pushed at once.
    let started = Instant::now();
    harness.control.set_state(Ok(running_state(1)));
    let first = next_within(&mut reader, Duration::from_secs(1))
        .await
        .expect("leading-edge snapshot");
    assert_eq!(first.id.as_deref(), Some("1"));
    assert_eq!(first.json()["counts"]["running"], 1);
    assert!(started.elapsed() < Duration::from_millis(50));

    // A burst inside the window collapses into one trailing snapshot with the latest state.
    let burst = Instant::now();
    for n in 2..=6 {
        harness.control.set_state(Ok(running_state(n)));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let trailing = next_within(&mut reader, Duration::from_secs(1))
        .await
        .expect("trailing-edge snapshot");
    assert_eq!(trailing.id.as_deref(), Some("6"));
    assert_eq!(trailing.json()["counts"]["running"], 6);
    assert!(
        burst.elapsed() >= Duration::from_millis(150),
        "waited for the debounce window: {:?}",
        burst.elapsed()
    );
    assert!(burst.elapsed() <= Duration::from_millis(250));

    // Nothing else is pending.
    assert_eq!(next_within(&mut reader, Duration::from_secs(5)).await, None);
    // The orchestrator was asked for 3 snapshots in total: initial, leading, trailing.
    assert_eq!(harness.control.state_calls(), 3);
}

#[tokio::test(start_paused = true)]
async fn heartbeats_arrive_every_15_seconds_without_an_id() {
    let harness = Harness::new(StaticControlPlane::new(Ok(running_state(1))));
    harness.control.notify();
    let mut reader = connect(&harness, None).await;
    handshake(&mut reader).await;

    let started = Instant::now();
    let heartbeat = next_within(&mut reader, Duration::from_secs(16))
        .await
        .expect("heartbeat");
    assert_eq!(started.elapsed(), Duration::from_secs(15));
    assert_eq!(heartbeat.event.as_deref(), Some("heartbeat"));
    assert_eq!(heartbeat.id, None);
    let data = heartbeat.json();
    assert_eq!(data["generation"], 1);
    let at = data["at"].as_str().unwrap();
    assert!(at.ends_with('Z') && !at.contains('.'), "{at}");

    let again = next_within(&mut reader, Duration::from_secs(16))
        .await
        .expect("second heartbeat");
    assert_eq!(again.event.as_deref(), Some("heartbeat"));
}

#[tokio::test(start_paused = true)]
async fn reconnecting_with_last_event_id_gets_the_current_snapshot() {
    let harness = Harness::new(StaticControlPlane::new(Ok(running_state(3))));
    for _ in 0..7 {
        harness.control.notify();
    }
    let mut reader = connect(&harness, Some("3")).await;
    let snapshot = handshake(&mut reader).await;
    assert_eq!(snapshot.id.as_deref(), Some("7"));
    assert_eq!(snapshot.json()["counts"]["running"], 3);
    // Nothing is replayed.
    assert_eq!(next_within(&mut reader, Duration::from_secs(1)).await, None);
}

#[tokio::test(start_paused = true)]
async fn slow_clients_skip_to_the_latest_snapshot() {
    let harness = Harness::new(StaticControlPlane::new(Ok(running_state(0))));
    let mut slow = connect(&harness, None).await;
    let mut fast = connect(&harness, None).await;
    handshake(&mut slow).await;
    handshake(&mut fast).await;

    for n in 1..=5 {
        harness.control.set_state(Ok(running_state(n)));
        let event = next_within(&mut fast, Duration::from_secs(1))
            .await
            .expect("fast client keeps up");
        assert_eq!(event.id, Some(n.to_string()));
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // The slow client never read in between: it gets only the newest snapshot.
    let event = next_within(&mut slow, Duration::from_secs(1))
        .await
        .expect("latest snapshot");
    assert_eq!(event.id.as_deref(), Some("5"));
    assert_eq!(event.json()["counts"]["running"], 5);
    assert_eq!(next_within(&mut slow, Duration::from_secs(1)).await, None);
    // One snapshot per change for both clients together (plus one initial per client).
    assert_eq!(harness.control.state_calls(), 2 + 5);
}

#[tokio::test(start_paused = true)]
async fn snapshot_errors_are_sent_in_band() {
    let harness = Harness::new(StaticControlPlane::new(Err(StateError::Unavailable)));
    let mut reader = connect(&harness, None).await;
    let snapshot = handshake(&mut reader).await;
    assert_eq!(
        snapshot.json()["error"],
        serde_json::json!({"code": "snapshot_unavailable", "message": "Snapshot unavailable"})
    );
    harness.control.set_state(Err(StateError::Timeout));
    let next = next_within(&mut reader, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(next.json()["error"]["code"], "snapshot_timeout");
}

#[tokio::test(start_paused = true)]
async fn a_closed_change_channel_keeps_heartbeats_flowing() {
    let harness = Harness::new(StaticControlPlane::new(Ok(running_state(1))));
    let mut reader = connect(&harness, None).await;
    handshake(&mut reader).await;
    harness.control.close_changes();
    let heartbeat = next_within(&mut reader, Duration::from_secs(16))
        .await
        .expect("heartbeat");
    assert_eq!(heartbeat.event.as_deref(), Some("heartbeat"));
}

#[tokio::test(start_paused = true)]
async fn client_limit_is_enforced() {
    let config = ServerConfig {
        max_sse_clients: 1,
        ..ServerConfig::default()
    };
    let harness = Harness::with(
        StaticControlPlane::new(Ok(running_state(0))),
        Store::disabled(),
        config,
    );
    let first = connect(&harness, None).await;
    let (status, body) = harness.json(Method::GET, "/api/v1/events").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        envelope("request_failed", "Too many live event streams")
    );
    drop(first);
    let mut again = connect(&harness, None).await;
    handshake(&mut again).await;
}

#[tokio::test(start_paused = true)]
async fn streams_end_on_shutdown() {
    let harness = Harness::new(StaticControlPlane::new(Ok(running_state(0))));
    let mut reader = connect(&harness, None).await;
    handshake(&mut reader).await;
    harness.shutdown.cancel();
    assert_eq!(next_within(&mut reader, Duration::from_secs(1)).await, None);
}
