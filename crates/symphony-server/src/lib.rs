//! symphony-server: the observability HTTP server (axum 0.8).
//!
//! * **Elixir-compatible JSON API**: `GET /api/v1/state`, `POST /api/v1/refresh`,
//!   `GET /api/v1/{issue_identifier}` and the JSON 404/405 envelopes, byte-for-byte in shape.
//! * **New endpoints**: `GET /api/v1/health`, `GET /api/v1/events` (Server-Sent Events, replacing
//!   the Phoenix LiveView socket), run history from `symphony-store` (`/api/v1/runs`,
//!   `/api/v1/runs/{id}`, `/api/v1/runs/{id}/events`, `/api/v1/totals`) and the contract itself
//!   at `GET /api/openapi.json`.
//! * **Embedded dashboard**: the Vite bundle in `web/dist`, embedded at compile time (a
//!   placeholder page when it was not built; see [`WEB_UI_EMBEDDED`]).
//!
//! The contract is `docs/api/openapi.yaml`; `docs/api/README.md` is the human guide.
//!
//! The server does not depend on the runtime. The binary implements [`ControlPlane`] for its
//! orchestrator handle and calls [`serve`]:
//!
//! ```no_run
//! # async fn demo(control: std::sync::Arc<dyn symphony_server::ControlPlane>) -> Result<(), symphony_server::ServerError> {
//! use symphony_server::{ServerConfig, serve};
//! use tokio_util::sync::CancellationToken;
//!
//! let shutdown = CancellationToken::new();
//! let server = serve(
//!     ServerConfig::new("127.0.0.1", 0),
//!     control,
//!     symphony_store::Store::disabled(),
//!     shutdown.clone(),
//! )
//! .await?;
//! println!("dashboard: http://{}/", server.local_addr());
//! // ... later
//! shutdown.cancel();
//! server.wait().await?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod api;
mod app;
mod assets;
mod config;
mod control;
mod error;
mod history;
pub mod presenter;
mod server;
mod sse;
pub mod testing;
pub mod view;

pub use api::OPENAPI_JSON;
pub use app::{App, router};
pub use assets::WEB_UI_EMBEDDED;
pub use config::{
    DEFAULT_HOST, DEFAULT_MAX_SSE_CLIENTS, DEFAULT_REFRESH_TIMEOUT, DEFAULT_REQUEST_TIMEOUT,
    DEFAULT_SHUTDOWN_GRACE, DEFAULT_SNAPSHOT_TIMEOUT, DEFAULT_SSE_DEBOUNCE, DEFAULT_SSE_HEARTBEAT,
    SSE_RETRY, ServerConfig,
};
pub use control::{ControlPlane, StateError, Unavailable};
pub use error::ServerError;
pub use server::{BoundServer, resolve_host, serve};
pub use view::{
    BlockedEntry, CodexTotals, IssueView, RefreshAccepted, RetryEntry, RunningEntry, StatePayload,
    StateView, TokenCounts,
};
