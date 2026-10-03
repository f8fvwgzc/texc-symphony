//! Router assembly, shared state and cross-cutting middleware.

use std::any::Any;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{
    CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use symphony_store::Store;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tower::{Layer, ServiceBuilder};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, NotForContentType, Predicate};
use tower_http::cors::{AllowOrigin, Any as AnyOrigin, CorsLayer};
use tower_http::normalize_path::{NormalizePath, NormalizePathLayer};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::config::ServerConfig;
use crate::control::{ControlPlane, StateError};
use crate::error::{ApiError, method_not_allowed};
use crate::sse::SseHub;
use crate::view::{StatePayload, StateView};
use crate::{api, assets, history, presenter, sse};

/// Path of the SSE endpoint (excluded from the request timeout and from compression).
pub(crate) const EVENTS_PATH: &str = "/api/v1/events";

/// Content-Security-Policy for every response. The dashboard is a same-origin Vite bundle with
/// no inline scripts; inline styles stay allowed for component libraries that set them.
const CSP: &str = "default-src 'self'; base-uri 'self'; frame-ancestors 'self'; object-src 'none'; \
img-src 'self' data:; style-src 'self' 'unsafe-inline'; script-src 'self'; connect-src 'self'; \
form-action 'self'";

/// The complete HTTP application: the axum [`Router`] behind trailing-slash normalization
/// (`/api/v1/state/` is `/api/v1/state`, as with Phoenix). A `tower::Service` that can be
/// driven directly (`oneshot`) or served with [`serve`](crate::serve).
pub type App = NormalizePath<Router>;

/// State shared by every handler (cheap to clone).
#[derive(Clone)]
pub(crate) struct AppState(Arc<Shared>);

pub(crate) struct Shared {
    pub(crate) control: Arc<dyn ControlPlane>,
    pub(crate) store: Store,
    pub(crate) config: ServerConfig,
    pub(crate) started: tokio::time::Instant,
    pub(crate) hub: SseHub,
    pub(crate) sse_slots: Option<Arc<Semaphore>>,
    pub(crate) shutdown: CancellationToken,
}

impl std::ops::Deref for AppState {
    type Target = Shared;

    fn deref(&self) -> &Shared {
        &self.0
    }
}

impl AppState {
    /// A snapshot bounded by `snapshot_timeout` (elapsed means [`StateError::Timeout`]).
    pub(crate) async fn snapshot(&self) -> Result<StateView, StateError> {
        tokio::time::timeout(self.config.snapshot_timeout, self.control.state())
            .await
            .unwrap_or(Err(StateError::Timeout))
    }

    /// The body of `GET /api/v1/state` (and of SSE `snapshot` events), stamped now.
    pub(crate) async fn state_payload(&self) -> StatePayload {
        let generated_at = Utc::now();
        presenter::state_payload(generated_at, self.snapshot().await)
    }
}

/// Build the application without binding a socket.
///
/// * `store` may be [`Store::disabled`]: the history endpoints then answer `503 store_disabled`.
/// * Cancelling `shutdown` ends every open SSE stream (needed for graceful shutdown).
///
/// Must be called inside a tokio runtime only when requests are driven (the SSE hub task is
/// spawned lazily on the first stream).
pub fn router(
    config: &ServerConfig,
    control: Arc<dyn ControlPlane>,
    store: Store,
    shutdown: CancellationToken,
) -> App {
    let sse_slots =
        (config.max_sse_clients > 0).then(|| Arc::new(Semaphore::new(config.max_sse_clients)));
    let state = AppState(Arc::new(Shared {
        control,
        store,
        config: config.clone(),
        started: tokio::time::Instant::now(),
        hub: SseHub::new(),
        sse_slots,
        shutdown,
    }));

    let routes = Router::new()
        .route("/", get(assets::index).fallback(method_not_allowed))
        .route(
            "/api/v1/state",
            get(api::state).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/refresh",
            post(api::refresh).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/health",
            get(api::health).fallback(method_not_allowed),
        )
        .route(EVENTS_PATH, get(sse::events).fallback(method_not_allowed))
        .route(
            "/api/v1/runs",
            get(history::list_runs).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/totals",
            get(history::totals).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/runs/{id}",
            get(history::get_run).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/runs/{id}/events",
            get(history::list_run_events).fallback(method_not_allowed),
        )
        .route(
            "/api/v1/{issue_identifier}",
            get(api::issue).fallback(method_not_allowed),
        )
        .route(
            "/api/openapi.json",
            get(api::openapi).fallback(method_not_allowed),
        )
        .fallback(assets::fallback);

    let mut routes = routes
        .layer(CompressionLayer::new().compress_when(
            DefaultPredicate::new().and(NotForContentType::const_new("text/event-stream")),
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_timeout,
        ));
    if let Some(cors) = cors_layer(&config.cors_allowed_origins) {
        routes = routes.layer(cors);
    }
    let routes = routes
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
                .layer(TraceLayer::new_for_http())
                .layer(PropagateRequestIdLayer::x_request_id())
                .layer(CatchPanicLayer::custom(panic_response))
                .layer(header(X_CONTENT_TYPE_OPTIONS, "nosniff"))
                .layer(header(X_FRAME_OPTIONS, "SAMEORIGIN"))
                .layer(header(REFERRER_POLICY, "strict-origin-when-cross-origin"))
                .layer(header(
                    HeaderName::from_static("x-permitted-cross-domain-policies"),
                    "none",
                ))
                .layer(header(CONTENT_SECURITY_POLICY, CSP)),
        )
        .with_state(state);

    NormalizePathLayer::trim_trailing_slash().layer(routes)
}

fn header(name: HeaderName, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::if_not_present(name, HeaderValue::from_static(value))
}

/// CORS for the configured origins; `None` (no CORS headers at all) when the list is empty.
fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let allow_origin = if origins.iter().any(|origin| origin.trim() == "*") {
        AllowOrigin::from(AnyOrigin)
    } else {
        let values: Vec<HeaderValue> = origins
            .iter()
            .filter_map(|origin| match HeaderValue::from_str(origin.trim()) {
                Ok(value) => Some(value),
                Err(_) => {
                    tracing::warn!(%origin, "ignoring invalid CORS origin");
                    None
                }
            })
            .collect();
        AllowOrigin::list(values)
    };
    Some(
        CorsLayer::new()
            .allow_origin(allow_origin)
            .allow_methods([Method::GET, Method::HEAD, Method::POST])
            .allow_headers([
                axum::http::header::CONTENT_TYPE,
                HeaderName::from_static("last-event-id"),
            ]),
    )
}

/// Bound every non-streaming request by `request_timeout` (`503 request_failed` when elapsed).
async fn request_timeout(State(app): State<AppState>, request: Request, next: Next) -> Response {
    if request.uri().path() == EVENTS_PATH {
        return next.run(request).await;
    }
    match tokio::time::timeout(app.config.request_timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(
                timeout_ms = app.config.request_timeout.as_millis(),
                "request timed out"
            );
            ApiError::request_failed(StatusCode::SERVICE_UNAVAILABLE).into_response()
        }
    }
}

/// A handler panic becomes `500 request_failed` (Phoenix `render_errors` parity) instead of a
/// dropped connection.
fn panic_response(panic: Box<dyn Any + Send + 'static>) -> Response<Body> {
    let detail = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    tracing::error!(panic = detail, "request handler panicked");
    ApiError::request_failed(StatusCode::INTERNAL_SERVER_ERROR).into_response()
}
