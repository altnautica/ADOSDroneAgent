//! The route surface: one axum `Router` served on both listeners.
//!
//! The native control surface answers the same `/api/*` (+ `/healthz`) routes
//! the FastAPI surface does, byte-identically, so the same GCS works against
//! either. This surface registers `/healthz`, `/api/version`, `/api/status`,
//! `/api/telemetry`, `/api/time`, `/api/params`, `/api/services`, the two
//! `/api/fleet/*` routes, the three `/api/mavlink/signing/*` reads, the four
//! `/api/wfb*` reads, the four `/api/pairing/*` routes, and the two
//! `/api/command{,s}` routes. Every other path falls through to the proxy.
//!
//! Error bodies use FastAPI's `{"detail": "..."}` shape on 4xx/5xx, NOT the
//! logd read-API's `{"error": {...}}` envelope, because the GCS already parses
//! the agent's `{"detail"}` errors. The proxy fallback and the [`detail`] helper
//! enforce that one shape everywhere on this surface.
//!
//! INVARIANT: every route registered in [`build_router`] MUST have a matching
//! entry in [`crate::routing::native_routes`]. The LAN-edge auth applies its
//! posture only to native paths; a route served here but missing from the native
//! set would be served with the auth SKIPPED. The `native_set_matches_router`
//! test pins the full set so the two never drift.

pub mod atlas;
pub mod camera_config;
pub mod can;
pub mod command;
pub mod compute_status;
pub mod config_schema;
pub mod dashboard_pin;
pub mod diagnostics;
pub mod fleet;
pub mod gs_bluetooth;
pub mod gs_camera_write;
pub mod gs_cmd;
pub mod gs_crsf;
pub mod gs_gamepad_write;
pub mod gs_input_read;
pub mod gs_mesh;
pub mod gs_mesh_write;
pub mod gs_network;
pub mod gs_network_write;
pub mod gs_pairing;
pub mod gs_pic;
pub mod gs_recording;
pub mod gs_recording_list;
pub mod gs_status;
pub mod gs_tunnel_config;
pub mod gs_ui_read;
pub mod gs_ui_write;
pub mod gs_wfb_pair;
pub mod gs_wfb_write;
pub mod gs_ws;
pub mod guided;
pub mod logs_write;
pub mod mac_adapters;
pub mod mac_pin;
pub mod mavlink_ports;
pub mod mcp;
pub mod network_client_read;
pub mod network_write;
pub mod pairing;
pub mod params;
pub mod params_single;
pub mod params_write;
pub mod plugins_config;
pub mod plugins_state;
pub mod plugins_tools;
pub mod service_control;
pub mod services;
pub mod signing;
pub mod signing_write;
pub mod status;
pub mod status_full;
pub mod system;
pub mod system_resources;
pub mod video;
pub mod vision;
pub mod vision_detector;
pub mod vision_upload;
pub mod wfb;
pub mod wfb_pair_write;
pub mod wfb_write;
pub mod ws_ticket;

use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::json;

use crate::proxy::proxy_to_residual;
use crate::state::AppState;

/// Build a FastAPI-shaped error response: `(status, {"detail": message})`. Every
/// 4xx/5xx on this surface goes through this so the body shape never drifts to
/// the logd `{"error":{...}}` envelope. Used by the routes that land in later
/// chunks (pairing 409s, command 503/400) as well as the proxy's
/// graceful-degradation reply.
pub fn detail(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": message.into() }))).into_response()
}

/// Build the route Router for a given app state. The same Router is served on
/// both edges; the auth/rate-limit layer is added per edge by the serve loop.
/// `/healthz` sits at the root; everything else is mounted under `/api`.
///
/// Any path not registered here falls through to the reverse-proxy fallback,
/// which forwards it to the residual Python over its internal Unix socket (and
/// degrades cleanly to a FastAPI-shaped `{"detail"}` when that upstream is
/// absent), so the front serves the migrated routes natively and proxies the
/// rest while the migration is in flight.
pub fn build_router(state: AppState, net_native: bool, hid_native: bool) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(system::healthz))
        .route("/api/version", get(system::get_version))
        .route("/api/status", get(status::get_status))
        .route("/api/telemetry", get(status::get_telemetry))
        .route("/api/time", get(system::get_time))
        // Control-plane RTT echo: a cheap FC-independent `{pong: epoch_ms}` the
        // GCS times its round-trip around to surface controlRttMs next to the
        // link badge. Public (no key), never touches the FC.
        .route("/api/ping", get(system::get_ping))
        // FC-source picker enumeration: the serial devices an FC could be on, for
        // the setup webapp + GCS dropdown (a filesystem scan, never a probe).
        .route("/api/mavlink/ports", get(mavlink_ports::list_ports))
        // Agent-config JSON Schema: the committed, build-time-embedded shape of
        // the config surface (types/enums/defaults + x-secret markers) so a
        // schema-driven settings UI renders without hand-typed forms. Shape
        // only, no live values; the values read stays on /api/config.
        .route("/api/config/schema", get(config_schema::get_config_schema))
        // Pairing: the node-identity probe + the local pairing handshake. info /
        // code / claim are public (the auth-exempt set); unpair requires the key.
        .route("/api/pairing/info", get(pairing::get_pairing_info))
        .route("/api/pairing/code", get(pairing::get_pairing_code))
        .route("/api/pairing/claim", post(pairing::claim_pairing))
        .route("/api/pairing/unpair", post(pairing::unpair))
        // Command: the fire-and-forget text-command executor (auth-gated when
        // paired) + the catalog. The executor builds a MAVLink frame and writes
        // it to the mavlink socket; the catalog is the static command list.
        .route("/api/command", post(command::execute_command))
        .route("/api/commands", get(command::list_commands))
        // CAN passthrough: a deliberate 501 stub (no agent-side bridge yet). The
        // GCS probes it and falls back to the MAVLink CAN_FORWARD path.
        .route("/api/can/passthrough", post(can::can_passthrough))
        // Operator cloud-export trigger: writes a push-request file the cloud
        // service consumes and reports the outcome. A thin trigger, no store read.
        .route("/api/logs/push", post(logs_write::push_logs))
        // Vision designate: operator click-to-follow. Locks the vision engine's
        // tracker for a camera onto the box the operator clicked, via the vision
        // socket. Auth-gated when paired (a write), 503 when vision is not up.
        .route("/api/vision/designate", post(vision::designate))
        // Vision engine status: the registered-model read-back for the GCS vision
        // hub, so it shows every model loaded on the drone (task, execution,
        // backend-loaded), not only the ones actively publishing detections. A
        // read; 503 when vision is not up.
        .route("/api/vision/status", get(vision::engine_status))
        // Vision capabilities: what perception this node offers, grouped by task
        // (kind) with their classes + inference-capability, or a single-capability
        // resolution via ?kind=&class=. A read; 503 when vision is not up.
        .route("/api/vision/capabilities", get(vision::engine_capabilities))
        // Vision detector selection: pick (PUT) / clear (DELETE) the model the
        // engine auto-loads. The write merges the vision.detector config block
        // (model_id + enabled, optional model_path) and restarts ados-vision so
        // the new detector takes effect. Auth-gated when paired (a write).
        .route(
            "/api/vision/detector",
            put(vision_detector::put_detector).delete(vision_detector::delete_detector),
        )
        // Vision custom-model upload: a multipart file + metadata streamed into
        // the models dir, hashed, and recorded in the custom-model catalog so the
        // model manager + the models-list route surface it. Auth-gated when paired.
        // The default 2 MB multipart body cap is lifted for this one route — a real
        // detector file (a yolov8n `.onnx` is ~12 MB, a `.rknn` several MB) exceeds
        // it, and this route's whole purpose is sideloading those files. The handler
        // streams the field to disk chunk-by-chunk, so disabling the cap does not
        // buffer the model in RAM; the on-disk size is bounded by the partition.
        .route(
            "/api/vision/models/upload",
            post(vision_upload::upload_model).layer(DefaultBodyLimit::disable()),
        )
        // Plugin per-drone config write: a GCS skill toggle / per-drone settings
        // change flips a plugin's `active`/settings config in the live plugin
        // host (the daemon's control socket). Auth-gated when paired (a write),
        // 503 when the plugin host is not up. The plugin READ routes stay proxied.
        .route(
            "/api/plugins/:plugin_id/config",
            put(plugins_config::put_plugin_config),
        )
        // Plugin MCP-tool invocation: an MCP client runs a plugin's declared tool
        // through the plugin host's control socket and gets the result. A native
        // write; 503 when the plugin host is not up. The connector gates the
        // tool's safety class; the plugin host gates on the plugin's mcp.expose.
        .route(
            "/api/plugins/:plugin_id/tools/:tool/invoke",
            post(plugins_tools::invoke_plugin_tool),
        )
        // Plugin published-state read: a plugin's latest state per topic, read
        // from the plugin host's per-plugin state sidecar so a LAN-paired GCS can
        // poll the plugin's own published state (a follow read-back, etc.). A
        // path-param route under the otherwise-proxied /api/plugins prefix; only
        // this exact GET is served natively, the rest of /api/plugins proxies.
        .route(
            "/api/plugins/:plugin_id/state",
            get(plugins_state::get_plugin_state),
        )
        // The compute node's cluster status, read from its heartbeat sidecar, so
        // a LAN-paired GCS renders the compute-cluster card local-first (Rule 39).
        .route(
            "/api/compute/status",
            get(compute_status::get_compute_status),
        )
        // ADOS Atlas per-drone world-model capture: readiness (drone-local facts
        // + live session state), the per-drone enable/config write, and the live
        // capture-session controls forwarded to the capture service's socket.
        .route("/api/atlas/readiness", get(atlas::get_atlas_readiness))
        .route("/api/atlas/config", put(atlas::put_atlas_config))
        .route("/api/atlas/capture/start", post(atlas::post_capture_start))
        .route("/api/atlas/capture/stop", post(atlas::post_capture_stop))
        .route("/api/atlas/capture/pause", post(atlas::post_capture_pause))
        .route(
            "/api/atlas/capture/resume",
            post(atlas::post_capture_resume),
        )
        // WebSocket auth ticket mint: exchanges the pairing key (LAN-edge auth)
        // for a short-lived self-contained HMAC ticket a browser GCS hands to the
        // MAVLink WS proxy through the subprotocol list.
        .route("/api/_ws/ticket", post(ws_ticket::mint_ws_ticket))
        // Dashboard-access PIN: the off-box browser gate. status + verify + set
        // are public at the edge (an off-box paired visitor must reach them);
        // set authorizes in the handler (on-box / key / session / trust-on-first-
        // use / current-PIN), and clear stays behind the normal gate (on-box or a
        // valid credential). A correct PIN mints a session the front accepts as an
        // alternative data-plane credential to X-ADOS-Key.
        .route(
            "/api/dashboard/pin/status",
            get(dashboard_pin::get_pin_status),
        )
        .route("/api/dashboard/pin/verify", post(dashboard_pin::verify_pin))
        .route("/api/dashboard/pin/set", post(dashboard_pin::set_pin))
        .route("/api/dashboard/pin/clear", post(dashboard_pin::clear_pin))
        // MCP-token management (AI-control surface). status is a read; mint issues
        // a credential (authorized in the handler to on-box or a valid key); revoke
        // needs the admin scope class. The edge honors a minted token only when the
        // mcp.token_accept_enabled flag is set (default off, reported by status).
        .route("/api/mcp/status", get(mcp::get_mcp_status))
        .route("/api/mcp/tokens", post(mcp::mint_mcp_token))
        .route("/api/mcp/revoke", post(mcp::revoke_mcp_token))
        // Params: the full cached FC parameter list, plus the single-param read +
        // write sharing one path (a path-param route; the write builds a PARAM_SET
        // frame and sends it to the FC, the read projects the one cached param).
        .route("/api/params", get(params::get_all_params))
        .route(
            // axum 0.7 path params use the `:name` form; the `{name}` form is a
            // literal segment here and would never match a real value (the
            // request would fall through to the reverse-proxy fallback).
            "/api/params/:name",
            get(params_single::get_param).post(params_write::set_param),
        )
        // Services: the live `ados-*.service` unit inventory + the per-unit restart
        // (allowlist-guarded to ados-* units) and the supervisor restart that
        // cycles the whole agent process tree.
        .route("/api/services", get(services::list_services))
        .route(
            "/api/services/:name/restart",
            post(service_control::restart_service),
        )
        .route(
            "/api/v1/system/restart-supervisor",
            post(service_control::restart_supervisor),
        )
        // Fleet roster: the opt-in mesh awareness surface. Both static on this
        // device — enrollment reports not-enrolled, peers is the empty list.
        .route("/api/fleet/enrollment", get(fleet::get_enrollment))
        .route("/api/fleet/peers", get(fleet::list_peers))
        // MAVLink v2 signing: FC capability, the require-flag value (GET) + toggle
        // (PUT, on the same path the read uses), the observational counters, and
        // the enroll/disable writes that push a key to the FC and clear its store.
        .route("/api/mavlink/signing/capability", get(signing::capability))
        .route(
            "/api/mavlink/signing/require",
            get(signing::require).put(signing_write::require),
        )
        .route("/api/mavlink/signing/counters", get(signing::counters))
        .route(
            "/api/mavlink/signing/enroll-fc",
            post(signing_write::enroll_fc),
        )
        .route(
            "/api/mavlink/signing/disable-on-fc",
            post(signing_write::disable_on_fc),
        )
        // WFB radio reads: link status, link-quality history, pair-state, and the
        // failover state (the channel / tx-power writes stay proxied).
        .route("/api/wfb", get(wfb::get_wfb_status))
        .route("/api/wfb/history", get(wfb::get_wfb_history))
        .route("/api/wfb/pair", get(wfb::get_wfb_pair_status))
        .route(
            "/api/wfb/pair/failover-status",
            get(wfb::get_failover_status),
        )
        // WFB radio writes: the channel change (a coordinated hop to the radio
        // command socket) and the runtime TX-power (set + persist).
        .route("/api/wfb/channel", post(wfb_write::set_wfb_channel))
        .route("/api/wfb/tx-power", put(wfb_write::set_wfb_tx_power))
        // WFB auto-pair toggle: persists the video.wfb.auto_pair_enabled arm flag
        // after reading the live pair status (a surgical config merge; a re-arm on
        // a paired rig is refused without a persist).
        .route(
            "/api/wfb/pair/auto-pair",
            put(wfb_pair_write::put_auto_pair),
        )
        // The consolidated status: agent info, services, resources, video,
        // telemetry, radio, and mesh in one round-trip.
        .route("/api/status/full", get(status_full::get_full_status))
        // System resources: CPU / memory / swap / disk / per-sensor temperatures
        // from the logging store's hardware snapshot (the LCD + GCS resource read).
        .route("/api/system", get(system_resources::get_system_resources))
        // The composite triage snapshot: agent identity + board summary + system
        // resources + network + device id + the last ados-agent log lines, for the
        // LCD Diagnostics drilldown and the GCS remote-display pane.
        .route("/api/v1/diagnostics", get(diagnostics::get_diagnostics))
        // The per-hop video-pipeline verifier: samples reliable cumulative/rate
        // counters (mediamtx bytesReceived, wfb_rx decode, fan-out, WHEP) twice
        // and attributes a stall to the exact hop that stopped, profile-aware.
        .route("/api/diag/video", get(diagnostics::get_video_diagnostics))
        // Video reads: glass-to-glass latency, the air-side pipeline snapshot, and
        // the encoder/radio config (the snapshot/record/switch writes + the
        // camera-enumeration route stay proxied).
        .route("/api/video/latency", get(video::get_video_latency))
        .route(
            "/api/v1/video/air-pipeline",
            get(video::get_air_pipeline_status),
        )
        .route("/api/video/config", get(video::get_video_config))
        // Camera roster: the reconciled per-node camera list (declared legs +
        // discovered devices + live stream state) the Cameras management surface
        // renders (GET, guaranteed 200), plus the operator write that persists the
        // leg list (PUT → the supervisor's merge-by-owner persist + restart). This
        // lives at /api/video/roster, distinct from the legacy /api/video/cameras
        // switchable-camera enumeration ({cameras, assignments}) which the ground
        // station's camera switch still serves from the residual API.
        .route(
            "/api/video/roster",
            get(camera_config::get_video_cameras).put(camera_config::put_video_cameras),
        )
        // Ground-station profile reads (404 off a drone): the status snapshot, the
        // stored radio config, and the three distributed-receive role reads.
        .route("/api/v1/ground-station/status", get(gs_status::get_status))
        .route(
            "/api/v1/ground-station/wfb",
            get(gs_status::get_wfb).put(gs_wfb_write::put_ground_station_wfb),
        )
        .route(
            "/api/v1/ground-station/wfb/relay/status",
            get(gs_status::get_wfb_relay_status),
        )
        .route(
            "/api/v1/ground-station/wfb/atlas-relay/status",
            get(gs_status::get_atlas_relay_status),
        )
        .route(
            "/api/v1/ground-station/wfb/receiver/relays",
            get(gs_status::get_wfb_receiver_relays),
        )
        .route(
            "/api/v1/ground-station/wfb/receiver/combined",
            get(gs_status::get_wfb_receiver_combined),
        )
        // Ground-station mesh reads (profile-gated): the role + capability hint,
        // the batman-adv state + its three slices, and the configured transport.
        .route(
            "/api/v1/ground-station/role",
            get(gs_mesh::get_role).put(gs_mesh_write::put_role),
        )
        .route("/api/v1/ground-station/mesh", get(gs_mesh::get_mesh_health))
        .route(
            "/api/v1/ground-station/mesh/neighbors",
            get(gs_mesh::get_mesh_neighbors),
        )
        .route(
            "/api/v1/ground-station/mesh/routes",
            get(gs_mesh::get_mesh_routes),
        )
        .route(
            "/api/v1/ground-station/mesh/gateways",
            get(gs_mesh::get_mesh_gateways),
        )
        .route(
            "/api/v1/ground-station/mesh/config",
            get(gs_mesh::get_mesh_config).put(gs_mesh_write::put_mesh_config),
        )
        // Ground-station network uplink reads (404 off a drone): the aggregate
        // view, ethernet, client scan, modem, the priority list, and cellular.
        .route(
            "/api/v1/ground-station/network",
            get(gs_network::get_ground_station_network),
        )
        .route(
            "/api/v1/ground-station/network/ethernet",
            get(gs_network::get_network_ethernet).put(gs_network_write::put_network_ethernet),
        )
        .route(
            "/api/v1/ground-station/network/client/scan",
            get(gs_network::get_network_client_scan),
        )
        .route(
            "/api/v1/ground-station/network/modem",
            get(gs_network::get_network_modem).put(gs_network_write::put_network_modem),
        )
        .route(
            "/api/v1/ground-station/network/priority",
            get(gs_network::get_network_priority).put(gs_network_write::put_network_priority),
        )
        .route(
            "/api/v1/ground-station/modem-status",
            get(gs_network::get_modem_status),
        )
        // Ground-station reads (profile-gated): the mesh pairing snapshot, the PIC
        // arbiter state, and the captive-portal token mint.
        .route(
            "/api/v1/ground-station/pair/pending",
            get(gs_pairing::get_pair_pending),
        )
        .route("/api/v1/ground-station/pic", get(gs_pairing::get_pic_state))
        .route(
            "/api/v1/ground-station/captive-token",
            get(gs_pairing::get_captive_token),
        )
        // Ground-station recordings listing (profile-gated 404 off a drone).
        .route(
            "/api/v1/ground-station/recording/list",
            get(gs_recording_list::get_recording_list),
        )
        // Ground-station persisted-UI reads (profile-gated): the OLED/button/screen
        // UI config blob and the HDMI kiosk display config.
        .route("/api/v1/ground-station/ui", get(gs_ui_read::get_ui))
        .route(
            "/api/v1/ground-station/display",
            get(gs_ui_read::get_display).put(gs_ui_write::put_display),
        )
        // Ground-station input reads (profile-gated): attached controllers + the
        // primary selection, and the paired Bluetooth devices.
        .route(
            "/api/v1/ground-station/gamepads",
            get(gs_input_read::get_gamepads),
        )
        .route(
            "/api/v1/ground-station/bluetooth/paired",
            get(gs_input_read::get_bluetooth_paired),
        )
        // Ground-station WebSocket relays (profile-gated): the uplink-matrix
        // change stream (polled off the durable store), the PIC arbiter's
        // transition stream (relayed off the native control socket), the mesh
        // + pairing event stream (fanned off the two cross-process journals), and
        // the front-panel button stream (relayed off the dedicated button socket
        // so the HDMI cockpit web app is driven by the GPIO buttons). Each handler
        // enforces the WebSocket auth contract itself (header key OR a scoped
        // ticket) because the upgrade bypasses the HTTP auth edge; the matching
        // paths are in the public-exempt set so the edge passes the handshake
        // through to the handler.
        .route("/api/v1/ground-station/ws/uplink", get(gs_ws::ws_uplink))
        .route(
            "/api/v1/ground-station/pic/events",
            get(gs_ws::ws_pic_events),
        )
        .route("/api/v1/ground-station/ws/mesh", get(gs_ws::ws_mesh))
        .route("/api/v1/ground-station/ws/buttons", get(gs_ws::ws_buttons))
        // Wi-Fi client reads (profile-agnostic): the live station status off the
        // uplink daemon's command socket, and the saved NM profiles. The scan stays
        // proxied (its rescan is a side effect with no daemon-socket op).
        .route(
            "/api/v1/network/client/status",
            get(network_client_read::get_client_status),
        )
        .route(
            "/api/v1/network/client/configured",
            get(network_client_read::get_client_configured),
        )
        // MAC-pin read: the per-adapter stable-MAC verdicts from the on-disk state
        // file (a pure read, the same file the pin write resolves a candidate from).
        .route(
            "/api/v1/network/mac/adapters",
            get(mac_adapters::get_mac_adapters),
        )
        // MAC-pin writes: pin a stable MAC for an adapter (POST) and clear it
        // (DELETE). Each merges the mac_pin config the supervisor reconciler reads
        // and drives the shared mac-pin engine for the .link removal + gated re-tag.
        .route("/api/v1/network/mac/pin", post(mac_pin::post_mac_pin))
        .route(
            "/api/v1/network/mac/:iface",
            delete(mac_pin::delete_mac_pin),
        )
        // Ground-station network writes: AP config + share-uplink toggle
        // (ethernet/modem PUTs share their read paths).
        .route(
            "/api/v1/ground-station/network/ap",
            put(gs_network_write::put_network_ap),
        )
        .route(
            "/api/v1/ground-station/network/share_uplink",
            put(gs_network_write::put_network_share_uplink),
        )
        // Ground-station mesh + WFB-pair writes (role/mesh-config PUTs share their
        // read paths): gateway preference + the WFB rx-key pair/unpair.
        .route(
            "/api/v1/ground-station/mesh/gateway_preference",
            put(gs_mesh_write::put_gateway_preference),
        )
        .route(
            "/api/v1/ground-station/wfb/pair",
            post(gs_wfb_pair::post_wfb_pair).delete(gs_wfb_pair::delete_wfb_pair),
        )
        // Ground-station video writes: recording start/stop + the camera-source switch.
        .route(
            "/api/v1/ground-station/recording/start",
            post(gs_recording::post_recording_start),
        )
        .route(
            "/api/v1/ground-station/recording/stop",
            post(gs_recording::post_recording_stop),
        )
        .route(
            "/api/v1/ground-station/camera/switch",
            post(gs_camera_write::post_camera_switch),
        )
        // Ground-station UI config writes (display PUT shares its read path): OLED +
        // buttons + screens, each persisted + SIGHUP to the display service.
        .route(
            "/api/v1/ground-station/ui/oled",
            put(gs_ui_write::put_ui_oled),
        )
        .route(
            "/api/v1/ground-station/ui/buttons",
            put(gs_ui_write::put_ui_buttons),
        )
        .route(
            "/api/v1/ground-station/ui/screens",
            put(gs_ui_write::put_ui_screens),
        )
        // Ground-station CRSF RC-lane surface (profile-gated): the lane state
        // sidecar (staleness-gated read), the programmatic channel injection,
        // and the RC-module parameter write — the writes forward to the lane
        // daemon's command socket (the owner of the live channel merge).
        .route("/api/v1/ground-station/crsf", get(gs_crsf::get_crsf_status))
        .route(
            "/api/v1/ground-station/crsf/channels",
            post(gs_crsf::post_crsf_channels),
        )
        .route(
            "/api/v1/ground-station/crsf/params",
            post(gs_crsf::post_crsf_param_write),
        )
        // Config-over-radio (relayed config): read the channel state sidecar,
        // and forward a config request to a radio-linked drone's /api/config
        // over the bearer. The seam the GCS calls for the "drone via ground
        // node" case.
        .route(
            "/api/v1/ground-station/relayed/config",
            get(gs_tunnel_config::get_relayed_config_status)
                .post(gs_tunnel_config::post_relayed_config),
        );

    // Wi-Fi client writes (profile-agnostic) are served natively only where the
    // ados-net uplink daemon runs (a ground station); elsewhere the route is not
    // registered and falls through to the residual's in-process nmcli handler.
    if net_native {
        router = router
            .route(
                "/api/v1/network/client/join",
                put(network_write::put_client_join),
            )
            .route(
                "/api/v1/network/client",
                delete(network_write::delete_client),
            )
            .route(
                "/api/v1/network/client/configured/:name",
                delete(network_write::delete_client_configured),
            )
            .route(
                "/api/v1/network/client/configured/:name/autoconnect",
                put(network_write::put_client_autoconnect),
            );
    }

    // PIC arbiter + gamepad + Bluetooth writes reach the Rust ados-pic / ados-input
    // daemons over their sockets, which exist only when hid-rust is enabled;
    // otherwise the routes fall through to the residual's in-process handlers.
    if hid_native {
        router = router
            .route(
                "/api/v1/ground-station/pic/claim",
                post(gs_pic::post_pic_claim),
            )
            .route(
                "/api/v1/ground-station/pic/release",
                post(gs_pic::post_pic_release),
            )
            .route(
                "/api/v1/ground-station/pic/confirm-token",
                post(gs_pic::post_pic_confirm_token),
            )
            .route(
                "/api/v1/ground-station/pic/heartbeat",
                post(gs_pic::post_pic_heartbeat),
            )
            .route(
                "/api/v1/ground-station/gamepads/primary",
                put(gs_gamepad_write::put_gamepad_primary),
            )
            .route(
                "/api/v1/ground-station/bluetooth/scan",
                post(gs_bluetooth::post_bluetooth_scan),
            )
            .route(
                "/api/v1/ground-station/bluetooth/pair",
                post(gs_bluetooth::post_bluetooth_pair),
            )
            .route(
                "/api/v1/ground-station/bluetooth/:mac",
                delete(gs_bluetooth::delete_bluetooth),
            );
    }

    // Everything else: reverse-proxy to the residual Python.
    router.fallback(proxy_to_residual).with_state(state)
}

#[cfg(test)]
mod param_syntax_tests {
    /// axum 0.7 path parameters use the `:name` form. A `{name}` literal in a
    /// `.route(...)` path is matched verbatim and never binds a real value, so
    /// the request silently falls through to the reverse-proxy fallback — the
    /// route is registered for auth but never served natively. Guard against
    /// reintroducing the 0.8 `{param}` form until the crate moves to axum 0.8.
    #[test]
    fn route_paths_use_axum_07_param_syntax() {
        let src = include_str!("mod.rs");
        let offenders: Vec<&str> = src
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("\"/api") && l.contains('{'))
            .collect();
        assert!(
            offenders.is_empty(),
            "route path(s) use {{param}} (axum 0.8) syntax; axum 0.7 needs :param: {offenders:?}"
        );
    }
}
