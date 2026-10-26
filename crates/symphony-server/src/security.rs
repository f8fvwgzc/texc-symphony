//! Request security: API-token authentication, the `Host` allow-list (DNS rebinding), the
//! `Origin` check for writes (CSRF), and single-use tickets for `EventSource` streams.
//!
//! * **Authentication** applies to `/api/*` except `GET /api/v1/health` and
//!   `GET /api/openapi.json`. Clients send `Authorization: Bearer <token>`; the built-in
//!   [`StaticToken`] compares SHA-256 digests in constant time. Other schemes (OIDC/JWT) plug in
//!   through [`Authenticator`]. Without an authenticator the API is open, which [`crate::serve`]
//!   only allows on a loopback address.
//! * **Tickets**: a browser `EventSource` cannot send headers, so an authenticated client first
//!   asks `POST /api/v1/events/tickets` for a ticket and opens the stream with `?ticket=...`.
//!   A ticket is valid once, for [`TICKET_TTL`].
//! * **Host**: requests whose `Host` names something other than loopback, the bind address or a
//!   configured name are refused, so a page on another site that rebinds its DNS name to this
//!   address cannot read the API. Requests without any `Host` (not a browser) pass.
//! * **Origin**: browser writes (`POST`, ...) must come from the same origin or a configured one.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, HOST, ORIGIN, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, HeaderValue, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::app::{AppState, is_stream_path};
use crate::error::ApiError;

/// How long an SSE ticket stays redeemable.
pub const TICKET_TTL: Duration = Duration::from_secs(60);
/// Outstanding tickets kept at most; the oldest is dropped beyond this.
pub const MAX_TICKETS: usize = 1024;

/// Verifies the bearer credential of an API request.
///
/// [`StaticToken`] is the built-in implementation; an OIDC/JWT verifier can implement this trait
/// and be passed as [`Auth::custom`].
#[async_trait]
pub trait Authenticator: Send + Sync + 'static {
    /// `true` when `bearer` (the value after `Authorization: Bearer `) is valid.
    async fn verify(&self, bearer: &str) -> bool;
}

/// A fixed API token (`server.api_token`). Only its SHA-256 digest is kept.
pub struct StaticToken {
    digest: [u8; 32],
}

impl StaticToken {
    /// The token `token`.
    pub fn new(token: &str) -> Self {
        Self {
            digest: sha256(token.as_bytes()),
        }
    }
}

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken(<redacted>)")
    }
}

#[async_trait]
impl Authenticator for StaticToken {
    async fn verify(&self, bearer: &str) -> bool {
        constant_time_eq(&self.digest, &sha256(bearer.as_bytes()))
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Compares two digests without an early exit, so timing does not reveal a matching prefix
/// (comparing digests also hides the token length).
fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let diff = a
        .iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// The authenticator of a server, or none (open API).
#[derive(Clone, Default)]
pub struct Auth(Option<Arc<dyn Authenticator>>);

impl Auth {
    /// No authentication (allowed only on loopback binds).
    pub fn disabled() -> Self {
        Self(None)
    }

    /// A fixed bearer token.
    pub fn token(token: &str) -> Self {
        Self(Some(Arc::new(StaticToken::new(token))))
    }

    /// Any other [`Authenticator`].
    pub fn custom(authenticator: Arc<dyn Authenticator>) -> Self {
        Self(Some(authenticator))
    }

    /// `true` when requests must authenticate.
    pub fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    async fn verify(&self, bearer: Option<&str>) -> bool {
        match (&self.0, bearer) {
            (None, _) => true,
            (Some(auth), Some(bearer)) => auth.verify(bearer).await,
            (Some(_), None) => false,
        }
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_enabled() {
            "Auth(enabled)"
        } else {
            "Auth(disabled)"
        })
    }
}

impl PartialEq for Auth {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Eq for Auth {}

/// Which `Host` names the server answers.
#[derive(Debug, Clone)]
pub(crate) struct HostPolicy {
    any: bool,
    names: HashSet<String>,
}

impl HostPolicy {
    /// Loopback names, the bind host (unless it is a wildcard address) and `extra` (`*` = any).
    pub(crate) fn new(bind_host: &str, extra: &[String]) -> Self {
        let mut names: HashSet<String> = ["localhost", "127.0.0.1", "::1"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let bind = normalize_host(bind_host);
        if !matches!(bind.as_str(), "" | "0.0.0.0" | "::") {
            names.insert(bind);
        }
        let mut any = false;
        for host in extra {
            let host = normalize_host(host);
            if host == "*" {
                any = true;
            } else if !host.is_empty() {
                names.insert(host);
            }
        }
        Self { any, names }
    }

    /// `true` for an accepted `host[:port]` (and for a request without one).
    pub(crate) fn allows(&self, authority: Option<&str>) -> bool {
        let Some(authority) = authority else {
            return true;
        };
        if self.any {
            return true;
        }
        let host = host_of(authority);
        self.names.contains(&host) || host.ends_with(".localhost")
    }
}

/// Lowercase host without brackets and trailing dot.
fn normalize_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.trim_end_matches('.').to_owned()
}

/// The host part of `host[:port]` / `[v6]:port`.
fn host_of(authority: &str) -> String {
    let authority = authority.trim();
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or_default()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, port)| {
                if port.bytes().all(|b| b.is_ascii_digit()) {
                    host
                } else {
                    authority
                }
            })
    };
    normalize_host(host)
}

/// The request's authority: the `Host` header, else the URI authority (HTTP/2).
fn request_authority(request: &Request) -> Option<String> {
    request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| request.uri().authority().map(|a| a.as_str().to_owned()))
}

/// `true` when a browser write is acceptable: no `Origin` (not a browser), the same origin as
/// the request's `Host`, or a configured origin. `Sec-Fetch-Site: cross-site` without a
/// configured origin is refused even when `Origin` is missing.
pub(crate) fn origin_allowed(
    headers: &HeaderMap,
    authority: Option<&str>,
    allowed: &[String],
) -> bool {
    let configured = |origin: &str| {
        allowed
            .iter()
            .any(|a| a == "*" || a.eq_ignore_ascii_case(origin))
    };
    let origin = headers.get(ORIGIN).and_then(|v| v.to_str().ok());
    let cross_site = headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("cross-site"));
    let Some(origin) = origin.map(|o| o.trim().trim_end_matches('/')) else {
        return !cross_site || allowed.iter().any(|a| a == "*");
    };
    if configured(origin) {
        return true;
    }
    if origin == "null" || cross_site {
        return false;
    }
    let Some((scheme, origin_authority)) = origin.split_once("://") else {
        return false;
    };
    matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        && authority.is_some_and(|a| a.trim().eq_ignore_ascii_case(origin_authority))
}

fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Paths reachable without authentication.
fn is_public(path: &str) -> bool {
    !path.starts_with("/api/") || path == "/api/v1/health" || path == "/api/openapi.json"
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

fn ticket_param(request: &Request) -> Option<String> {
    let query = request.uri().query()?;
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "ticket")
        .map(|(_, value)| value.into_owned())
}

fn unauthorized() -> Response {
    let mut response = ApiError::Unauthorized.into_response();
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"symphony\""),
    );
    response
}

/// Host, Origin and authentication checks (see the module docs), in that order.
pub(crate) async fn guard(State(app): State<AppState>, request: Request, next: Next) -> Response {
    let authority = request_authority(&request);
    if !app.hosts.allows(authority.as_deref()) {
        tracing::warn!(host = ?authority, "refusing request for a host that is not allowed");
        return ApiError::HostNotAllowed.into_response();
    }
    if !is_safe(request.method())
        && !origin_allowed(
            request.headers(),
            authority.as_deref(),
            &app.config.cors_allowed_origins,
        )
    {
        tracing::warn!(
            origin = ?request.headers().get(ORIGIN),
            path = request.uri().path(),
            "refusing a cross-origin write"
        );
        return ApiError::OriginNotAllowed.into_response();
    }
    let path = request.uri().path();
    if !app.config.auth.is_enabled() || is_public(path) || request.method() == Method::OPTIONS {
        return next.run(request).await;
    }
    if request.method() == Method::GET
        && is_stream_path(path)
        && let Some(ticket) = ticket_param(&request)
    {
        if app.tickets.redeem(&ticket) {
            return next.run(request).await;
        }
        return unauthorized();
    }
    if app.config.auth.verify(bearer(request.headers())).await {
        next.run(request).await
    } else {
        unauthorized()
    }
}

/// Outstanding SSE tickets (single use, [`TICKET_TTL`]).
#[derive(Debug, Default)]
pub(crate) struct TicketBook {
    tickets: Mutex<HashMap<String, Instant>>,
}

impl TicketBook {
    /// A fresh ticket (256 random bits, hex) and its lifetime.
    pub(crate) fn issue(&self) -> Result<(String, Duration), ApiError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|err| {
            tracing::error!("cannot generate an SSE ticket: {err}");
            ApiError::request_failed(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
        })?;
        let ticket = hex::encode(bytes);
        let now = Instant::now();
        let mut tickets = self.lock();
        tickets.retain(|_, expires| *expires > now);
        if tickets.len() >= MAX_TICKETS
            && let Some(oldest) = tickets
                .iter()
                .min_by_key(|(_, expires)| **expires)
                .map(|(t, _)| t.clone())
        {
            tickets.remove(&oldest);
        }
        tickets.insert(ticket.clone(), now + TICKET_TTL);
        Ok((ticket, TICKET_TTL))
    }

    /// Consumes `ticket`; `true` when it existed and had not expired.
    pub(crate) fn redeem(&self, ticket: &str) -> bool {
        self.lock()
            .remove(ticket)
            .is_some_and(|expires| expires > Instant::now())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Instant>> {
        // A poisoned lock only means another request panicked mid-update; the map is still valid.
        self.tickets.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_include_loopback_bind_host_and_extras() {
        let policy = HostPolicy::new("10.0.0.5", &["Symphony.Example".into()]);
        for ok in [
            "localhost:4000",
            "127.0.0.1",
            "[::1]:4000",
            "10.0.0.5:4000",
            "symphony.example",
            "SYMPHONY.EXAMPLE.:443",
            "dash.localhost:4000",
        ] {
            assert!(policy.allows(Some(ok)), "{ok}");
        }
        for bad in [
            "evil.example",
            "evil.example:4000",
            "10.0.0.6",
            "[::2]:4000",
        ] {
            assert!(!policy.allows(Some(bad)), "{bad}");
        }
        assert!(policy.allows(None));
        let wildcard = HostPolicy::new("0.0.0.0", &[]);
        assert!(!wildcard.allows(Some("0.0.0.0:4000")));
        assert!(HostPolicy::new("0.0.0.0", &["*".into()]).allows(Some("anything")));
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn origins_must_match_the_host_or_the_allow_list() {
        let host = Some("127.0.0.1:4000");
        assert!(origin_allowed(&headers(&[]), host, &[]));
        assert!(origin_allowed(
            &headers(&[("origin", "http://127.0.0.1:4000")]),
            host,
            &[]
        ));
        assert!(!origin_allowed(
            &headers(&[("origin", "https://evil.example")]),
            host,
            &[]
        ));
        assert!(!origin_allowed(&headers(&[("origin", "null")]), host, &[]));
        assert!(origin_allowed(
            &headers(&[("origin", "https://dash.example")]),
            host,
            &["https://dash.example".into()]
        ));
        assert!(!origin_allowed(
            &headers(&[("sec-fetch-site", "cross-site")]),
            host,
            &[]
        ));
        assert!(!origin_allowed(
            &headers(&[
                ("origin", "http://127.0.0.1:4000"),
                ("sec-fetch-site", "cross-site")
            ]),
            host,
            &[]
        ));
    }

    #[tokio::test]
    async fn static_tokens_compare_digests() {
        let token = StaticToken::new("correct horse");
        assert!(token.verify("correct horse").await);
        assert!(!token.verify("correct hors").await);
        assert!(!token.verify("").await);
        assert!(!format!("{token:?}").contains("horse"));
        assert!(Auth::token("x").is_enabled());
        assert!(Auth::disabled().verify(None).await);
        assert!(!Auth::token("x").verify(None).await);
    }

    #[test]
    fn bearer_values_are_parsed_case_insensitively() {
        assert_eq!(
            bearer(&headers(&[("authorization", "Bearer abc")])),
            Some("abc")
        );
        assert_eq!(
            bearer(&headers(&[("authorization", "bearer  abc ")])),
            Some("abc")
        );
        assert_eq!(bearer(&headers(&[("authorization", "Basic abc")])), None);
        assert_eq!(bearer(&headers(&[("authorization", "Bearer ")])), None);
    }

    #[tokio::test(start_paused = true)]
    async fn tickets_are_single_use_and_expire() {
        let book = TicketBook::default();
        let (ticket, ttl) = book.issue().unwrap();
        assert_eq!(ticket.len(), 64);
        assert_eq!(ttl, TICKET_TTL);
        assert!(book.redeem(&ticket));
        assert!(!book.redeem(&ticket));
        let (late, _) = book.issue().unwrap();
        tokio::time::advance(TICKET_TTL + Duration::from_millis(1)).await;
        assert!(!book.redeem(&late));
        assert!(!book.redeem("made-up"));
    }

    #[tokio::test]
    async fn the_ticket_book_is_bounded() {
        let book = TicketBook::default();
        let first = book.issue().unwrap().0;
        for _ in 0..MAX_TICKETS {
            book.issue().unwrap();
        }
        assert_eq!(book.lock().len(), MAX_TICKETS);
        assert!(!book.redeem(&first));
    }
}
