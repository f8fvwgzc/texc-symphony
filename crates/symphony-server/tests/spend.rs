//! The kill switch endpoints and Prometheus metrics.

mod common;

use axum::http::{Method, StatusCode};
use common::{Harness, envelope, static_snapshot};
use symphony_server::SpendView;
use symphony_server::testing::StaticControlPlane;

#[tokio::test]
async fn pause_and_resume_drive_the_kill_switch() {
    let harness = Harness::snapshot();
    let (status, body) = harness
        .json(Method::POST, "/api/v1/pause?reason=runaway%20spend")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({"paused": true, "reason": "runaway spend"})
    );
    assert_eq!(harness.control.paused().as_deref(), Some("runaway spend"));

    let (status, body) = harness.json(Method::POST, "/api/v1/resume").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({"paused": false, "reason": null}));
    assert_eq!(harness.control.paused(), None);

    for uri in ["/api/v1/pause", "/api/v1/resume"] {
        let (status, body) = harness.json(Method::GET, uri).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
        assert_eq!(body, envelope("method_not_allowed", "Method not allowed"));
    }

    let down = Harness::new(StaticControlPlane::unavailable());
    let (status, body) = down.json(Method::POST, "/api/v1/pause").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        envelope("orchestrator_unavailable", "Orchestrator is unavailable")
    );
}

#[tokio::test]
async fn metrics_expose_queues_tokens_and_spend() {
    let mut view = static_snapshot();
    view.spend = Some(Box::new(SpendView {
        paused: true,
        stopped_reason: Some("dispatch paused: incident".into()),
        day: "2026-10-25".into(),
        day_tokens: 99,
        day_cost_usd: 0.25,
        total_cost_usd: 1.5,
        ..SpendView::default()
    }));
    let harness = Harness::new(StaticControlPlane::new(Ok(view)));
    let (status, headers, body) = harness.send(Method::GET, "/api/v1/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    let text = String::from_utf8(body.to_vec()).unwrap();
    for line in [
        "symphony_up 1",
        "symphony_running_agents 1",
        "symphony_retrying_issues 1",
        "symphony_blocked_issues 1",
        "symphony_input_tokens 4",
        "symphony_output_tokens 8",
        "symphony_cost_usd 1.5",
        "symphony_day_tokens 99",
        "symphony_day_cost_usd 0.25",
        "symphony_dispatch_paused 1",
        "symphony_dispatch_stopped 1",
        "# TYPE symphony_cost_usd gauge",
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "missing {line:?} in\n{text}"
        );
    }

    let down = Harness::new(StaticControlPlane::unavailable());
    let (status, _, body) = down.send(Method::GET, "/api/v1/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8(body.to_vec())
            .unwrap()
            .contains("symphony_up 0")
    );
}
