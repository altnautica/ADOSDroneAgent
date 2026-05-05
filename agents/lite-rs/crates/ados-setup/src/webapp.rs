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
    extract::Path,
    http::{header, StatusCode},
    response::{IntoResponse, Redirect, Response},
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

/// `GET /` redirects to the wizard.
pub async fn redirect_root() -> Redirect {
    Redirect::temporary("/setup/")
}

/// `GET /setup` and `GET /setup/` both serve `index.html`.
pub async fn serve_index() -> Response {
    serve_path("index.html").await
}

/// `GET /setup/{path}` serves a file from the embedded webapp directory.
pub async fn serve_asset(Path(path): Path<String>) -> Response {
    serve_path(&path).await
}

async fn serve_path(path: &str) -> Response {
    // Strip a leading slash (from path-segment captures) + reject
    // path-traversal attempts (`..`) so a curl with a hostile URL can
    // never escape the embedded dir.
    let cleaned = path.trim_start_matches('/');
    if cleaned.contains("..") {
        return (StatusCode::BAD_REQUEST, "invalid path").into_response();
    }
    // Empty path resolves to index.html.
    let lookup_path = if cleaned.is_empty() {
        "index.html"
    } else {
        cleaned
    };

    match SETUP_WEBAPP.get_file(lookup_path) {
        Some(file) => {
            let body = file.contents();
            let mime = guess_mime(lookup_path);
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    // Webapp assets are static + signed via the binary
                    // signature itself. A 5-min cache is reasonable.
                    (header::CACHE_CONTROL, "public, max-age=300"),
                ],
                body,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, format!("not found: {lookup_path}")).into_response(),
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
