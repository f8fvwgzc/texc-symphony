//! The embedded dashboard bundle (port of "dashboard bootstraps ... from embedded static assets",
//! minus the dropped Phoenix vendor scripts).

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::{Harness, envelope};
use symphony_server::WEB_UI_EMBEDDED;

#[tokio::test]
async fn index_is_served_with_no_cache() {
    let harness = Harness::snapshot();
    let (status, headers, body) = harness.send(Method::GET, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(headers["cache-control"], "no-cache");
    assert!(headers.contains_key("etag"));
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("<title>Symphony Observability</title>"));
    if WEB_UI_EMBEDDED {
        assert!(
            html.contains("/assets/"),
            "index references the hashed bundle"
        );
    } else {
        assert!(html.contains("pnpm --dir web build"), "placeholder page");
    }
    assert!(
        !html.contains("/vendor/phoenix"),
        "no LiveView vendor scripts"
    );

    let (status, headers, body) = harness.send(Method::HEAD, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
}

#[tokio::test]
async fn bundle_files_are_served_with_cache_headers() {
    let harness = Harness::snapshot();
    let (_, _, index) = harness.send(Method::GET, "/").await;
    let html = String::from_utf8(index.to_vec()).unwrap();
    // Every same-origin `src`/`href` in index.html must resolve.
    let references: Vec<&str> = html
        .split(['"', '\''])
        .filter(|part| part.starts_with('/') && !part.starts_with("/api/") && part.len() > 1)
        .collect();
    if WEB_UI_EMBEDDED {
        assert!(!references.is_empty());
    }
    for reference in references {
        let (status, headers, body) = harness.send(Method::GET, reference).await;
        assert_eq!(status, StatusCode::OK, "{reference}");
        assert!(!body.is_empty(), "{reference}");
        let cache = headers["cache-control"].to_str().unwrap();
        if reference.starts_with("/assets/") {
            assert_eq!(cache, "public, max-age=31536000, immutable", "{reference}");
        } else {
            assert_eq!(cache, "no-cache", "{reference}");
        }
        if reference.ends_with(".png") {
            assert_eq!(headers["content-type"], "image/png");
        }
        if reference.ends_with(".js") {
            assert_eq!(headers["content-type"], "text/javascript; charset=utf-8");
        }
        if reference.ends_with(".css") {
            assert_eq!(headers["content-type"], "text/css; charset=utf-8");
        }
    }
}

#[tokio::test]
async fn etags_allow_revalidation() {
    let harness = Harness::snapshot();
    let (_, headers, _) = harness.send(Method::GET, "/").await;
    let etag = headers["etag"].clone();
    let request = Request::builder()
        .uri("/")
        .header("if-none-match", etag.clone())
        .body(Body::empty())
        .unwrap();
    let response = harness.request(request).await;
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers()["etag"], etag);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert!(body.is_empty());
}

#[tokio::test]
async fn non_bundle_paths_keep_the_json_404() {
    let harness = Harness::snapshot();
    for (method, uri) in [
        (Method::GET, "/runs"),
        (Method::GET, "/assets/missing-123.js"),
        (Method::GET, "/index.htm"),
        (Method::POST, "/index.html"),
        (Method::GET, "/vendor/phoenix/phoenix.js"),
    ] {
        let (status, body) = harness.json(method.clone(), uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(body, envelope("not_found", "Route not found"));
    }
}
