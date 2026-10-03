//! HTTP transport with the observable behaviour of Elixir's `Req 0.5.17` defaults.
//!
//! The adapters never talk to `reqwest` directly. They build an [`HttpRequest`] and hand it to an
//! [`HttpClient`], which owns the policy:
//!
//! - **Timeouts:** connect 30 s (explicit in Elixir), receive/read 15 s (Req default).
//! - **Retries** (Req `retry: :safe_transient`): only `GET`/`HEAD`, on status 408/429/500/502/503/504
//!   or a transient transport error (timeout, connection refused, connection closed); up to 3 retries
//!   with delays of 1 s, 2 s, 4 s, or the `Retry-After` header (seconds or HTTP date, capped at
//!   [`RetryPolicy::max_delay`]). After the last retry the final response is returned as-is.
//! - **Redirects:** followed manually (the reqwest client never follows them) up to 10 hops. A
//!   relative `Location` resolves against the current URL. When the target differs in scheme, host or
//!   port, credential headers (`Authorization`, `Private-Token`, `Cookie`, `Proxy-Authorization`) are
//!   dropped for the rest of the chain. 301/302/303 turn a non-`HEAD` request into a body-less `GET`;
//!   307/308 keep the method and body.
//! - **Body decoding:** JSON when `Content-Type` is `application/json` or `*/*+json` and the body is
//!   non-empty (invalid JSON is a transport error, as in Req); otherwise the body becomes a JSON
//!   string (`""` when empty). gzip/brotli bodies are decompressed by reqwest.
//! - **Secret scrubbing (new in Rust):** every [`TransportError`] message passes through
//!   [`scrub_secrets`] with the request's credential values, and request `Debug` output redacts header
//!   values. Elixir relied on "never log headers"; the Rust port enforces it.
//!
//! [`Transport`] is the single-shot seam: [`ReqwestTransport`] is the production implementation, and
//! tests can substitute their own.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use serde_json::Value;
use tracing::Instrument;
use url::Url;

/// HTTP methods used by the adapters and agent tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Method {
    /// `GET`
    Get,
    /// `HEAD`
    Head,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
}

impl Method {
    /// Uppercase method name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }

    /// Parses an exact uppercase method name.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "GET" => Self::Get,
            "HEAD" => Self::Head,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            _ => return None,
        })
    }

    /// `GET`/`HEAD`: the only methods Req's `:safe_transient` policy retries.
    pub fn is_safe(self) -> bool {
        matches!(self, Self::Get | Self::Head)
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A transport failure (no HTTP response, or a response that could not be decoded).
///
/// Messages never contain credentials: they are scrubbed when the error is created.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// Connect or receive timeout.
    #[error("timeout")]
    Timeout,
    /// The connection was refused.
    #[error("econnrefused")]
    ConnectionRefused,
    /// The connection was closed before a complete response arrived.
    #[error("closed")]
    Closed,
    /// More than the allowed number of redirects.
    #[error("too_many_redirects")]
    TooManyRedirects,
    /// The request URL (or a redirect target) is not a valid absolute URL.
    #[error("invalid_url: {0}")]
    InvalidUrl(String),
    /// A JSON content type with a body that is not valid JSON.
    #[error("invalid_json: {0}")]
    InvalidJson(String),
    /// Any other transport failure (scrubbed description).
    #[error("transport_error: {0}")]
    Other(String),
}

impl TransportError {
    /// Elixir-style `inspect/1` of the reason (`:timeout`, `{:transport_error, "..."}`).
    pub fn inspect(&self) -> String {
        use crate::error::inspect_string;
        match self {
            Self::Timeout => ":timeout".into(),
            Self::ConnectionRefused => ":econnrefused".into(),
            Self::Closed => ":closed".into(),
            Self::TooManyRedirects => ":too_many_redirects".into(),
            Self::InvalidUrl(url) => format!("{{:invalid_url, {}}}", inspect_string(url)),
            Self::InvalidJson(msg) => format!("{{:invalid_json, {}}}", inspect_string(msg)),
            Self::Other(msg) => format!("{{:transport_error, {}}}", inspect_string(msg)),
        }
    }

    /// Transient failures that Req retries for safe methods.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Timeout | Self::ConnectionRefused | Self::Closed)
    }

    fn scrubbed(self, secrets: &[String]) -> Self {
        match self {
            Self::InvalidUrl(m) => Self::InvalidUrl(scrub_secrets(&m, secrets)),
            Self::InvalidJson(m) => Self::InvalidJson(scrub_secrets(&m, secrets)),
            Self::Other(m) => Self::Other(scrub_secrets(&m, secrets)),
            other => other,
        }
    }
}

/// Replacement text for scrubbed secrets.
pub const REDACTED: &str = "[REDACTED]";

/// Removes credentials from `text`: every listed secret value (4+ characters), the token following
/// `Bearer `/`Basic `, and the user-info part of URLs (`https://user:pass@host` ->
/// `https://[REDACTED]@host`).
pub fn scrub_secrets(text: &str, secrets: &[String]) -> String {
    let mut out = text.to_owned();
    let mut sorted: Vec<&String> = secrets.iter().filter(|s| s.len() >= 4).collect();
    // Longest first so a secret that contains another one is removed whole.
    sorted.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in sorted {
        out = out.replace(secret.as_str(), REDACTED);
    }
    for scheme in ["Bearer ", "bearer ", "Basic ", "basic "] {
        out = redact_after(&out, scheme);
    }
    redact_userinfo(&out)
}

fn redact_after(text: &str, marker: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(marker) {
        let (head, tail) = rest.split_at(idx + marker.len());
        out.push_str(head);
        let token_len = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ',' | '}' | ')'))
            .unwrap_or(tail.len());
        if token_len > 0 && !tail.starts_with(REDACTED) {
            out.push_str(REDACTED);
        } else {
            out.push_str(&tail[..token_len]);
        }
        rest = &tail[token_len..];
    }
    out.push_str(rest);
    out
}

fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        let (head, tail) = rest.split_at(idx + 3);
        out.push_str(head);
        let authority_end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace() || c == '"')
            .unwrap_or(tail.len());
        let authority = &tail[..authority_end];
        match authority.rfind('@') {
            Some(at) if !authority[..at].is_empty() && &authority[..at] != REDACTED => {
                out.push_str(REDACTED);
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
        }
        rest = &tail[authority_end..];
    }
    out.push_str(rest);
    out
}

/// One fully-built HTTP exchange handed to a [`Transport`].
#[derive(Clone)]
pub struct RawRequest {
    /// Method.
    pub method: Method,
    /// Absolute URL (query included).
    pub url: Url,
    /// Header name/value pairs (names are case-insensitive).
    pub headers: Vec<(String, String)>,
    /// Request body bytes.
    pub body: Option<Vec<u8>>,
}

impl fmt::Debug for RawRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawRequest")
            .field("method", &self.method)
            .field("url", &self.url.as_str())
            .field("headers", &redacted_headers(&self.headers))
            .field("body_len", &self.body.as_ref().map(Vec::len))
            .finish()
    }
}

fn redacted_headers(headers: &[(String, String)]) -> Vec<(String, &'static str)> {
    headers
        .iter()
        .map(|(name, _)| (name.clone(), "<redacted>"))
        .collect()
}

/// A raw HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    /// Status code.
    pub status: u16,
    /// Header name/value pairs (names lowercase).
    pub headers: Vec<(String, String)>,
    /// Body bytes (already decompressed).
    pub body: Vec<u8>,
}

impl RawResponse {
    /// First header value with the given (case-insensitive) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        find_header(&self.headers, name)
    }
}

fn find_header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Single-shot HTTP exchange (no redirects, no retries). Implementations must not follow redirects.
#[async_trait]
pub trait Transport: Send + Sync + fmt::Debug {
    /// Sends one request and returns the raw response.
    async fn send(&self, request: RawRequest) -> Result<RawResponse, TransportError>;
}

/// Connect timeout (Elixir `connect_options: [timeout: 30_000]`).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Receive timeout (Req default `receive_timeout: 15_000`).
pub const RECEIVE_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum redirects followed (Req default `max_redirects: 10`).
pub const MAX_REDIRECTS: u32 = 10;

/// The production [`Transport`] on top of `reqwest` (rustls, gzip, brotli, redirects disabled).
#[derive(Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
    origin_overrides: Vec<(String, Url)>,
}

impl fmt::Debug for ReqwestTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReqwestTransport")
            .field("origin_overrides", &self.origin_overrides)
            .finish()
    }
}

impl ReqwestTransport {
    /// Builds the client with the Req-compatible timeouts.
    pub fn new() -> Result<Self, TransportError> {
        Self::with_timeouts(CONNECT_TIMEOUT, RECEIVE_TIMEOUT)
    }

    /// Builds the client with explicit connect and read timeouts.
    pub fn with_timeouts(connect: Duration, read: Duration) -> Result<Self, TransportError> {
        let client = reqwest::Client::builder()
            .connect_timeout(connect)
            .read_timeout(read)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| TransportError::Other(error_chain(&err)))?;
        Ok(Self {
            client,
            origin_overrides: Vec::new(),
        })
    }

    /// Sends requests addressed to `logical` (e.g. `https://gitlab.test`) to `actual` instead (e.g. a
    /// local mock server). Redirect and credential decisions keep using the logical URL; only the
    /// socket destination changes. Intended for tests and local fakes.
    pub fn with_origin_override(
        mut self,
        logical: &str,
        actual: &str,
    ) -> Result<Self, TransportError> {
        let logical_url =
            Url::parse(logical).map_err(|_| TransportError::InvalidUrl(logical.to_owned()))?;
        let actual_url =
            Url::parse(actual).map_err(|_| TransportError::InvalidUrl(actual.to_owned()))?;
        self.origin_overrides
            .push((logical_url.origin().ascii_serialization(), actual_url));
        Ok(self)
    }

    fn physical_url(&self, url: &Url) -> Url {
        let origin = url.origin().ascii_serialization();
        let Some((_, actual)) = self.origin_overrides.iter().find(|(o, _)| *o == origin) else {
            return url.clone();
        };
        let mut target = actual.clone();
        target.set_path(url.path());
        target.set_query(url.query());
        target
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn send(&self, request: RawRequest) -> Result<RawResponse, TransportError> {
        let method = match request.method {
            Method::Get => reqwest::Method::GET,
            Method::Head => reqwest::Method::HEAD,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        };
        let url = self.physical_url(&request.url);
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &request.headers {
            let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| TransportError::Other(format!("invalid header name {name}")))?;
            let mut header_value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|_| TransportError::Other(format!("invalid header value for {name}")))?;
            if is_credential_header(name) {
                header_value.set_sensitive(true);
            }
            headers.append(header_name, header_value);
        }
        let mut builder = self.client.request(method, url).headers(headers);
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let response = builder.send().await.map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let body = response.bytes().await.map_err(map_reqwest_error)?.to_vec();
        Ok(RawResponse {
            status,
            headers,
            body,
        })
    }
}

fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(inner) = source {
        let text = inner.to_string();
        if !parts.iter().any(|p| p.contains(&text)) {
            parts.push(text);
        }
        source = inner.source();
    }
    parts.join(": ")
}

fn map_reqwest_error(err: reqwest::Error) -> TransportError {
    let timeout = err.is_timeout();
    let connect = err.is_connect();
    let err = err.without_url();
    let chain = error_chain(&err);
    let lower = chain.to_ascii_lowercase();
    if timeout || lower.contains("timed out") {
        TransportError::Timeout
    } else if lower.contains("connection refused") {
        TransportError::ConnectionRefused
    } else if !connect
        && (lower.contains("connection closed")
            || lower.contains("incomplete")
            || lower.contains("connection reset")
            || lower.contains("broken pipe"))
    {
        TransportError::Closed
    } else {
        TransportError::Other(chain)
    }
}

fn is_credential_header(name: &str) -> bool {
    [
        "authorization",
        "private-token",
        "cookie",
        "proxy-authorization",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// Req's default retry schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt (Req: 3).
    pub max_retries: u32,
    /// First delay; doubled for every further retry (Req: 1 s, 2 s, 4 s).
    pub base_delay: Duration,
    /// Upper bound for a server-supplied `Retry-After` (new in Rust: Req had no cap).
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
        }
    }
}

const RETRYABLE_STATUSES: [u16; 6] = [408, 429, 500, 502, 503, 504];

/// A logical request (before redirects/retries).
#[derive(Clone)]
pub struct HttpRequest {
    /// Method.
    pub method: Method,
    /// Absolute URL; may already carry a query string (params are then appended with `&`).
    pub url: String,
    /// Query parameters, already stringified, in order.
    pub query: Vec<(String, String)>,
    /// Request headers.
    pub headers: Vec<(String, String)>,
    /// JSON body (adds `Content-Type: application/json`).
    pub json: Option<Value>,
    /// Credential values to scrub from error messages.
    pub secrets: Vec<String>,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field("query", &self.query)
            .field("headers", &redacted_headers(&self.headers))
            .field("has_body", &self.json.is_some())
            .finish()
    }
}

impl HttpRequest {
    /// A request without query, headers or body.
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            query: Vec::new(),
            headers: Vec::new(),
            json: None,
            secrets: Vec::new(),
        }
    }

    /// Adds a header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Adds a credential header whose value (and `secret`) are scrubbed from errors.
    pub fn credential_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        let value = value.into();
        self.secrets.push(value.clone());
        self.secrets.push(secret.into());
        self.headers.push((name.into(), value));
        self
    }

    /// Sets query parameters.
    pub fn query(mut self, query: Vec<(String, String)>) -> Self {
        self.query = query;
        self
    }

    /// Sets a JSON body.
    pub fn json(mut self, body: Option<Value>) -> Self {
        self.json = body;
        self
    }
}

/// A decoded response.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    /// Final status code (after redirects and retries).
    pub status: u16,
    /// Final response headers (lowercase names).
    pub headers: Vec<(String, String)>,
    /// Decoded body: JSON for JSON content types, else a string.
    pub body: Value,
}

/// Redirect + retry + decoding policy over a [`Transport`]. Cheap to clone.
#[derive(Clone, Debug)]
pub struct HttpClient {
    transport: Arc<dyn Transport>,
    retry: RetryPolicy,
    max_redirects: u32,
}

impl HttpClient {
    /// The production client (reqwest with Req-compatible defaults).
    pub fn reqwest() -> Result<Self, TransportError> {
        Ok(Self::new(Arc::new(ReqwestTransport::new()?)))
    }

    /// A client over a custom transport with the default retry and redirect policy.
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            retry: RetryPolicy::default(),
            max_redirects: MAX_REDIRECTS,
        }
    }

    /// Overrides the retry policy (tests use millisecond delays).
    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Overrides the redirect limit.
    pub fn with_max_redirects(mut self, max_redirects: u32) -> Self {
        self.max_redirects = max_redirects;
        self
    }

    /// The active retry policy.
    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry
    }

    /// Executes the request with redirects, retries and body decoding.
    ///
    /// Runs inside a `tracker_http` span carrying the method and the URL without its query string
    /// (credentials are header-only, so the span never carries secrets).
    pub async fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let secrets = request.secrets.clone();
        let target = request.url.split('?').next().unwrap_or_default().to_owned();
        let span = tracing::debug_span!(
            "tracker_http",
            method = %request.method,
            url = %scrub_secrets(&target, &secrets)
        );
        self.send_inner(request)
            .instrument(span)
            .await
            .map_err(|err| err.scrubbed(&secrets))
    }

    async fn send_inner(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut url = Url::parse(&request.url)
            .map_err(|_| TransportError::InvalidUrl(request.url.clone()))?;
        if !request.query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(request.query.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        let mut headers = request.headers.clone();
        let mut body = match &request.json {
            Some(value) => {
                if find_header(&headers, "content-type").is_none() {
                    headers.push(("Content-Type".into(), "application/json".into()));
                }
                Some(
                    serde_json::to_vec(value)
                        .map_err(|err| TransportError::Other(err.to_string()))?,
                )
            }
            None => None,
        };
        let mut method = request.method;
        let mut hops = 0;
        loop {
            let raw = RawRequest {
                method,
                url: url.clone(),
                headers: headers.clone(),
                body: body.clone(),
            };
            let response = self.send_with_retry(raw).await?;
            let location = response
                .header("location")
                .filter(|_| matches!(response.status, 301 | 302 | 303 | 307 | 308))
                .map(str::to_owned);
            let Some(location) = location else {
                return decode(response);
            };
            if hops >= self.max_redirects {
                return Err(TransportError::TooManyRedirects);
            }
            hops += 1;
            let next = url
                .join(&location)
                .map_err(|_| TransportError::InvalidUrl(location.clone()))?;
            if !same_origin(&url, &next) {
                headers.retain(|(name, _)| !is_credential_header(name));
            }
            if matches!(response.status, 301..=303) && method != Method::Head {
                if method != Method::Get {
                    headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
                }
                method = Method::Get;
                body = None;
            }
            tracing::debug!(status = response.status, "following redirect");
            url = next;
        }
    }

    async fn send_with_retry(&self, raw: RawRequest) -> Result<RawResponse, TransportError> {
        let mut attempt: u32 = 0;
        loop {
            let result = self.transport.send(raw.clone()).await;
            let retryable = raw.method.is_safe()
                && match &result {
                    Ok(response) => RETRYABLE_STATUSES.contains(&response.status),
                    Err(err) => err.is_transient(),
                };
            if !retryable || attempt >= self.retry.max_retries {
                return result;
            }
            let backoff = self
                .retry
                .base_delay
                .saturating_mul(2u32.saturating_pow(attempt));
            let delay = result
                .as_ref()
                .ok()
                .and_then(|r| r.header("retry-after"))
                .and_then(parse_retry_after)
                .unwrap_or(backoff)
                .min(self.retry.max_delay);
            let left = self.retry.max_retries - attempt;
            match &result {
                Ok(response) => tracing::warn!(
                    "retry: got response with status {}, will retry in {}ms, {} attempts left",
                    response.status,
                    delay.as_millis(),
                    left
                ),
                Err(err) => tracing::warn!(
                    "retry: got exception {}, will retry in {}ms, {} attempts left",
                    err.inspect(),
                    delay.as_millis(),
                    left
                ),
            }
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

/// `Retry-After`: delay seconds or an HTTP date.
pub fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(
        at.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn is_json_content_type(value: &str) -> bool {
    let mime = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime == "application/json" || mime.ends_with("+json")
}

fn decode(response: RawResponse) -> Result<HttpResponse, TransportError> {
    let json = response
        .header("content-type")
        .is_some_and(is_json_content_type);
    let body = if json && !response.body.is_empty() {
        serde_json::from_slice(&response.body)
            .map_err(|err| TransportError::InvalidJson(err.to_string()))?
    } else {
        Value::String(String::from_utf8_lossy(&response.body).into_owned())
    };
    Ok(HttpResponse {
        status: response.status,
        headers: response.headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbing_removes_known_secrets_bearer_tokens_and_userinfo() {
        let secrets = vec!["super-secret-token".to_string(), "abc".to_string()];
        let text = "failed Authorization: Bearer super-secret-token at https://user:pw@host/x abc";
        let scrubbed = scrub_secrets(text, &secrets);
        assert!(!scrubbed.contains("super-secret-token"));
        assert!(!scrubbed.contains("user:pw"));
        assert!(scrubbed.contains("https://[REDACTED]@host/x"));
        assert!(scrubbed.contains("Bearer [REDACTED]"));
        // Secrets shorter than four characters are not replaced (too likely to be ordinary words).
        assert!(scrubbed.ends_with("abc"));
        assert_eq!(
            scrub_secrets("Basic dXNlcjpwYXNz, more", &[]),
            "Basic [REDACTED], more"
        );
    }

    #[test]
    fn retry_after_parses_seconds_and_dates() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon"), None);
    }

    #[test]
    fn json_content_types() {
        assert!(is_json_content_type("application/json; charset=utf-8"));
        assert!(is_json_content_type("application/vnd.github+json"));
        assert!(!is_json_content_type("text/plain"));
    }

    #[test]
    fn origins_compare_scheme_host_and_port() {
        let a = Url::parse("https://gitlab.test/x").unwrap();
        assert!(same_origin(
            &a,
            &Url::parse("https://gitlab.test:443/sink").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("http://gitlab.test/sink").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("https://sink.test/sink").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("https://gitlab.test:8443/").unwrap()
        ));
    }

    #[test]
    fn request_debug_redacts_header_values() {
        let request = HttpRequest::new(Method::Get, "https://x.test").credential_header(
            "Authorization",
            "Bearer tok-123456",
            "tok-123456",
        );
        let debug = format!("{request:?}");
        assert!(!debug.contains("tok-123456"));
    }
}
