//! Node identity, health, FC reads and commands, services, fleet, signing and
//! the composite status and diagnostics reads.

use crate::routes::table::Routes;
use crate::routes::{
    battery, can, cloud_link, command, dashboard, diag_storage, diagnostics, fleet, mavlink_ports,
    params, params_single, params_write, relay_secret, service_control, services, signing,
    signing_write, status, status_full, swarm, system, system_resources, ws_ticket,
};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.get("/healthz", system::healthz)
        .get("/api/version", system::get_version)
        .get("/api/status", status::get_status)
        .get("/api/telemetry", status::get_telemetry)
        .get("/api/time", system::get_time)
        // Control-plane RTT echo: a cheap FC-independent `{pong: epoch_ms}` the
        // GCS times its round-trip around. Public (no key), never touches the FC.
        .get("/api/ping", system::get_ping)
        // FC-source picker enumeration: the serial devices an FC could be on (a
        // filesystem scan, never a probe).
        .get("/api/mavlink/ports", mavlink_ports::list_ports)
        // The fire-and-forget text-command executor (auth-gated when paired)
        // and the static command catalog.
        .post("/api/command", command::execute_command)
        .get("/api/commands", command::list_commands)
        // CAN passthrough: a deliberate 501 (no agent-side bridge). The GCS
        // probes it and falls back to the MAVLink CAN_FORWARD path.
        .post("/api/can/passthrough", can::can_passthrough)
        // The cached FC parameter list, plus the single-param read and write
        // sharing one path (the write sends a PARAM_SET to the FC).
        .get("/api/params", params::get_all_params)
        .get("/api/params/{name}", params_single::get_param)
        .post("/api/params/{name}", params_write::set_param)
        // The `ados-*.service` inventory, the allowlisted per-unit restart and
        // the supervisor restart that cycles the whole agent process tree.
        .get("/api/services", services::list_services)
        .post(
            "/api/services/{name}/restart",
            service_control::restart_service,
        )
        .post(
            "/api/v1/system/restart-supervisor",
            service_control::restart_supervisor,
        )
        // Fleet roster and the relay peer secret.
        .post("/api/relay/peer-secret", relay_secret::post_peer_secret)
        .get("/api/fleet/enrollment", fleet::get_enrollment)
        .get("/api/fleet/peers", fleet::list_peers)
        // MAVLink v2 signing: FC capability, counters, and the enroll/disable
        // writes that push a key to the FC and clear its store.
        .get("/api/mavlink/signing/capability", signing::capability)
        .get("/api/mavlink/signing/counters", signing::counters)
        .post("/api/mavlink/signing/enroll-fc", signing_write::enroll_fc)
        .post(
            "/api/mavlink/signing/disable-on-fc",
            signing_write::disable_on_fc,
        )
        // WebSocket auth ticket mint: exchanges the pairing key for a
        // short-lived HMAC ticket a browser hands to a WebSocket handshake.
        .post("/api/_ws/ticket", ws_ticket::mint_ws_ticket)
        // The consolidated status and the agent webapp's one-pager poll.
        .get("/api/status/full", status_full::get_full_status)
        .get(
            "/api/v1/dashboard/snapshot",
            dashboard::get_dashboard_snapshot,
        )
        // The swarm neighbour table. Profile-agnostic: a drone answers it with
        // the ground station powered off.
        .get("/api/swarm/neighbors", swarm::get_neighbors)
        // Per-pack battery health.
        .get("/api/v1/battery", battery::get_battery)
        // Whether the cloud relay is talking to the cloud.
        .get("/api/cloud/link", cloud_link::get_cloud_link)
        // System resources, the composite triage snapshot, the per-hop video
        // verifier and the storage-wear verdict.
        .get("/api/system", system_resources::get_system_resources)
        .get("/api/v1/diagnostics", diagnostics::get_diagnostics)
        .get("/api/diag/video", diagnostics::get_video_diagnostics)
        .get("/api/diag/storage", diag_storage::get_storage_diagnostics)
}
