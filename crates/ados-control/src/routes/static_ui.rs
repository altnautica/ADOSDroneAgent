//! The browser dashboard (`/`) and the on-box cockpit (`/cockpit/`), served from
//! their built bundles.
//!
//! Both are static single-page apps. The agent package ships them as
//! `ados/dashboard/static` and `ados/cockpit/static`; this front finds them in the
//! installed package (or at `ADOS_DASHBOARD_DIR` / `ADOS_COCKPIT_DIR`) and serves
//! them itself, so the operator UI does not depend on the residual Python
//! process.
//!
//! - The dashboard is client-routed: a path that names no file and does not look
//!   like an asset (no `.` in its last segment, not under `assets/`) gets
//!   `index.html` so the router can resolve it. A missing asset is a real 404.
//! - The cockpit has no client-side routing: `/cockpit` redirects to `/cockpit/`
//!   keeping the query (the cockpit reads its access key and render flags from
//!   it), and a missing file is a 404.
//! - Hashed build assets (`assets/*`) may be cached forever; everything else,
//!   the entry above all, revalidates (`no-cache` plus an `ETag`), so a browser
//!   never keeps running a bundle the node stopped serving.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::routes::detail;

/// Which bundle a request addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bundle {
    Dashboard,
    Cockpit,
}

impl Bundle {
    fn env(self) -> &'static str {
        match self {
            Bundle::Dashboard => "ADOS_DASHBOARD_DIR",
            Bundle::Cockpit => "ADOS_COCKPIT_DIR",
        }
    }

    fn package(self) -> &'static str {
        match self {
            Bundle::Dashboard => "dashboard",
            Bundle::Cockpit => "cockpit",
        }
    }

    fn cell(self) -> &'static OnceLock<PathBuf> {
        static DASHBOARD: OnceLock<PathBuf> = OnceLock::new();
        static COCKPIT: OnceLock<PathBuf> = OnceLock::new();
        match self {
            Bundle::Dashboard => &DASHBOARD,
            Bundle::Cockpit => &COCKPIT,
        }
    }
}

/// The library dir of the agent's virtualenv, where the package is installed.
const VENV_LIB: &str = "/opt/ados/venv/lib";

/// Find `ados/<package>/static` under `<lib>/python3*/site-packages`.
fn find_in_venv(lib: &Path, package: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(lib)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("python3"))
        .map(|e| {
            e.path()
                .join("site-packages/ados")
                .join(package)
                .join("static")
        })
        .filter(|p| p.join("index.html").is_file())
        .collect();
    // Two interpreters side by side only during an upgrade; the newest wins.
    candidates.sort();
    candidates.pop()
}

/// The bundle root: the env override when set, else the installed package. A
/// found root is remembered for the life of the process; a missing one is
/// looked up again on the next request, so a bundle installed after start is
/// picked up.
fn bundle_root(bundle: Bundle) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(bundle.env()) {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    if let Some(found) = bundle.cell().get() {
        return Some(found.clone());
    }
    let found = find_in_venv(Path::new(VENV_LIB), bundle.package())?;
    Some(bundle.cell().get_or_init(|| found).clone())
}

/// Join a request-relative path onto `root`, or `None` when any segment could
/// leave it or is not a plain file name. Percent escapes are refused rather
/// than decoded: build output never needs them.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        if seg == "."
            || seg == ".."
            || seg
                .bytes()
                .any(|b| matches!(b, b'\\' | b'%' | 0) || b.is_ascii_control())
        {
            return None;
        }
        out.push(seg);
    }
    Some(out)
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// The cache policy for a request-relative path.
fn cache_control(rel: &str) -> &'static str {
    if rel.starts_with("assets/") {
        // The name carries a content hash, so the bytes behind it never change.
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// Whether a dashboard path that names no file should get the SPA entry: not an
/// asset, not a file-looking name, and never an API path, where a typo must
/// stay a crisp 404 rather than an HTML page.
fn spa_fallback_applies(rel: &str) -> bool {
    let last = rel.rsplit('/').next().unwrap_or("");
    !rel.starts_with("assets/") && !last.contains('.') && !(rel == "api" || rel.starts_with("api/"))
}

/// A file read for serving.
struct Served {
    bytes: Vec<u8>,
    etag: String,
    content_type: &'static str,
}

/// Read `path`, or `path/index.html` when it is a directory.
async fn read_file(path: PathBuf) -> Option<Served> {
    let mut path = path;
    let mut meta = tokio::fs::metadata(&path).await.ok()?;
    if meta.is_dir() {
        path.push("index.html");
        meta = tokio::fs::metadata(&path).await.ok()?;
    }
    if !meta.is_file() {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Some(Served {
        etag: format!("\"{mtime:x}-{:x}\"", bytes.len()),
        content_type: content_type(&path),
        bytes,
    })
}

fn respond(request: &Request, rel: &str, served: Served) -> Response {
    let cache = HeaderValue::from_static(cache_control(rel));
    let etag = HeaderValue::from_str(&served.etag).ok();
    let not_modified = etag.as_ref().is_some_and(|tag| {
        request
            .headers()
            .get(header::IF_NONE_MATCH)
            .is_some_and(|v| v == tag)
    });
    let mut response = if not_modified {
        StatusCode::NOT_MODIFIED.into_response()
    } else if request.method() == Method::HEAD {
        let mut r = Response::new(Body::empty());
        r.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from(served.bytes.len()),
        );
        r
    } else {
        Response::new(Body::from(served.bytes))
    };
    let headers = response.headers_mut();
    if !not_modified {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(served.content_type),
        );
    }
    headers.insert(header::CACHE_CONTROL, cache);
    if let Some(tag) = etag {
        headers.insert(header::ETAG, tag);
    }
    response
}

/// Serve a dashboard or cockpit request.
pub async fn serve(request: Request) -> Response {
    let dashboard = bundle_root(Bundle::Dashboard);
    let cockpit = bundle_root(Bundle::Cockpit);
    serve_from(dashboard.as_deref(), cockpit.as_deref(), request).await
}

async fn serve_from(
    dashboard: Option<&Path>,
    cockpit: Option<&Path>,
    request: Request,
) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return detail(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    }
    let path = request.uri().path().to_owned();

    if path == "/cockpit" {
        let target = match request.uri().query() {
            Some(q) if !q.is_empty() => format!("/cockpit/?{q}"),
            _ => "/cockpit/".to_owned(),
        };
        return match HeaderValue::from_str(&target) {
            Ok(location) => {
                let mut r = StatusCode::TEMPORARY_REDIRECT.into_response();
                r.headers_mut().insert(header::LOCATION, location);
                r
            }
            Err(_) => detail(StatusCode::NOT_FOUND, "Not Found"),
        };
    }

    if let Some(rel) = path.strip_prefix("/cockpit/") {
        let Some(root) = cockpit else {
            return bundle_missing("cockpit");
        };
        return match safe_join(root, rel) {
            Some(file) => match read_file(file).await {
                Some(served) => respond(&request, rel, served),
                None => detail(StatusCode::NOT_FOUND, "Not Found"),
            },
            None => detail(StatusCode::NOT_FOUND, "Not Found"),
        };
    }

    let Some(root) = dashboard else {
        return bundle_missing("dashboard");
    };
    let rel = path.trim_start_matches('/');
    if let Some(served) = match safe_join(root, rel) {
        Some(file) => read_file(file).await,
        None => None,
    } {
        return respond(&request, rel, served);
    }
    if spa_fallback_applies(rel) {
        if let Some(served) = read_file(root.join("index.html")).await {
            return respond(&request, "index.html", served);
        }
    }
    detail(StatusCode::NOT_FOUND, "Not Found")
}

fn bundle_missing(name: &str) -> Response {
    detail(
        StatusCode::SERVICE_UNAVAILABLE,
        format!("The {name} bundle is not installed on this node."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(dir: &Path) {
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("index.html"), "<html>entry</html>").unwrap();
        std::fs::write(dir.join("assets/app-1a2b.js"), "console.log(1)").unwrap();
    }

    async fn get(dashboard: &Path, cockpit: &Path, uri: &str) -> Response {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        serve_from(Some(dashboard), Some(cockpit), req).await
    }

    async fn text(r: Response) -> String {
        String::from_utf8(
            axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn the_entry_revalidates_and_hashed_assets_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let (d, c) = (tmp.path().join("d"), tmp.path().join("c"));
        bundle(&d);
        bundle(&c);

        let entry = get(&d, &c, "/cockpit/").await;
        assert_eq!(entry.status(), StatusCode::OK);
        assert_eq!(entry.headers()[header::CACHE_CONTROL], "no-cache");
        assert_eq!(text(entry).await, "<html>entry</html>");

        let asset = get(&d, &c, "/cockpit/assets/app-1a2b.js").await;
        assert_eq!(asset.status(), StatusCode::OK);
        let cache = asset.headers()[header::CACHE_CONTROL].to_str().unwrap();
        assert!(cache.contains("immutable") && cache.contains("max-age=31536000"));
        assert!(asset.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/javascript"));
    }

    #[tokio::test]
    async fn the_cockpit_redirect_keeps_the_access_key() {
        let tmp = tempfile::tempdir().unwrap();
        let (d, c) = (tmp.path().join("d"), tmp.path().join("c"));
        bundle(&d);
        bundle(&c);
        let r = get(&d, &c, "/cockpit?layer=minimal&key=abc123").await;
        assert_eq!(r.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            r.headers()[header::LOCATION],
            "/cockpit/?layer=minimal&key=abc123"
        );
        let bare = get(&d, &c, "/cockpit").await;
        assert_eq!(bare.headers()[header::LOCATION], "/cockpit/");
    }

    #[tokio::test]
    async fn client_routes_get_the_entry_but_missing_assets_and_api_paths_do_not() {
        let tmp = tempfile::tempdir().unwrap();
        let (d, c) = (tmp.path().join("d"), tmp.path().join("c"));
        bundle(&d);
        bundle(&c);

        let route = get(&d, &c, "/setup/network").await;
        assert_eq!(route.status(), StatusCode::OK);
        assert_eq!(text(route).await, "<html>entry</html>");

        for missing in [
            "/assets/gone-9f.js",
            "/favicon-missing.png",
            "/api/typo",
            "/cockpit/nope",
        ] {
            assert_eq!(
                get(&d, &c, missing).await.status(),
                StatusCode::NOT_FOUND,
                "{missing}"
            );
        }
    }

    #[tokio::test]
    async fn nothing_outside_the_bundle_is_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let (d, c) = (tmp.path().join("d"), tmp.path().join("c"));
        bundle(&d);
        bundle(&c);
        std::fs::write(tmp.path().join("secret.txt"), "key").unwrap();
        for uri in [
            "/../secret.txt",
            "/cockpit/../secret.txt",
            "/%2e%2e/secret.txt",
        ] {
            let r = get(&d, &c, uri).await;
            if r.status() == StatusCode::OK {
                assert_ne!(text(r).await, "key", "{uri} escaped the bundle");
            }
        }
    }

    #[tokio::test]
    async fn an_unchanged_entry_answers_304() {
        let tmp = tempfile::tempdir().unwrap();
        let (d, c) = (tmp.path().join("d"), tmp.path().join("c"));
        bundle(&d);
        bundle(&c);
        let first = get(&d, &c, "/").await;
        let tag = first.headers()[header::ETAG].clone();
        let req = Request::builder()
            .uri("/")
            .header(header::IF_NONE_MATCH, tag)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            serve_from(Some(&d), Some(&c), req).await.status(),
            StatusCode::NOT_MODIFIED
        );
    }

    #[test]
    fn the_installed_bundle_is_found_in_the_venv() {
        let tmp = tempfile::tempdir().unwrap();
        let static_dir = tmp
            .path()
            .join("python3.11/site-packages/ados/cockpit/static");
        bundle(&static_dir);
        assert_eq!(find_in_venv(tmp.path(), "cockpit"), Some(static_dir));
        assert_eq!(find_in_venv(tmp.path(), "dashboard"), None);
    }
}
