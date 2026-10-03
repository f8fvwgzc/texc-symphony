//! Live-state handlers: `state`, `refresh` and `{issue_identifier}` (Elixir compatible), plus
//! `health` and `openapi.json`.

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;

use crate::app::AppState;
use crate::error::ApiError;
use crate::view::{Health, IssueView, RefreshAccepted, StatePayload, StoreMode};

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
    Json(Health {
        status: "ok".to_owned(),
        version: app.config.version.clone(),
        uptime_seconds: app.started.elapsed().as_secs(),
        store: if app.store.is_enabled() {
            StoreMode::Sqlite
        } else {
            StoreMode::Disabled
        },
    })
}

/// `GET /api/openapi.json`.
pub(crate) async fn openapi() -> impl IntoResponse {
    ([(CONTENT_TYPE, "application/json")], OPENAPI_JSON)
}
