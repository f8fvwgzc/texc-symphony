//! Server configuration.

use std::time::Duration;

/// Default bind host (loopback, as the spec recommends).
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// Default `snapshot_timeout` (Elixir `snapshot_timeout_ms` 15 000).
pub const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(15);
/// Default `refresh_timeout` (Elixir `GenServer.call` default 5 000).
pub const DEFAULT_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
/// Default `request_timeout` (must exceed the snapshot timeout).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default SSE debounce window.
pub const DEFAULT_SSE_DEBOUNCE: Duration = Duration::from_millis(200);
/// Default SSE heartbeat interval.
pub const DEFAULT_SSE_HEARTBEAT: Duration = Duration::from_secs(15);
/// SSE `retry:` hint sent to clients.
pub const SSE_RETRY: Duration = Duration::from_millis(3000);
/// Default maximum number of concurrent SSE streams.
pub const DEFAULT_MAX_SSE_CLIENTS: usize = 64;
/// Default grace period for open connections after shutdown is requested.
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// How [`serve`](crate::serve) binds and behaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// IP literal or resolvable host name (`server.host`, default `127.0.0.1`). A name resolves to
    /// its first IPv4 address, else its first IPv6 address; failure aborts startup.
    pub host: String,
    /// TCP port (`server.port` / `--port`); `0` binds an ephemeral port (see
    /// [`BoundServer::local_addr`](crate::BoundServer::local_addr)).
    pub port: u16,
    /// Origins allowed by CORS (`*` allows any). Empty (the default) sends no CORS headers, so
    /// browsers only allow same-origin use.
    pub cors_allowed_origins: Vec<String>,
    /// Maximum concurrent `GET /api/v1/events` streams; further clients get
    /// `503 request_failed`. `0` means unlimited.
    pub max_sse_clients: usize,
    /// Upper bound for a state snapshot (`snapshot_timeout` when elapsed).
    pub snapshot_timeout: Duration,
    /// Upper bound for a refresh request (`503 orchestrator_unavailable` when elapsed).
    pub refresh_timeout: Duration,
    /// Upper bound for any non-streaming request (`503 request_failed` when elapsed).
    pub request_timeout: Duration,
    /// SSE debounce window (leading + trailing edge).
    pub sse_debounce: Duration,
    /// SSE heartbeat interval.
    pub sse_heartbeat: Duration,
    /// After shutdown is requested, how long open connections may take to finish before the
    /// server stops waiting for them.
    pub shutdown_grace: Duration,
    /// Version reported by `GET /api/v1/health` (the binary's crate version).
    pub version: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            host: DEFAULT_HOST.to_owned(),
            port: 0,
            cors_allowed_origins: Vec::new(),
            max_sse_clients: DEFAULT_MAX_SSE_CLIENTS,
            snapshot_timeout: DEFAULT_SNAPSHOT_TIMEOUT,
            refresh_timeout: DEFAULT_REFRESH_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            sse_debounce: DEFAULT_SSE_DEBOUNCE,
            sse_heartbeat: DEFAULT_SSE_HEARTBEAT,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

impl ServerConfig {
    /// Defaults with `host` and `port`.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        ServerConfig {
            host: host.into(),
            port,
            ..ServerConfig::default()
        }
    }
}
