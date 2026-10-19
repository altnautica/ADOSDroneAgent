//! The ground-station surface. Every handler gates on the resolved profile and
//! answers `404 E_PROFILE_MISMATCH` off a ground station.

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, patch, post, put};
use http::Method;

use crate::routes::table::Routes;
use crate::routes::{
    gs_camera_write, gs_crsf, gs_factory_reset, gs_fleet_hero, gs_input_read, gs_mesh,
    gs_mesh_write, gs_network, gs_network_write, gs_pairing, gs_pic, gs_recording,
    gs_recording_list, gs_relay_proxy, gs_relayed_status, gs_status, gs_tiles, gs_tunnel_config,
    gs_ui_read, gs_ui_write, gs_wfb_invite, gs_wfb_pair, gs_wfb_write, gs_ws,
};
use crate::state::AppState;

/// The relay-proxy lane: an HTTP-shaped request forwarded to a WFB-linked drone
/// the ground station has no IP reach to.
const RELAY_PROXY: &str = "/api/v1/ground-station/relay-proxy/{peer_device_id}/{*path}";

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    let r = radio_and_mesh(r);
    let r = network(r);
    let r = pairing_and_media(r);
    let r = ui_and_input(r);
    relay(r)
}

/// Status, the stored radio config, distributed receive, mesh and WFB pairing.
fn radio_and_mesh(r: Routes<AppState>) -> Routes<AppState> {
    r.get("/api/v1/ground-station/status", gs_status::get_status)
        .get("/api/v1/ground-station/wfb", gs_status::get_wfb)
        .put(
            "/api/v1/ground-station/wfb",
            gs_wfb_write::put_ground_station_wfb,
        )
        .get(
            "/api/v1/ground-station/wfb/relay/status",
            gs_status::get_wfb_relay_status,
        )
        .get(
            "/api/v1/ground-station/wfb/receiver/relays",
            gs_status::get_wfb_receiver_relays,
        )
        .get(
            "/api/v1/ground-station/wfb/receiver/combined",
            gs_status::get_wfb_receiver_combined,
        )
        // The role and capability hint, batman-adv state and its slices, the
        // configured transport and the gateway preference.
        .get("/api/v1/ground-station/role", gs_mesh::get_role)
        .put("/api/v1/ground-station/role", gs_mesh_write::put_role)
        .get("/api/v1/ground-station/mesh", gs_mesh::get_mesh_health)
        .get(
            "/api/v1/ground-station/mesh/neighbors",
            gs_mesh::get_mesh_neighbors,
        )
        .get(
            "/api/v1/ground-station/mesh/routes",
            gs_mesh::get_mesh_routes,
        )
        .get(
            "/api/v1/ground-station/mesh/gateways",
            gs_mesh::get_mesh_gateways,
        )
        .get(
            "/api/v1/ground-station/mesh/config",
            gs_mesh::get_mesh_config,
        )
        .put(
            "/api/v1/ground-station/mesh/config",
            gs_mesh_write::put_mesh_config,
        )
        .put(
            "/api/v1/ground-station/mesh/gateway_preference",
            gs_mesh_write::put_gateway_preference,
        )
        // The WFB rx-key pair and station-wide unpair, the single-slot release,
        // and the phone-receiver invites (relay-forbidden).
        .post(
            "/api/v1/ground-station/wfb/pair",
            gs_wfb_pair::post_wfb_pair,
        )
        .delete(
            "/api/v1/ground-station/wfb/pair",
            gs_wfb_pair::delete_wfb_pair,
        )
        .delete(
            "/api/v1/ground-station/wfb/pair/{device_id}",
            gs_wfb_pair::delete_fleet_slot,
        )
        .get(
            "/api/v1/ground-station/wfb/invite",
            gs_wfb_invite::list_invites,
        )
        .post(
            "/api/v1/ground-station/wfb/invite",
            gs_wfb_invite::post_invite,
        )
        .get(
            "/api/v1/ground-station/wfb/invite/{id}",
            gs_wfb_invite::get_invite,
        )
        .post(
            "/api/v1/ground-station/wfb/invite/{id}/approve",
            gs_wfb_invite::approve_invite,
        )
        .post(
            "/api/v1/ground-station/wfb/invite/{id}/reject",
            gs_wfb_invite::reject_invite,
        )
        // Fleet attention: promote one drone, demote every other slot.
        .post(
            "/api/v1/ground-station/fleet/hero",
            gs_fleet_hero::post_fleet_hero,
        )
        // Return the station to first-boot posture. On-box callers only.
        .post(
            "/api/v1/ground-station/factory-reset",
            gs_factory_reset::post_factory_reset,
        )
}

/// The uplink: aggregate view, ethernet, client scan and join, modem, priority,
/// AP and uplink sharing.
fn network(r: Routes<AppState>) -> Routes<AppState> {
    r.get(
        "/api/v1/ground-station/network",
        gs_network::get_ground_station_network,
    )
    .get(
        "/api/v1/ground-station/network/ethernet",
        gs_network::get_network_ethernet,
    )
    .put(
        "/api/v1/ground-station/network/ethernet",
        gs_network_write::put_network_ethernet,
    )
    .get(
        "/api/v1/ground-station/network/client/scan",
        gs_network::get_network_client_scan,
    )
    .get(
        "/api/v1/ground-station/network/modem",
        gs_network::get_network_modem,
    )
    .put(
        "/api/v1/ground-station/network/modem",
        gs_network_write::put_network_modem,
    )
    .get(
        "/api/v1/ground-station/network/priority",
        gs_network::get_network_priority,
    )
    .put(
        "/api/v1/ground-station/network/priority",
        gs_network_write::put_network_priority,
    )
    .get(
        "/api/v1/ground-station/modem-status",
        gs_network::get_modem_status,
    )
    .put(
        "/api/v1/ground-station/network/ap",
        gs_network_write::put_network_ap,
    )
    .put(
        "/api/v1/ground-station/network/share_uplink",
        gs_network_write::put_network_share_uplink,
    )
    .put(
        "/api/v1/ground-station/network/client/join",
        gs_network_write::put_gs_network_client_join,
    )
    .delete(
        "/api/v1/ground-station/network/client",
        gs_network_write::delete_gs_network_client,
    )
}

/// Mesh pairing, the PIC read, recordings, the camera switch and the field
/// tile packs.
fn pairing_and_media(r: Routes<AppState>) -> Routes<AppState> {
    r.get(
        "/api/v1/ground-station/pair/pending",
        gs_pairing::get_pair_pending,
    )
    .post(
        "/api/v1/ground-station/pair/accept",
        gs_pairing::post_pair_accept,
    )
    .post(
        "/api/v1/ground-station/pair/close",
        gs_pairing::post_pair_close,
    )
    .post(
        "/api/v1/ground-station/pair/approve/{device_id}",
        gs_pairing::post_pair_approve,
    )
    .post(
        "/api/v1/ground-station/pair/revoke/{device_id}",
        gs_pairing::post_pair_revoke,
    )
    .post(
        "/api/v1/ground-station/pair/join",
        gs_pairing::post_pair_join,
    )
    .get("/api/v1/ground-station/pic", gs_pic::get_pic_state)
    // Recordings: the on-disk listing, mediamtx's playback segment list and
    // clip cut (proxied off the loopback playback server), and the per-segment
    // delete. matchit resolves a literal segment ahead of `{segment}`, so
    // `/recording/list` is never read as a name.
    .get(
        "/api/v1/ground-station/recording/list",
        gs_recording_list::get_recording_list,
    )
    .get(
        "/api/v1/ground-station/recording/segments",
        gs_recording_list::get_recording_segments,
    )
    .get(
        "/api/v1/ground-station/recording/clip",
        gs_recording_list::get_recording_clip,
    )
    .delete(
        "/api/v1/ground-station/recording/{segment}",
        gs_recording_list::delete_recording_segment,
    )
    .post(
        "/api/v1/ground-station/recording/start",
        gs_recording::post_recording_start,
    )
    .post(
        "/api/v1/ground-station/recording/stop",
        gs_recording::post_recording_stop,
    )
    .post(
        "/api/v1/ground-station/camera/switch",
        gs_camera_write::post_camera_switch,
    )
    // Field tile packs: the listing and one tile. The last segment is
    // `{y}.mvt`; the router has no static-suffix parameters, so the handler
    // checks the suffix.
    .get("/api/v1/ground-station/tiles", gs_tiles::get_tile_packs)
    .get(
        "/api/v1/ground-station/tiles/{name}/{z}/{x}/{tile}",
        gs_tiles::get_tile,
    )
}

/// The persisted UI and kiosk config, input reads, CRSF, the WebSocket relays
/// and the relayed-node reads.
fn ui_and_input(r: Routes<AppState>) -> Routes<AppState> {
    r.get("/api/v1/ground-station/ui", gs_ui_read::get_ui)
        .put("/api/v1/ground-station/ui/oled", gs_ui_write::put_ui_oled)
        .put(
            "/api/v1/ground-station/ui/buttons",
            gs_ui_write::put_ui_buttons,
        )
        .put(
            "/api/v1/ground-station/ui/screens",
            gs_ui_write::put_ui_screens,
        )
        .get("/api/v1/ground-station/display", gs_ui_read::get_display)
        .put("/api/v1/ground-station/display", gs_ui_write::put_display)
        .get(
            "/api/v1/ground-station/gamepads",
            gs_input_read::get_gamepads,
        )
        .get(
            "/api/v1/ground-station/bluetooth/paired",
            gs_input_read::get_bluetooth_paired,
        )
        // The CRSF RC lane: the staleness-gated state read, channel injection
        // and the RC-module parameter write (forwarded to the lane daemon).
        .get("/api/v1/ground-station/crsf", gs_crsf::get_crsf_status)
        .post(
            "/api/v1/ground-station/crsf/channels",
            gs_crsf::post_crsf_channels,
        )
        .post(
            "/api/v1/ground-station/crsf/params",
            gs_crsf::post_crsf_param_write,
        )
        // WebSocket relays. Each handler enforces the WebSocket auth contract
        // itself (header key or a scoped ticket), because the upgrade bypasses
        // the HTTP auth edge.
        .ws("/api/v1/ground-station/ws/uplink", gs_ws::ws_uplink)
        .ws("/api/v1/ground-station/pic/events", gs_ws::ws_pic_events)
        .ws("/api/v1/ground-station/ws/mesh", gs_ws::ws_mesh)
        .ws("/api/v1/ground-station/ws/buttons", gs_ws::ws_buttons)
        // Config over the radio: the channel state read and the forward of a
        // config request to a radio-linked drone; and what this station knows
        // about the nodes it relays.
        .get(
            "/api/v1/ground-station/relayed/config",
            gs_tunnel_config::get_relayed_config_status,
        )
        .post(
            "/api/v1/ground-station/relayed/config",
            gs_tunnel_config::post_relayed_config,
        )
        .get(
            "/api/v1/ground-station/relayed/status",
            gs_relayed_status::get_relayed_status,
        )
}

/// The relay-proxy lane, registered as the five methods the handler supports
/// rather than `any`, so axum 405s every other method before the handler runs.
/// The body limit is the relay's own request ceiling; the handler answers 413
/// past it.
fn relay(r: Routes<AppState>) -> Routes<AppState> {
    let limit = || DefaultBodyLimit::max(ados_protocol::aux_rpc::MAX_REQUEST_BODY + 1);
    r.method(
        RELAY_PROXY,
        Method::GET,
        get(gs_relay_proxy::handle).layer(limit()),
    )
    .method(
        RELAY_PROXY,
        Method::POST,
        post(gs_relay_proxy::handle).layer(limit()),
    )
    .method(
        RELAY_PROXY,
        Method::PUT,
        put(gs_relay_proxy::handle).layer(limit()),
    )
    .method(
        RELAY_PROXY,
        Method::DELETE,
        delete(gs_relay_proxy::handle).layer(limit()),
    )
    .method(
        RELAY_PROXY,
        Method::PATCH,
        patch(gs_relay_proxy::handle).layer(limit()),
    )
}
