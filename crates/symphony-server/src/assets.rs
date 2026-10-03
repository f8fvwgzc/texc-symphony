//! The embedded web dashboard (`web/dist`, embedded at compile time by `build.rs`).
//!
//! * `GET /` serves `index.html` with `Cache-Control: no-cache` (always revalidated, so a new
//!   build is picked up at once).
//! * Hashed Vite assets (`/assets/*`) are served with `public, max-age=31536000, immutable`.
//! * Other bundle files (`/favicon.png`, ...) are `no-cache`.
//! * Every file carries a strong `ETag`; `If-None-Match` answers `304`.
//! * The dashboard uses hash routing, so there is no SPA fallback: any other path, and any
//!   non-GET method on a bundle path, is the JSON `404 not_found` envelope.

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

/// One file of the embedded bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EmbeddedAsset {
    /// Request path without the leading `/`.
    pub(crate) path: &'static str,
    /// `Content-Type` header value.
    pub(crate) content_type: &'static str,
    /// Strong ETag (quoted).
    pub(crate) etag: &'static str,
    /// Content-hashed file that may be cached forever.
    pub(crate) immutable: bool,
    /// File contents.
    pub(crate) body: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

const CACHE_IMMUTABLE: &str = "public, max-age=31536000, immutable";
const CACHE_REVALIDATE: &str = "no-cache";

/// Look `path` (without leading `/`) up in a table sorted by path.
pub(crate) fn find<'a>(assets: &'a [EmbeddedAsset], path: &str) -> Option<&'a EmbeddedAsset> {
    assets
        .binary_search_by(|asset| asset.path.cmp(path))
        .ok()
        .and_then(|index| assets.get(index))
}

/// Response for `asset`: `304` when `If-None-Match` matches, otherwise `200` (empty body for
/// `HEAD`).
pub(crate) fn asset_response(
    asset: &EmbeddedAsset,
    method: &Method,
    headers: &HeaderMap,
) -> Response {
    let cache_control = if asset.immutable {
        CACHE_IMMUTABLE
    } else {
        CACHE_REVALIDATE
    };
    let not_modified = headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag == asset.etag || tag.strip_prefix("W/") == Some(asset.etag));
    let common = [(ETAG, asset.etag), (CACHE_CONTROL, cache_control)];
    if not_modified {
        return (StatusCode::NOT_MODIFIED, common).into_response();
    }
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(asset.body)
    };
    (
        StatusCode::OK,
        common,
        [(CONTENT_TYPE, asset.content_type)],
        body,
    )
        .into_response()
}

/// `GET /`: the dashboard (or the placeholder page when `web/dist` was not built).
pub(crate) async fn index(method: Method, headers: HeaderMap) -> Response {
    match find(EMBEDDED_ASSETS, "index.html") {
        Some(asset) => asset_response(asset, &method, &headers),
        None => ApiError::NotFound.into_response(),
    }
}

/// Router fallback: bundle files for `GET`/`HEAD`, the JSON 404 envelope for everything else.
pub(crate) async fn fallback(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    if method == Method::GET || method == Method::HEAD {
        let path = uri.path().trim_start_matches('/');
        if !path.is_empty()
            && let Some(asset) = find(EMBEDDED_ASSETS, path)
        {
            return asset_response(asset, &method, &headers);
        }
    }
    ApiError::NotFound.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &[EmbeddedAsset] = &[
        EmbeddedAsset {
            path: "assets/app-1234.js",
            content_type: "text/javascript; charset=utf-8",
            etag: "\"js\"",
            immutable: true,
            body: b"console.log(1)",
        },
        EmbeddedAsset {
            path: "favicon.png",
            content_type: "image/png",
            etag: "\"png\"",
            immutable: false,
            body: b"\x89PNG",
        },
        EmbeddedAsset {
            path: "index.html",
            content_type: "text/html; charset=utf-8",
            etag: "\"html\"",
            immutable: false,
            body: b"<!doctype html>",
        },
    ];

    #[test]
    fn embedded_table_is_sorted_and_has_an_index() {
        assert!(EMBEDDED_ASSETS.windows(2).all(|w| w[0].path < w[1].path));
        let index = find(EMBEDDED_ASSETS, "index.html").expect("index.html is always embedded");
        assert_eq!(index.content_type, "text/html; charset=utf-8");
        assert!(!index.immutable);
        if !WEB_UI_EMBEDDED {
            assert_eq!(EMBEDDED_ASSETS.len(), 1);
            assert_eq!(index.body, include_bytes!("placeholder.html"));
        }
    }

    #[test]
    fn placeholder_page_explains_how_to_build_the_dashboard() {
        let page = include_str!("placeholder.html");
        assert!(page.contains("pnpm --dir web build"));
        assert!(page.contains("/api/v1/state"));
        assert!(!page.contains("<script"), "must satisfy the CSP");
    }

    #[test]
    fn lookup_and_cache_headers() {
        assert!(find(TABLE, "assets/app-1234.js").is_some());
        assert!(find(TABLE, "missing.js").is_none());
        let headers = HeaderMap::new();
        let js = asset_response(&TABLE[0], &Method::GET, &headers);
        assert_eq!(js.status(), StatusCode::OK);
        assert_eq!(js.headers()[CACHE_CONTROL], CACHE_IMMUTABLE);
        assert_eq!(js.headers()[ETAG], "\"js\"");
        let png = asset_response(&TABLE[1], &Method::GET, &headers);
        assert_eq!(png.headers()[CACHE_CONTROL], "no-cache");
        assert_eq!(png.headers()[CONTENT_TYPE], "image/png");
    }

    #[test]
    fn if_none_match_gives_304() {
        for value in ["\"html\"", "W/\"html\"", "\"x\", \"html\"", "*"] {
            let mut headers = HeaderMap::new();
            headers.insert(IF_NONE_MATCH, value.parse().unwrap());
            let response = asset_response(&TABLE[2], &Method::GET, &headers);
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED, "{value}");
        }
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, "\"other\"".parse().unwrap());
        assert_eq!(
            asset_response(&TABLE[2], &Method::GET, &headers).status(),
            StatusCode::OK
        );
    }
}
