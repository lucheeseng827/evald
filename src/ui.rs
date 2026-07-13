//! Embedded console — the built `frontend/dist/` assets baked into the binary.
//!
//! The console is a Vite + React + TypeScript SPA (source in `frontend/src/`), compiled to
//! `frontend/dist/` — which is committed, so `rust-embed` bakes the static bundle into the
//! single musl artifact with **no Node toolchain at Rust build time** (air-gapped, no
//! separate web server). Regenerate `dist/` with `npm run build` after any UI edit.
//!
//! This is the OSS console — the `Local node · OSS` surfaces only. It contains NO
//! Enterprise-Edition view code (guarded by `bundle_has_no_ee_surfaces` below): the EE
//! `fleet-query` node serves its OWN bundle (the OSS views + the `Fleet · EE` surfaces)
//! from `ee/frontend/dist` via `evald_fleet::ui`. That split is the license boundary in
//! the UI. Both nodes still answer `GET /v1/meta` for the edition/version handshake.
//!
//! [`static_handler`] is wired as the axum **fallback**, behind the `/v1/*` API routes: it
//! serves a matching asset (`index.html`, `assets/*.js`, `assets/*.css`), else falls back to
//! `index.html` so the single-page app owns client-side navigation. In debug builds
//! `rust-embed` reads the files from disk; in release it embeds their bytes.

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "frontend/dist/"]
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

    /// The built console must be present: the shell plus at least one JS and one CSS asset.
    /// A missing bundle means someone shipped without running `npm run build` (or `dist/`
    /// was gitignored) — caught here rather than as a blank page at runtime.
    #[test]
    fn console_bundle_is_embedded() {
        assert!(
            Assets::get("index.html").is_some(),
            "index.html shell must be embedded"
        );
        let has_js = Assets::iter().any(|p| p.ends_with(".js"));
        let has_css = Assets::iter().any(|p| p.ends_with(".css"));
        assert!(
            has_js,
            "a built JS asset must be embedded (run `npm run build`)"
        );
        assert!(
            has_css,
            "a built CSS asset must be embedded (run `npm run build`)"
        );
    }

    /// Return the single built JS bundle's text (Vite emits one entry chunk).
    fn bundle_js() -> String {
        let path = Assets::iter()
            .find(|p| p.ends_with(".js"))
            .expect("a built JS asset must be embedded");
        let asset = Assets::get(&path).unwrap();
        String::from_utf8(asset.data.into_owned()).expect("bundle is utf-8")
    }

    /// The search/filter + payload-virtualization + blob-reference features must stay in the
    /// console — asserted on string literals that survive minification (function names do
    /// not), so a future edit that drops them is caught at build time (the JS behavior itself
    /// is exercised out-of-band; there is no in-binary JS runtime).
    #[test]
    fn bundle_keeps_search_and_virtualization() {
        let js = bundle_js();
        for marker in [
            "trace-search", // the search input id
            "evald-blob:",  // offloaded-payload reference rendering
            "/v1/blobs/",   // blob fetch link
            "/v1/traces/",  // span-tree fetch
        ] {
            assert!(js.contains(marker), "console bundle must keep `{marker}`");
        }
    }

    /// This is the OSS console: it must show the OSS nav group and wire the OSS endpoints.
    #[test]
    fn bundle_wires_oss_surfaces() {
        let js = bundle_js();
        for marker in [
            "Local node · OSS", // the OSS nav-group label
            "/v1/stats",
            "/v1/spans",
            "/v1/scores",
            "/v1/sql",
        ] {
            assert!(js.contains(marker), "OSS console bundle must wire `{marker}`");
        }
    }

    /// LICENSE BOUNDARY GUARD: the OSS crate ships (and public-mirrors) this bundle, so it
    /// must contain ZERO Enterprise-Edition surfaces. The EE console (fleet/tenants/billing/
    /// audit/members/judge-keys) lives only in the private `ee/` tree — see `ee/frontend`. If
    /// a refactor re-welds an EE view into the OSS frontend, this fails before it ever mirrors.
    #[test]
    fn bundle_has_no_ee_surfaces() {
        let js = bundle_js();
        for ee_marker in [
            "Fleet · EE",
            "/v1/admin/tokens",
            "/v1/admin/judge-keys",
            "/v1/fleet/lag",
            "/v1/usage",
            "/v1/invoice",
            "managed_judge",
            "MeteredUnit",
            "Organizations & projects",
            "Audit ledger",
        ] {
            assert!(
                !js.contains(ee_marker),
                "OSS console bundle must NOT contain EE surface `{ee_marker}` (license boundary)"
            );
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
    async fn serves_bundle_js_with_js_content_type() {
        // The built JS asset (assets/index.js) must serve with a JS content type.
        let path = Assets::iter()
            .find(|p| p.ends_with(".js"))
            .expect("a JS asset");
        let resp = static_handler(format!("/{path}").parse().unwrap()).await;
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
