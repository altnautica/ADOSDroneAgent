//! The plugin GCS asset route and its path and content-type resolution.

use super::*;

/// Resolve `asset` under `gcs_root`, refusing anything that could escape it: an
/// absolute path or a `..` component lexically, then a symlink that resolves
/// outside once canonicalized. `Ok(None)` is a path that does not exist or is
/// not a regular file.
pub(super) fn resolve_gcs_asset(gcs_root: &Path, asset: &str) -> Result<Option<PathBuf>, Refusal> {
    let escapes = || {
        Refusal::new(
            12,
            "manifest_invalid",
            "gcs asset path escapes the plugin dir",
            StatusCode::BAD_REQUEST,
        )
    };
    let relative = Path::new(asset);
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(escapes());
    }
    let Ok(root) = gcs_root.canonicalize() else {
        return Ok(None);
    };
    let Ok(target) = root.join(relative).canonicalize() else {
        return Ok(None);
    };
    if !target.starts_with(&root) {
        return Err(escapes());
    }
    Ok(target.is_file().then_some(target))
}

/// The content type a static asset is served with, by extension.
pub(super) fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" | "htm" => "text/html; charset=utf-8",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "yaml" | "yml" => "text/yaml; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `GET /api/plugins/{plugin_id}/gcs/{*asset_path}`: one file from an installed
/// plugin's unpacked `gcs/` dir, so a LAN-paired GCS mounts the GCS half with
/// no cloud. Path-contained: `..`, absolute paths and outward symlinks are
/// refused, and only regular files are served.
pub async fn get_gcs_asset(
    State(state): State<AppState>,
    AxumPath((plugin_id, asset_path)): AxumPath<(String, String)>,
) -> Response {
    let result = state
        .plugins
        .read(move |sup| {
            if sup.find_install(&plugin_id).is_none() || !is_plugin_id(&plugin_id) {
                return Err(Refusal::not_installed(&plugin_id));
            }
            let gcs_root = sup.paths().install_dir.join(&plugin_id).join("gcs");
            let target = resolve_gcs_asset(&gcs_root, &asset_path)?
                .ok_or_else(|| Refusal::not_found(format!("gcs asset {asset_path:?} not found")))?;
            let body = std::fs::read(&target)
                .map_err(|e| Refusal::host_io(format!("read of gcs asset failed: {e}")))?;
            Ok((content_type_for(&target), body))
        })
        .await;
    respond(result.map(|(content_type, body)| {
        (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response()
    }))
}
