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
    /// A non-loopback address was requested without API authentication.
    #[error(
        "auth_required: refusing to serve the API on {addr} without an API token; set server.api_token (or SYMPHONY_API_TOKEN), or bind 127.0.0.1"
    )]
    AuthRequired {
        /// The requested address.
        addr: SocketAddr,
    },
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
    /// 401 `unauthorized`: missing or invalid API token / SSE ticket.
    Unauthorized,
    /// 403 `host_not_allowed`: the `Host` header names another site (DNS rebinding).
    HostNotAllowed,
    /// 403 `origin_not_allowed`: a browser write from another origin.
    OriginNotAllowed,
    /// 404 `task_not_found`.
    TaskNotFound,
    /// 409 `task_conflict`: the task is not in a state that allows the action.
    Conflict(String),
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
            ApiError::NotFound
            | ApiError::IssueNotFound
            | ApiError::RunNotFound
            | ApiError::TaskNotFound => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            ApiError::OrchestratorUnavailable | ApiError::StoreDisabled => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ApiError::InvalidParameter(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::HostNotAllowed | ApiError::OriginNotAllowed => StatusCode::FORBIDDEN,
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
            ApiError::Unauthorized => {
                ErrorEnvelope::new("unauthorized", "Missing or invalid API token")
            }
            ApiError::HostNotAllowed => {
                ErrorEnvelope::new("host_not_allowed", "Host header is not allowed")
            }
            ApiError::OriginNotAllowed => {
                ErrorEnvelope::new("origin_not_allowed", "Cross-origin request refused")
            }
            ApiError::TaskNotFound => ErrorEnvelope::new("task_not_found", "Task not found"),
            ApiError::Conflict(message) => ErrorEnvelope::new("task_conflict", message.clone()),
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
            StoreError::Conflict(message) => ApiError::Conflict(message),
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
