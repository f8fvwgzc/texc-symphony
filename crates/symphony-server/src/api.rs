//! Live-state handlers: `state`, `refresh` and `{issue_identifier}` (Elixir compatible), plus
//! `health` and `openapi.json`.

// Modified 2026-10-25: SSE tickets, kill switch and Prometheus metrics handlers (audit A1/A4).

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;

use crate::app::AppState;
use crate::error::ApiError;
use crate::view::{
    Health, IssueView, PauseState, RefreshAccepted, SseTicket, StatePayload, StoreMode,
};

/// The API contract (`docs/api/openapi.yaml`) as JSON, converted at build time.
pub const OPENAPI_JSON: &str = include_str!(concat!(env!("OUT_DIR"), "/openapi.json"));

/// `GET /api/v1/state`: always 200; snapshot failures are reported in the body.
pub(crate) async fn state(State(app): State<AppState>) -> Json<StatePayload> {
    Json(app.state_payload().await)
}

/// `POST /api/v1/refresh`: 202, or 503 when the orchestrator is down or does not answer within
/// `refresh_timeout` (Elixir crashed into a 500 on that timeout).
pub(crate) async fn refresh(
    State(app): State<AppState>,
) -> Result<(StatusCode, Json<RefreshAccepted>), ApiError> {
    match tokio::time::timeout(app.config.refresh_timeout, app.control.refresh()).await {
        Ok(Ok(accepted)) => Ok((StatusCode::ACCEPTED, Json(accepted))),
        Ok(Err(_)) => Err(ApiError::OrchestratorUnavailable),
        Err(_) => {
            tracing::warn!("refresh request timed out");
            Err(ApiError::OrchestratorUnavailable)
        }
    }
}

/// `POST /api/v1/pause`: engage the kill switch (stop every agent, hold dispatch). An optional
/// `reason` query parameter is stored and shown while paused.
pub(crate) async fn pause(
    State(app): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<PauseState>, ApiError> {
    let reason = query.as_deref().and_then(|q| {
        form_urlencoded::parse(q.as_bytes())
            .find(|(key, _)| key == "reason")
            .map(|(_, value)| value.chars().take(500).collect::<String>())
    });
    set_paused(&app, true, reason).await
}

/// `POST /api/v1/resume`: release the kill switch.
pub(crate) async fn resume(State(app): State<AppState>) -> Result<Json<PauseState>, ApiError> {
    set_paused(&app, false, None).await
}

async fn set_paused(
    app: &AppState,
    paused: bool,
    reason: Option<String>,
) -> Result<Json<PauseState>, ApiError> {
    match tokio::time::timeout(
        app.config.refresh_timeout,
        app.control.set_paused(paused, reason),
    )
    .await
    {
        Ok(Ok(state)) => Ok(Json(state)),
        Ok(Err(_)) | Err(_) => Err(ApiError::OrchestratorUnavailable),
    }
}

/// `GET /api/v1/metrics`: Prometheus text exposition of the live state and spend.
pub(crate) async fn metrics(State(app): State<AppState>) -> impl IntoResponse {
    let body = match app.snapshot().await {
        Ok(view) => crate::presenter::prometheus(&view),
        Err(_) => "# orchestrator snapshot unavailable\nsymphony_up 0\n".to_owned(),
    };
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
}

/// `GET /api/v1/{issue_identifier}`: 200, or 404 when the issue is unknown or the snapshot
/// failed. An undecodable identifier cannot name an issue, so it is a 404 as well.
pub(crate) async fn issue(
    State(app): State<AppState>,
    identifier: Result<Path<String>, PathRejection>,
) -> Result<Json<IssueView>, ApiError> {
    let Ok(Path(identifier)) = identifier else {
        return Err(ApiError::IssueNotFound);
    };
    match tokio::time::timeout(app.config.snapshot_timeout, app.control.issue(&identifier)).await {
        Ok(Some(issue)) => Ok(Json(issue)),
        Ok(None) | Err(_) => Err(ApiError::IssueNotFound),
    }
}

/// `GET /api/v1/health`: never touches the orchestrator.
pub(crate) async fn health(State(app): State<AppState>) -> Json<Health> {
    let store = if !app.store.is_enabled() {
        StoreMode::Disabled
    } else if app.store.is_postgres() {
        StoreMode::Postgres
    } else {
        StoreMode::Sqlite
    };
    Json(Health {
        // A lost database connection (PostgreSQL) shows as `degraded` in the body (the status
        // code stays 200, so this one endpoint serves both the liveness and readiness probes);
        // operators can alert on it rather than it always reading `ok` while it cannot dispatch.
        status: if app.store.is_degraded() {
            "degraded"
        } else {
            "ok"
        }
        .to_owned(),
        version: app.config.version.clone(),
        uptime_seconds: app.started.elapsed().as_secs(),
        store,
        auth_required: app.config.auth.is_enabled(),
    })
}

/// `POST /api/v1/events/tickets`: a single-use ticket for opening an SSE stream with
/// `?ticket=...` (the request itself is authenticated like any other API call).
pub(crate) async fn issue_ticket(
    State(app): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let (ticket, ttl) = app.tickets.issue()?;
    let expires_at = chrono::Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default();
    // The ticket travels only in a cookie, never in a URL or a body a script could read.
    let cookie = crate::security::ticket_set_cookie(
        &ticket,
        ttl,
        crate::security::authority_of(&headers).as_deref(),
    );
    Ok((
        StatusCode::CREATED,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(SseTicket {
            expires_at,
            expires_in_ms: u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX),
        }),
    ))
}

/// `GET /api/openapi.json`.
pub(crate) async fn openapi() -> impl IntoResponse {
    ([(CONTENT_TYPE, "application/json")], OPENAPI_JSON)
}
