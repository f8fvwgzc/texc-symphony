//! Binding and running the HTTP server.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::ServiceExt;
use axum::extract::Request;
use symphony_store::Store;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::app::router;
use crate::config::ServerConfig;
use crate::control::ControlPlane;
use crate::error::ServerError;

/// A running server (see [`serve`]).
#[derive(Debug)]
pub struct BoundServer {
    local_addr: SocketAddr,
    task: JoinHandle<Result<(), ServerError>>,
}

impl BoundServer {
    /// The address actually bound (the real port when `port` was `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The bound port (Elixir `HttpServer.bound_port/0`).
    pub fn port(&self) -> u16 {
        self.local_addr.port()
    }

    /// Wait until the server has stopped (after the shutdown token was cancelled and open
    /// connections finished, or `shutdown_grace` elapsed).
    pub async fn wait(self) -> Result<(), ServerError> {
        self.task
            .await
            .map_err(|err| ServerError::Task(err.to_string()))?
    }
}

/// Resolve `server.host`: an IP literal, else the first IPv4 address of the name, else its
/// first IPv6 address (Elixir `HttpServer.parse_host/1`).
pub async fn resolve_host(host: &str) -> Result<IpAddr, ServerError> {
    let host = host.trim();
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return Ok(ip);
    }
    let invalid = |reason: String| ServerError::InvalidHost {
        host: host.to_owned(),
        reason,
    };
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|err| invalid(err.to_string()))?
        .collect();
    addresses
        .iter()
        .find(|addr| addr.is_ipv4())
        .or_else(|| addresses.first())
        .map(SocketAddr::ip)
        .ok_or_else(|| invalid("no addresses found".to_owned()))
}

/// Bind `config.host:config.port` and serve the API and dashboard until `shutdown` is cancelled.
///
/// Returns once the listener is bound (so the caller can print the real URL). A bad host or a
/// bind failure is an error, which should abort startup like in Elixir. On shutdown, SSE streams
/// end immediately, in-flight requests may finish within `shutdown_grace`, and then
/// [`BoundServer::wait`] resolves.
pub async fn serve(
    config: ServerConfig,
    control: Arc<dyn ControlPlane>,
    store: Store,
    shutdown: CancellationToken,
) -> Result<BoundServer, ServerError> {
    let ip = resolve_host(&config.host).await?;
    let addr = SocketAddr::new(ip, config.port);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|source| ServerError::Bind { addr, source })?;
    let local_addr = listener
        .local_addr()
        .map_err(|source| ServerError::Bind { addr, source })?;
    tracing::info!(%local_addr, "observability server listening");

    let app = router(&config, control, store, shutdown.clone());
    let grace = config.shutdown_grace;
    let task = tokio::spawn(async move {
        let server = axum::serve(listener, ServiceExt::<Request>::into_make_service(app))
            .with_graceful_shutdown(shutdown.clone().cancelled_owned());
        let server = server.into_future();
        tokio::pin!(server);
        let result = tokio::select! {
            result = &mut server => result.map_err(ServerError::from),
            () = async {
                shutdown.cancelled().await;
                tokio::time::sleep(grace).await;
            } => {
                tracing::warn!(grace_ms = grace.as_millis(), "connections still open after shutdown grace; closing the server");
                Ok(())
            }
        };
        tracing::info!(%local_addr, "observability server stopped");
        result
    });
    Ok(BoundServer { local_addr, task })
}
