//! Errors: HTTP error responses (the JSON envelope) and server startup/runtime errors.

use std::io;
use std::net::SocketAddr;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use symphony_store::StoreError;

use crate::view::ErrorEnvelope;

/// Failure to start or run the HTTP server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// `server.host` is neither an IP literal nor a resolvable name.
    #[error("invalid_host: {host}: {reason}")]
    InvalidHost {
        /// The configured host.
        host: String,
        /// Resolver error.
        reason: String,
    },
    /// The listener could not be bound.
    #[error("bind_failed: {addr}: {source}")]
    Bind {
        /// Address that was being bound.
        addr: SocketAddr,
        /// OS error.
        source: io::Error,
    },
    /// The accept loop failed.
    #[error("server_failed: {0}")]
    Io(#[from] io::Error),
    /// The server task panicked or was aborted.
    #[error("server_task_failed: {0}")]
    Task(String),
}

/// An error answered with `{"error": {"code", "message"}}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApiError {
    /// 404 `not_found`.
    NotFound,
    /// 405 `method_not_allowed`.
    MethodNotAllowed,
    /// 404 `issue_not_found`.
    IssueNotFound,
    /// 503 `orchestrator_unavailable`.
    OrchestratorUnavailable,
    /// 404 `run_not_found`.
    RunNotFound,
    /// 400 `invalid_parameter` with a message naming the parameter.
    InvalidParameter(String),
    /// 503 `store_disabled`.
    StoreDisabled,
    /// `request_failed` with the given status and message.
    RequestFailed(StatusCode, String),
}

impl ApiError {
    /// `request_failed` with the canonical reason phrase of `status` as message.
    pub(crate) fn request_failed(status: StatusCode) -> Self {
        ApiError::RequestFailed(
            status,
            status
                .canonical_reason()
                .unwrap_or("Request failed")
                .to_owned(),
        )
    }

    pub(crate) fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound | ApiError::IssueNotFound | ApiError::RunNotFound => {
                StatusCode::NOT_FOUND
            }
            ApiError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            ApiError::OrchestratorUnavailable | ApiError::StoreDisabled => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ApiError::InvalidParameter(_) => StatusCode::BAD_REQUEST,
            ApiError::RequestFailed(status, _) => *status,
        }
    }

    pub(crate) fn envelope(&self) -> ErrorEnvelope {
        match self {
            ApiError::NotFound => ErrorEnvelope::new("not_found", "Route not found"),
            ApiError::MethodNotAllowed => {
                ErrorEnvelope::new("method_not_allowed", "Method not allowed")
            }
            ApiError::IssueNotFound => ErrorEnvelope::new("issue_not_found", "Issue not found"),
            ApiError::OrchestratorUnavailable => {
                ErrorEnvelope::new("orchestrator_unavailable", "Orchestrator is unavailable")
            }
            ApiError::RunNotFound => ErrorEnvelope::new("run_not_found", "Run not found"),
            ApiError::InvalidParameter(message) => {
                ErrorEnvelope::new("invalid_parameter", message.clone())
            }
            ApiError::StoreDisabled => {
                ErrorEnvelope::new("store_disabled", "Run history store is disabled")
            }
            ApiError::RequestFailed(_, message) => {
                ErrorEnvelope::new("request_failed", message.clone())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(self.envelope())).into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::RunNotFound(_) => ApiError::RunNotFound,
            StoreError::InvalidArgument(message) => ApiError::InvalidParameter(message),
            StoreError::Unavailable => {
                tracing::warn!(error = %err, "run history store unavailable");
                ApiError::request_failed(StatusCode::SERVICE_UNAVAILABLE)
            }
            other => {
                tracing::error!(error = %other, "run history query failed");
                ApiError::request_failed(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

/// Handler for paths that exist but not for this method.
pub(crate) async fn method_not_allowed() -> ApiError {
    ApiError::MethodNotAllowed
}
