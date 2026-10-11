//! The plugin surface: lifecycle, per-drone config, published state, MCP tools
//! and the passthrough to a plugin's own HTTP API.

use axum::extract::DefaultBodyLimit;
use axum::routing::post;
use http::Method;

use crate::routes::table::Routes;
use crate::routes::{
    plugins_config, plugins_lifecycle, plugins_proxy, plugins_state, plugins_tools,
};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // Per-drone config: the GET reads the live store back so a reloaded GCS
    // can hand the plugin's UI its current settings; the PUT flips `active` or
    // settings in the live plugin host. 503 when the plugin host is not up.
    r.get(
        "/api/plugins/{plugin_id}/config",
        plugins_config::get_plugin_config,
    )
    .put(
        "/api/plugins/{plugin_id}/config",
        plugins_config::put_plugin_config,
    )
    // An MCP client runs a plugin's declared tool through the plugin host.
    .post(
        "/api/plugins/{plugin_id}/tools/{tool}/invoke",
        plugins_tools::invoke_plugin_tool,
    )
    // A plugin's latest published state per topic, from its state sidecar.
    .get(
        "/api/plugins/{plugin_id}/state",
        plugins_state::get_plugin_state,
    )
    // The lifecycle: catalog, list and detail, the GCS asset, manifest,
    // attestation and readiness reads, parse and install (upload, URL,
    // built-in), grant, revoke, enable, disable, remove, the update
    // preferences, the capability token and the install-job progress stream.
    // The two upload routes take an archive-sized body; the handler still caps
    // the file.
    .get("/api/v1/plugins/catalog", plugins_lifecycle::get_catalog)
    .get("/api/plugins", plugins_lifecycle::list_plugins)
    .method(
        "/api/plugins/parse",
        Method::POST,
        post(plugins_lifecycle::parse_plugin)
            .layer(DefaultBodyLimit::max(plugins_lifecycle::UPLOAD_BODY_LIMIT)),
    )
    .method(
        "/api/plugins/install",
        Method::POST,
        post(plugins_lifecycle::install_plugin)
            .layer(DefaultBodyLimit::max(plugins_lifecycle::UPLOAD_BODY_LIMIT)),
    )
    .post(
        "/api/plugins/parse_from_url",
        plugins_lifecycle::parse_from_url,
    )
    .post(
        "/api/plugins/install_from_url",
        plugins_lifecycle::install_from_url,
    )
    .post(
        "/api/plugins/install_builtin",
        plugins_lifecycle::install_builtin,
    )
    .post(
        "/api/plugins/capability-token",
        plugins_lifecycle::mint_capability_token,
    )
    .ws(
        "/api/plugins/jobs/{job_id}",
        plugins_lifecycle::stream_install_job,
    )
    .get("/api/plugins/{plugin_id}", plugins_lifecycle::get_plugin)
    .delete("/api/plugins/{plugin_id}", plugins_lifecycle::remove_plugin)
    .get(
        "/api/plugins/{plugin_id}/gcs/{*asset_path}",
        plugins_lifecycle::get_gcs_asset,
    )
    .get(
        "/api/plugins/{plugin_id}/locales/{lang}",
        plugins_lifecycle::get_locale,
    )
    .get(
        "/api/plugins/{plugin_id}/manifest",
        plugins_lifecycle::get_manifest,
    )
    .get(
        "/api/plugins/{plugin_id}/attestation",
        plugins_lifecycle::get_attestation,
    )
    .get(
        "/api/plugins/{plugin_id}/readiness",
        plugins_lifecycle::get_readiness,
    )
    .post(
        "/api/plugins/{plugin_id}/grant",
        plugins_lifecycle::grant_permission,
    )
    .delete(
        "/api/plugins/{plugin_id}/perms/{permission_id}",
        plugins_lifecycle::revoke_permission,
    )
    .post(
        "/api/plugins/{plugin_id}/enable",
        plugins_lifecycle::enable_plugin,
    )
    .post(
        "/api/plugins/{plugin_id}/disable",
        plugins_lifecycle::disable_plugin,
    )
    .post(
        "/api/plugins/{plugin_id}/pin",
        plugins_lifecycle::pin_plugin,
    )
    .post(
        "/api/plugins/{plugin_id}/unpin",
        plugins_lifecycle::unpin_plugin,
    )
    .post(
        "/api/plugins/{plugin_id}/auto-update",
        plugins_lifecycle::set_auto_update,
    )
    // A plugin's own HTTP API (`agent.http: true`), every method and a
    // WebSocket upgrade, forwarded to its `plugin-http/<id>/http.sock`.
    .get(
        "/api/plugins/{plugin_id}/x/{*rest}",
        plugins_proxy::plugin_http,
    )
    .post(
        "/api/plugins/{plugin_id}/x/{*rest}",
        plugins_proxy::plugin_http,
    )
    .put(
        "/api/plugins/{plugin_id}/x/{*rest}",
        plugins_proxy::plugin_http,
    )
    .patch(
        "/api/plugins/{plugin_id}/x/{*rest}",
        plugins_proxy::plugin_http,
    )
    .delete(
        "/api/plugins/{plugin_id}/x/{*rest}",
        plugins_proxy::plugin_http,
    )
}
