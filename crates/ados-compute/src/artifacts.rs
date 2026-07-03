//! Static artifact server for compute outputs, path-jailed to the work root.
//!
//! A reconstruction backend writes its deliverable (a `.ply` splat, a `.rrd`
//! recording, a `.tif` orthomosaic) as a `file://` path under the node's work
//! root. The GCS cannot read a `file://` over the LAN, so the daemon serves those
//! files over plain HTTP from the same listener: a job's `outputUrl` becomes
//! `http://<reachable-bind>/artifacts/<relpath>`, which the Outputs viewer fetches
//! directly.
//!
//! The route is strictly path-jailed: only filesystem-normal path components are
//! accepted, and the canonicalised resolved path must sit under the canonicalised
//! work root (so a `..` traversal or a symlink escape is rejected). The internal
//! `file://` URI is preserved on the output's metadata so a downstream pipeline
//! stage still consumes the local file, not the HTTP URL.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::backends::file_uri_to_path;
use crate::Output;

/// The router that serves work-root artifacts over `GET /artifacts/<relpath>`.
/// `relpath` is the path of the artifact below the work root (e.g.
/// `<dataset>/output.ply`); a non-normal component or an out-of-root resolution
/// is a `404`.
pub fn artifact_router(work_root: PathBuf) -> Router {
    Router::new()
        .route("/artifacts/*path", get(serve_artifact))
        .with_state(Arc::new(work_root))
}

/// Resolve a request-relative path to an absolute path under `work_root`, or
/// `None` if it escapes. Rejects an empty path, an absolute path, and any `..` /
/// `.` / prefix component up front, then canonicalises and confirms the result is
/// under the canonicalised work root (which also defeats a symlink that points
/// outside the root). The file must exist for canonicalisation to succeed.
pub fn resolve_under_root(work_root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let candidate = Path::new(rel);
    for component in candidate.components() {
        if !matches!(component, Component::Normal(_)) {
            return None;
        }
    }
    let joined = work_root.join(candidate);
    let canonical = std::fs::canonicalize(&joined).ok()?;
    let canonical_root = std::fs::canonicalize(work_root).ok()?;
    canonical.starts_with(&canonical_root).then_some(canonical)
}

/// The content type for an artifact, keyed off its extension. A splat (`.ply`) and
/// a recording (`.rrd`) are binary downloads; the raster artifacts get their image
/// type so a browser can render them inline.
fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("tif") | Some("tiff") => "image/tiff",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("json") => "application/json",
        // .ply (splat), .rrd (recording), and anything else: a binary download.
        _ => "application/octet-stream",
    }
}

async fn serve_artifact(
    State(work_root): State<Arc<PathBuf>>,
    AxumPath(rel): AxumPath<String>,
) -> Response {
    let Some(path) = resolve_under_root(&work_root, &rel) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type_for(&path))], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Percent-encode the few characters that break a URL path, joining the
/// components of `rel` with `/`. The work root may sit on a path with a space, so
/// a raw concat would yield a URL a fetch cannot resolve.
fn encode_url_path(rel: &Path) -> String {
    let mut parts = Vec::new();
    for component in rel.components() {
        if let Component::Normal(part) = component {
            let mut encoded = String::new();
            for ch in part.to_string_lossy().chars() {
                match ch {
                    ' ' => encoded.push_str("%20"),
                    '#' => encoded.push_str("%23"),
                    '?' => encoded.push_str("%3F"),
                    '%' => encoded.push_str("%25"),
                    c => encoded.push(c),
                }
            }
            parts.push(encoded);
        }
    }
    parts.join("/")
}

/// Rewrite an output's `file://` URI (when it sits under `work_root`) to the
/// fetchable LAN artifact URL `<public_base>/artifacts/<relpath>`, preserving the
/// original `file://` URI on the output metadata as `local_uri` so a downstream
/// pipeline stage still chains on the local file. A non-`file://` URI (a `mock://`
/// output with no file) or a path outside the work root is left untouched.
pub fn rewrite_output_to_artifact_url(output: &mut Output, work_root: &Path, public_base: &str) {
    if !output.uri.starts_with("file://") {
        return;
    }
    let path = file_uri_to_path(&output.uri);
    let Ok(rel) = path.strip_prefix(work_root) else {
        return;
    };
    let local_uri = std::mem::take(&mut output.uri);
    output.uri = format!(
        "{}/artifacts/{}",
        public_base.trim_end_matches('/'),
        encode_url_path(rel)
    );
    match output.meta.as_object_mut() {
        Some(map) => {
            map.insert(
                "local_uri".to_string(),
                serde_json::Value::String(local_uri),
            );
        }
        None => {
            output.meta = serde_json::json!({ "local_uri": local_uri });
        }
    }
}

/// Rewrite the host of a stored `<base>/artifacts/<relpath>` URL to the current
/// `public_base`. The relpath is stable, so a URL frozen at an earlier daemon
/// run's hostname (which drifts, and may be an unreachable mDNS name) is served
/// under the live base on every read — self-healing against hostname drift.
/// A URL with no `/artifacts/` segment (a `mock://` placeholder, an empty ref)
/// passes through unchanged.
pub fn rewrite_artifact_host(uri: &str, public_base: &str) -> String {
    match uri.find("/artifacts/") {
        Some(idx) => format!(
            "{}/artifacts/{}",
            public_base.trim_end_matches('/'),
            &uri[idx + "/artifacts/".len()..]
        ),
        None => uri.to_string(),
    }
}

/// True for a bind host that is not a reachable URL host (the wildcard / any
/// address), so the public base falls back to the node hostname.
fn is_unspecified_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "[::]" | "")
}

/// Split a bind address into (host, port), handling `host:port` and the
/// bracketed IPv6 `[::]:port` form. Defaults the port to `8092` when absent.
fn split_host_port(bind: &str) -> (String, String) {
    if let Some(rest) = bind.strip_prefix('[') {
        // [ipv6]:port
        if let Some((host, port)) = rest.split_once("]:") {
            return (format!("[{host}]"), port.to_string());
        }
        return (bind.to_string(), "8092".to_string());
    }
    match bind.rsplit_once(':') {
        Some((host, port)) => (host.to_string(), port.to_string()),
        None => (bind.to_string(), "8092".to_string()),
    }
}

/// An mDNS-resolvable host for `hostname`: the bare name gets a `.local` suffix
/// (matching the mDNS SRV target the node advertises), a dotted name is used as-is.
fn mdns_host(hostname: &str) -> String {
    if hostname.contains('.') {
        hostname.to_string()
    } else {
        format!("{hostname}.local")
    }
}

/// Derive the public base URL the GCS uses to fetch artifacts. The explicit
/// override wins; otherwise the base is built from the bind address, substituting
/// the node hostname (mDNS `.local`) for an unspecified bind host so the URL is
/// reachable off-box, and `127.0.0.1` when no hostname is available.
pub fn derive_public_base(
    bind: &str,
    override_url: Option<&str>,
    hostname: Option<&str>,
) -> String {
    if let Some(url) = override_url {
        let url = url.trim().trim_end_matches('/');
        if !url.is_empty() {
            return url.to_string();
        }
    }
    let (host, port) = split_host_port(bind);
    let host = if is_unspecified_host(&host) {
        hostname
            .map(mdns_host)
            .unwrap_or_else(|| "127.0.0.1".to_string())
    } else {
        host
    };
    format!("http://{host}:{port}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::path_to_file_uri;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn write_file(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn resolve_accepts_a_file_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "ds-1/output.ply", b"ply");
        let resolved = resolve_under_root(dir.path(), "ds-1/output.ply").unwrap();
        assert_eq!(
            resolved,
            std::fs::canonicalize(dir.path().join("ds-1/output.ply")).unwrap()
        );
    }

    #[test]
    fn resolve_rejects_traversal_absolute_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "ds-1/output.ply", b"ply");
        assert!(resolve_under_root(dir.path(), "../etc/passwd").is_none());
        assert!(resolve_under_root(dir.path(), "ds-1/../../etc/passwd").is_none());
        assert!(resolve_under_root(dir.path(), "/etc/passwd").is_none());
        assert!(resolve_under_root(dir.path(), ".").is_none());
        assert!(resolve_under_root(dir.path(), "").is_none());
        // A real file the jail still serves (sanity that it is not over-rejecting).
        assert!(resolve_under_root(dir.path(), "ds-1/output.ply").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_a_symlink_that_escapes_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"secret").unwrap();
        // A symlink inside the work root pointing outside it. Canonicalisation
        // resolves the link, so the under-root check rejects it.
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("escape"))
            .unwrap();
        assert!(resolve_under_root(dir.path(), "escape").is_none());
    }

    #[tokio::test]
    async fn serves_an_artifact_with_a_content_type() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "ds-1/output.ply", b"PLYDATA");
        let router = artifact_router(dir.path().to_path_buf());
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/artifacts/ds-1/output.ply")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"PLYDATA");
    }

    #[tokio::test]
    async fn a_traversal_request_is_not_served() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "ds-1/output.ply", b"PLYDATA");
        let router = artifact_router(dir.path().to_path_buf());
        // A percent-encoded traversal: axum decodes it, the component check rejects it.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/artifacts/..%2f..%2fetc%2fpasswd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn rewrite_maps_a_file_uri_to_an_artifact_url_and_keeps_local() {
        let work_root = Path::new("/var/ados/compute/work");
        let file_uri = path_to_file_uri("/var/ados/compute/work/ds-1/output.ply");
        let mut output = Output {
            id: "job-1-out".into(),
            job_id: "job-1".into(),
            kind: "splat".into(),
            uri: file_uri.clone(),
            meta: serde_json::json!({ "gaussian_count": 1000 }),
            created_ms: 0,
        };
        rewrite_output_to_artifact_url(&mut output, work_root, "http://node.local:8092");
        assert_eq!(
            output.uri,
            "http://node.local:8092/artifacts/ds-1/output.ply"
        );
        // The original file:// is preserved for pipeline chaining, alongside the
        // existing meta (not clobbered).
        assert_eq!(output.meta["local_uri"], file_uri);
        assert_eq!(output.meta["gaussian_count"], 1000);
    }

    #[test]
    fn rewrite_leaves_a_mock_or_out_of_root_uri_untouched() {
        let work_root = Path::new("/var/ados/compute/work");
        let mut mock = Output::new(
            "out".into(),
            "job".into(),
            "splat".into(),
            "mock://splat/ds-1".into(),
            0,
        );
        rewrite_output_to_artifact_url(&mut mock, work_root, "http://node.local:8092");
        assert_eq!(mock.uri, "mock://splat/ds-1");
        assert!(mock.meta.get("local_uri").is_none());

        let mut outside = Output::new(
            "out2".into(),
            "job".into(),
            "splat".into(),
            path_to_file_uri("/tmp/elsewhere/output.ply"),
            0,
        );
        rewrite_output_to_artifact_url(&mut outside, work_root, "http://node.local:8092");
        assert!(outside.uri.starts_with("file://"));
    }

    #[test]
    fn rewrite_percent_encodes_a_spaced_work_path() {
        let work_root = Path::new("/My Work/compute");
        let mut output = Output::new(
            "out".into(),
            "job".into(),
            "splat".into(),
            path_to_file_uri("/My Work/compute/ds 1/output.ply"),
            0,
        );
        rewrite_output_to_artifact_url(&mut output, work_root, "http://node.local:8092/");
        // The trailing slash on the base is trimmed and the spaced relpath encoded.
        assert_eq!(
            output.uri,
            "http://node.local:8092/artifacts/ds%201/output.ply"
        );
    }

    #[test]
    fn public_base_prefers_the_override() {
        assert_eq!(
            derive_public_base("0.0.0.0:8092", Some("http://compute.example:9000/"), None),
            "http://compute.example:9000"
        );
    }

    #[test]
    fn rewrite_artifact_host_swaps_the_host_and_keeps_the_relpath() {
        // A URL frozen at an earlier hostname is re-served under the live base.
        assert_eq!(
            rewrite_artifact_host(
                "http://stale-host.local:8092/artifacts/recon-1/output.ply",
                "http://192.168.1.5:8092",
            ),
            "http://192.168.1.5:8092/artifacts/recon-1/output.ply"
        );
        // A trailing slash on the base is normalised.
        assert_eq!(
            rewrite_artifact_host(
                "http://x/artifacts/ds-9/output.rrd",
                "http://192.168.1.5:8092/",
            ),
            "http://192.168.1.5:8092/artifacts/ds-9/output.rrd"
        );
        // Non-artifact URLs pass through unchanged.
        assert_eq!(
            rewrite_artifact_host("mock://splat/ds-1", "http://192.168.1.5:8092"),
            "mock://splat/ds-1"
        );
    }

    #[test]
    fn public_base_uses_a_concrete_bind_host() {
        assert_eq!(
            derive_public_base("192.168.1.5:8092", None, Some("node")),
            "http://192.168.1.5:8092"
        );
    }

    #[test]
    fn public_base_substitutes_hostname_for_an_unspecified_bind() {
        assert_eq!(
            derive_public_base("0.0.0.0:8092", None, Some("rtx-box")),
            "http://rtx-box.local:8092"
        );
        // No hostname available: loopback (on-box only, but never a broken host).
        assert_eq!(
            derive_public_base("0.0.0.0:8092", None, None),
            "http://127.0.0.1:8092"
        );
        // A dotted hostname is used as-is (no double .local).
        assert_eq!(
            derive_public_base("[::]:8092", None, Some("node.lan")),
            "http://node.lan:8092"
        );
    }

    #[test]
    fn content_type_covers_the_artifact_kinds() {
        assert_eq!(
            content_type_for(Path::new("a/output.ply")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for(Path::new("a/scene.rrd")),
            "application/octet-stream"
        );
        assert_eq!(content_type_for(Path::new("a/ortho.tif")), "image/tiff");
        assert_eq!(content_type_for(Path::new("a/frame.jpg")), "image/jpeg");
        assert_eq!(
            content_type_for(Path::new("a/meta.json")),
            "application/json"
        );
    }
}
