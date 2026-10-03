//! A real listener on an ephemeral port (port of "http server serves embedded assets, accepts
//! form posts, and rejects invalid hosts").

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{parse_block, running_state, static_snapshot};
use symphony_server::testing::StaticControlPlane;
use symphony_server::{ServerConfig, ServerError, resolve_host, serve};
use symphony_store::Store;
use tokio_util::sync::CancellationToken;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

#[tokio::test]
async fn serves_over_tcp_and_shuts_down_gracefully() {
    let control = Arc::new(StaticControlPlane::new(Ok(static_snapshot())));
    let shutdown = CancellationToken::new();
    let server = serve(
        ServerConfig::new("127.0.0.1", 0),
        control.clone(),
        Store::disabled(),
        shutdown.clone(),
    )
    .await
    .unwrap();
    let port = server.port();
    assert_ne!(port, 0, "ephemeral port is reported");
    assert_eq!(server.local_addr().ip().to_string(), "127.0.0.1");
    let base = format!("http://127.0.0.1:{port}");
    let http = client();

    let state: serde_json::Value = http
        .get(format!("{base}/api/v1/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        state["counts"],
        serde_json::json!({"running": 1, "retrying": 1, "blocked": 1})
    );

    let index = http.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(index.status(), 200);
    assert!(
        index
            .text()
            .await
            .unwrap()
            .contains("Symphony Observability")
    );

    let refresh = http
        .post(format!("{base}/api/v1/refresh"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(refresh.status(), 202);
    let refresh: serde_json::Value = refresh.json().await.unwrap();
    assert_eq!(refresh["queued"], true);

    let not_allowed = http
        .post(format!("{base}/api/v1/state"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .unwrap();
    assert_eq!(not_allowed.status(), 405);
    let body: serde_json::Value = not_allowed.json().await.unwrap();
    assert_eq!(body["error"]["code"], "method_not_allowed");

    // An open SSE stream receives live updates over the wire...
    let mut events = http
        .get(format!("{base}/api/v1/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(events.headers()["content-type"], "text/event-stream");
    let mut buffer = String::new();
    let mut blocks = Vec::new();
    for wanted in [2, 3] {
        if wanted == 3 {
            // Change the state only after the initial snapshot was taken.
            control.set_state(Ok(running_state(4)));
        }
        while blocks.len() < wanted {
            let chunk = events.chunk().await.unwrap().expect("stream stays open");
            buffer.push_str(std::str::from_utf8(&chunk).unwrap());
            while let Some(end) = buffer.find("\n\n") {
                blocks.push(parse_block(&buffer[..end + 2]));
                buffer.drain(..end + 2);
            }
        }
    }
    assert_eq!(blocks[1].json()["counts"]["running"], 1);
    assert_eq!(blocks[0].retry, Some(3000));
    assert_eq!(blocks[2].event.as_deref(), Some("snapshot"));
    assert_eq!(blocks[2].json()["counts"]["running"], 4);

    // ...and does not hold up graceful shutdown.
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server.wait())
        .await
        .expect("server stops promptly")
        .unwrap();
    assert!(
        events.chunk().await.map(|c| c.is_none()).unwrap_or(true),
        "stream ended"
    );
    assert!(
        http.get(format!("{base}/api/v1/health"))
            .send()
            .await
            .is_err(),
        "listener closed"
    );
}

#[tokio::test]
async fn invalid_hosts_and_busy_ports_are_startup_errors() {
    let control: Arc<dyn symphony_server::ControlPlane> =
        Arc::new(StaticControlPlane::unavailable());
    let error = serve(
        ServerConfig::new("bad host", 0),
        control.clone(),
        Store::disabled(),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ServerError::InvalidHost { .. }), "{error}");
    assert!(error.to_string().starts_with("invalid_host: bad host"));

    let shutdown = CancellationToken::new();
    let first = serve(
        ServerConfig::new("127.0.0.1", 0),
        control.clone(),
        Store::disabled(),
        shutdown.clone(),
    )
    .await
    .unwrap();
    let error = serve(
        ServerConfig::new("127.0.0.1", first.port()),
        control,
        Store::disabled(),
        CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ServerError::Bind { .. }), "{error}");
    shutdown.cancel();
    first.wait().await.unwrap();
}

#[tokio::test]
async fn hosts_resolve_like_elixir() {
    assert_eq!(
        resolve_host("127.0.0.1").await.unwrap().to_string(),
        "127.0.0.1"
    );
    assert_eq!(resolve_host("::1").await.unwrap().to_string(), "::1");
    assert_eq!(resolve_host("[::1]").await.unwrap().to_string(), "::1");
    assert_eq!(resolve_host("").await.unwrap().to_string(), "127.0.0.1");
    assert_eq!(
        resolve_host("0.0.0.0").await.unwrap().to_string(),
        "0.0.0.0"
    );
    let localhost = resolve_host("localhost").await.unwrap();
    assert!(localhost.is_loopback());
    assert!(resolve_host("bad host").await.is_err());
}
