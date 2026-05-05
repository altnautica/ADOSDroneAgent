//! Static-file serving for the universal setup webapp.
//!
//! The HTML/CSS/JS at `web/setup/` is embedded into the binary at
//! compile time via the `include_dir!` macro. axum handlers below
//! serve those files at:
//!
//! - `/`            → 302 to `/setup/`
//! - `/setup`       → `web/setup/index.html`
//! - `/setup/`      → `web/setup/index.html`
//! - `/setup/<path>` → `web/setup/<path>` (or 404 if not found)
//!
//! The agent never reads the filesystem for these files — they live in
//! the binary's read-only data section. This is what makes the lite
//! agent self-contained: a single static binary on a fresh Buildroot
//! rootfs serves the entire wizard.

use axum::{
    extract::Request,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use include_dir::{include_dir, Dir};

// Embedded webapp lives at `agents/lite-rs/crates/ados-setup/web-setup/`,
// kept in sync with the canonical `web/setup/` at the repo root.
// The crate-local copy is required because `cross` (used for CI
// cross-compile) only mounts the workspace root (agents/lite-rs/) into
// the build container, so a `../../../../web/setup` path would resolve
// to a directory outside the mount and break the build.
//
// Sync from canonical: `cp -r web/setup/* agents/lite-rs/crates/ados-setup/web-setup/`
// Both the Python full agent and this Rust lite agent serve identical
// HTML/CSS/JS — keep them in sync until we move to a shared crate +
// build-time fetch.
static SETUP_WEBAPP: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/web-setup");

/// Fallback handler — serves any non-API request from the embedded
/// webapp dir. Mirrors the Python full agent's `StaticFiles` mount at
/// the root so the same `web/setup/index.html` works unchanged
/// (HTML uses absolute paths like `/style.css`, `/app.js`).
///
/// Behavior:
/// - `/` → `web-setup/index.html`
/// - `/<asset>` → `web-setup/<asset>` if it exists
/// - everything else → `web-setup/index.html` (SPA-style fallback so
///   client-side routes like `/setup` and `/network` still load the
///   wizard shell, which then dispatches via JavaScript)
pub async fn serve_request(req: Request) -> Response {
    let path = req.uri().path();
    let cleaned = path.trim_start_matches('/');

    // Defense-in-depth: reject any path-traversal attempts before we
    // hit the embedded dir lookup.
    if cleaned.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }

    // API namespace stays strict — an unmatched /api/* route is a 404
    // that should not be swallowed by the webapp fallback. The wizard
    // never lives under /api/, only the JSON + WS surface does.
    if path.starts_with("/api/") {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    // Empty path or trailing-slash-only paths serve index.html.
    let lookup_path = if cleaned.is_empty() || cleaned.ends_with('/') {
        "index.html"
    } else {
        cleaned
    };

    // Try the requested asset; if missing, fall back to index.html so
    // the JavaScript dispatcher in app.js can route on its own.
    let file = SETUP_WEBAPP
        .get_file(lookup_path)
        .or_else(|| SETUP_WEBAPP.get_file("index.html"));

    match file {
        Some(file) => {
            let body = file.contents();
            let mime = guess_mime(file.path().to_string_lossy().as_ref());
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    // Webapp assets are tied to the binary signature.
                    (header::CACHE_CONTROL, "public, max-age=300"),
                ],
                body,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

fn guess_mime(path: &str) -> &'static str {
    if let Some(ext) = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
    {
        match ext.to_ascii_lowercase().as_str() {
            "html" | "htm" => "text/html; charset=utf-8",
            "css" => "text/css; charset=utf-8",
            "js" | "mjs" => "application/javascript; charset=utf-8",
            "json" => "application/json",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "ico" => "image/x-icon",
            "woff" => "font/woff",
            "woff2" => "font/woff2",
            "ttf" => "font/ttf",
            "txt" => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        }
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_webapp_contains_index_html() {
        assert!(
            SETUP_WEBAPP.get_file("index.html").is_some(),
            "web/setup/index.html must be embedded"
        );
    }

    #[test]
    fn embedded_webapp_contains_app_js() {
        assert!(
            SETUP_WEBAPP.get_file("app.js").is_some(),
            "web/setup/app.js must be embedded"
        );
    }

    #[test]
    fn mime_detection_covers_common_extensions() {
        assert_eq!(guess_mime("index.html"), "text/html; charset=utf-8");
        assert_eq!(guess_mime("app.js"), "application/javascript; charset=utf-8");
        assert_eq!(guess_mime("style.css"), "text/css; charset=utf-8");
        assert_eq!(guess_mime("brand.svg"), "image/svg+xml");
    }
}
