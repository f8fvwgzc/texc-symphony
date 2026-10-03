//! Run history endpoints backed by `symphony-store` (new in the Rust port).

use std::collections::HashMap;

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use serde::Serialize;
use symphony_store::{RunEvent, RunId, RunPage, RunQuery, RunRecord, RunStatus, TotalsRecord};

use crate::app::AppState;
use crate::error::ApiError;

/// Default and maximum page size of `GET /api/v1/runs`.
const RUNS_LIMIT: (u32, u32) = (50, 200);
/// Default and maximum page size of `GET /api/v1/runs/{id}/events`.
const EVENTS_LIMIT: (u32, u32) = (500, 1000);

/// Body of `GET /api/v1/runs/{id}/events`.
#[derive(Debug, Serialize)]
pub(crate) struct RunEventList {
    events: Vec<RunEvent>,
}

/// `GET /api/v1/runs?limit&before_id&issue&status`.
pub(crate) async fn list_runs(
    State(app): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Json<RunPage>, ApiError> {
    let params = parse_query(query.as_deref());
    let limit = limit_param(&params, RUNS_LIMIT)?;
    let before_id = match params.get("before_id") {
        None => None,
        Some(raw) => Some(
            raw.parse::<i64>()
                .ok()
                .filter(|id| *id >= 1)
                .ok_or_else(|| invalid("before_id must be a positive integer"))?,
        ),
    };
    let issue_identifier = match params.get("issue") {
        None => None,
        Some(raw) if raw.is_empty() => return Err(invalid("issue must not be empty")),
        Some(raw) => Some(raw.clone()),
    };
    let status = match params.get("status") {
        None => None,
        Some(raw) => Some(raw.parse::<RunStatus>().map_err(|_| {
            invalid("status must be one of running, succeeded, failed, cancelled, blocked")
        })?),
    };
    let store = enabled_store(&app)?;
    let page = store
        .list_runs(RunQuery {
            limit: Some(limit),
            before_id,
            issue_identifier,
            status,
        })
        .await?;
    Ok(Json(page))
}

/// `GET /api/v1/runs/{id}`.
pub(crate) async fn get_run(
    State(app): State<AppState>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Json<RunRecord>, ApiError> {
    let id = run_id(id)?;
    let store = enabled_store(&app)?;
    store
        .get_run(id)
        .await?
        .map(Json)
        .ok_or(ApiError::RunNotFound)
}

/// `GET /api/v1/runs/{id}/events?after_seq&limit`.
pub(crate) async fn list_run_events(
    State(app): State<AppState>,
    id: Result<Path<String>, PathRejection>,
    RawQuery(query): RawQuery,
) -> Result<Json<RunEventList>, ApiError> {
    let id = run_id(id)?;
    let params = parse_query(query.as_deref());
    let after_seq = match params.get("after_seq") {
        None => 0,
        Some(raw) => raw
            .parse::<i64>()
            .ok()
            .filter(|seq| *seq >= 0)
            .ok_or_else(|| invalid("after_seq must be a non-negative integer"))?,
    };
    let limit = limit_param(&params, EVENTS_LIMIT)?;
    let store = enabled_store(&app)?;
    // Both queries are queued back to back on the store thread.
    let (run, events) = tokio::join!(
        store.get_run(id),
        store.list_events(id, Some(after_seq), Some(limit))
    );
    if run?.is_none() {
        return Err(ApiError::RunNotFound);
    }
    Ok(Json(RunEventList { events: events? }))
}

/// `GET /api/v1/totals`.
pub(crate) async fn totals(State(app): State<AppState>) -> Result<Json<TotalsRecord>, ApiError> {
    let store = enabled_store(&app)?;
    Ok(Json(store.totals().await?))
}

fn enabled_store(app: &AppState) -> Result<&symphony_store::Store, ApiError> {
    if app.store.is_enabled() {
        Ok(&app.store)
    } else {
        Err(ApiError::StoreDisabled)
    }
}

fn invalid(message: &str) -> ApiError {
    ApiError::InvalidParameter(message.to_owned())
}

fn run_id(path: Result<Path<String>, PathRejection>) -> Result<RunId, ApiError> {
    path.ok()
        .and_then(|Path(raw)| raw.parse::<i64>().ok())
        .filter(|id| *id >= 1)
        .map(RunId)
        .ok_or_else(|| invalid("id must be a positive integer"))
}

/// `limit` within `1..=max`, `default` when absent.
fn limit_param(
    params: &HashMap<String, String>,
    (default, max): (u32, u32),
) -> Result<u32, ApiError> {
    match params.get("limit") {
        None => Ok(default),
        Some(raw) => raw
            .parse::<u32>()
            .ok()
            .filter(|limit| (1..=max).contains(limit))
            .ok_or_else(|| {
                ApiError::InvalidParameter(format!("limit must be an integer between 1 and {max}"))
            }),
    }
}

/// Decoded query parameters; the last occurrence of a name wins, unknown names are ignored.
fn parse_query(query: Option<&str>) -> HashMap<String, String> {
    query
        .map(|query| {
            form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing_decodes_and_keeps_the_last_value() {
        let params = parse_query(Some("issue=MT%2F42&limit=1&limit=7&x"));
        assert_eq!(params.get("issue").map(String::as_str), Some("MT/42"));
        assert_eq!(params.get("limit").map(String::as_str), Some("7"));
        assert_eq!(params.get("x").map(String::as_str), Some(""));
        assert!(parse_query(None).is_empty());
    }

    #[test]
    fn limits_are_validated() {
        let params = |v: &str| parse_query(Some(&format!("limit={v}")));
        assert_eq!(limit_param(&HashMap::new(), RUNS_LIMIT), Ok(50));
        assert_eq!(limit_param(&params("200"), RUNS_LIMIT), Ok(200));
        assert_eq!(limit_param(&params("1000"), EVENTS_LIMIT), Ok(1000));
        for bad in ["0", "201", "-1", "abc", "", "1.5"] {
            assert_eq!(
                limit_param(&params(bad), RUNS_LIMIT),
                Err(ApiError::InvalidParameter(
                    "limit must be an integer between 1 and 200".into()
                )),
                "{bad}"
            );
        }
    }
}
