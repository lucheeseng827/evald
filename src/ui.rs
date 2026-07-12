//! Embedded SPA — the `frontend/` assets baked into the binary (PoC build step 8).
//!
//! `rust-embed` compiles `frontend/` (a dependency-free vanilla-JS SPA: trace list →
//! trace tree → scores, plus a SQL console over [`crate::sql`]) into the binary, so the UI
//! ships in the single static musl artifact — no Node toolchain, no separate web server,
//! works air-gapped. [`static_handler`] is wired as the axum **fallback**, behind the
//! `/v1/*` API routes: it serves a matching asset, else falls back to `index.html` so the
//! single-page app owns client-side navigation.
//!
//! In debug builds `rust-embed` reads the files from disk (live-edit the SPA without a
//! rebuild); in release it embeds their bytes.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "frontend/"]
struct Assets;

/// Serve an embedded asset by request path, falling back to `index.html` (the SPA shell)
/// for paths that don't map to a bundled file.
pub async fn static_handler(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');

    // Reject path traversal before any lookup. In *debug* builds rust-embed reads from disk,
    // so a literal `..` segment could otherwise escape `frontend/` and disclose a local
    // file; fail closed in both debug and release. (`Uri::path` is not normalized, so the
    // `..` reaches us verbatim.)
    if raw.split('/').any(|seg| seg == ".." || seg == ".") {
        return (StatusCode::NOT_FOUND, "evald: not found\n").into_response();
    }

    let path = if raw.is_empty() { "index.html" } else { raw };

    if let Some(asset) = Assets::get(path) {
        return (
            [
                (header::CONTENT_TYPE, content_type(path)),
                (header::CACHE_CONTROL, cache_control(path)),
            ],
            asset.data.into_owned(),
        )
            .into_response();
    }

    // SPA fallback: unknown route → the app shell (client-side nav owns the rest).
    match Assets::get("index.html") {
        Some(asset) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            asset.data.into_owned(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "evald: UI assets not bundled in this build\n",
        )
            .into_response(),
    }
}

/// `no-cache` on the HTML shell so a new build is always picked up; modest caching for the
/// static JS/CSS. (No hashed asset names yet, so nothing is cached immutably.)
fn cache_control(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "no-cache"
    } else {
        "max-age=3600"
    }
}

/// Best-effort content type from the file extension (the SPA is HTML/CSS/JS only; a small
/// explicit table avoids pulling a mime-guess dependency).
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_html_is_embedded() {
        // The SPA shell must be present so `serve` has a UI to hand out.
        assert!(Assets::get("index.html").is_some());
    }

    /// The search/filter + payload-virtualization + blob-reference features must be bundled
    /// into the embedded SPA — a smoke test so a future edit that drops them from the assets
    /// (or renames the wiring) is caught at build time, since the JS behavior itself is
    /// exercised out-of-band (no in-binary JS runtime).
    #[test]
    fn spa_bundles_search_and_virtualization() {
        let html = Assets::get("index.html").expect("index.html embedded");
        let html = std::str::from_utf8(&html.data).unwrap();
        assert!(
            html.contains("trace-search"),
            "search input must be in the shell"
        );

        let js = Assets::get("app.js").expect("app.js embedded");
        let js = std::str::from_utf8(&js.data).unwrap();
        for marker in [
            "buildHaystack",      // full-text index
            "traceMatches",       // AND-term filter
            "initTraceSearch",    // search wiring
            "appendPayloadBlock", // payload virtualization
            "evald-blob:",        // offloaded-payload reference rendering
        ] {
            assert!(js.contains(marker), "app.js must wire `{marker}`");
        }
    }

    /// The aggregated-stats Dashboard view must stay bundled: its nav entry in the shell and
    /// its aggregation/render wiring in the JS.
    #[test]
    fn spa_bundles_dashboard_view() {
        let html = Assets::get("index.html").expect("index.html embedded");
        let html = std::str::from_utf8(&html.data).unwrap();
        assert!(
            html.contains("data-view=\"dashboard\""),
            "dashboard nav entry must exist"
        );
        assert!(
            html.contains("view-dashboard"),
            "dashboard view container must exist"
        );

        let js = Assets::get("app.js").expect("app.js embedded");
        let js = std::str::from_utf8(&js.data).unwrap();
        for marker in [
            "computeDashboard",
            "renderDashboard",
            "loadIngestHealth",
            "percentile",
        ] {
            assert!(js.contains(marker), "app.js must wire `{marker}`");
        }
    }

    #[tokio::test]
    async fn unknown_route_falls_back_to_index() {
        let resp = static_handler("/some/spa/route".parse().unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn serves_app_js_with_js_content_type() {
        let resp = static_handler("/app.js".parse().unwrap()).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        // A literal `..` segment must 404 (it would escape frontend/ on a debug disk read).
        for evil in [
            "/../Cargo.toml",
            "/../../etc/passwd",
            "/assets/../../src/ui.rs",
        ] {
            let resp = static_handler(evil.parse().unwrap()).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "should 404: {evil}");
        }
    }
}
