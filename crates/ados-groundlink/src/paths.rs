//! Contract-E runtime file paths for the ground-station data-plane, mirroring
//! the Python constants in `core/paths.py`. Live state lives in `/run/ados`
//! (tmpfs, wiped on reboot); persistent mesh material lives under
//! `/etc/ados/mesh`. The constants are the cross-process contract: the wfb
//! manager writes them, the API layer + on-box UI read them.

// ---------------------------------------------------------------------------
// Runtime directory: /run/ados/ (tmpfs, ephemeral live state)
// ---------------------------------------------------------------------------

/// Live wfb-ng radio stats snapshot (rssi, snr, packets, fec, bitrate). On the
/// ground-station profile this is written ~once per second by the receive-side
/// wfb manager and read by the API layer and the on-box link-stats UI.
pub const WFB_STATS_JSON: &str = "/run/ados/wfb-stats.json";

/// Channel-hop supervisor snapshot (band, thresholds, last-hop time, the hop
/// history ring). On a drone rig the transmit-side supervisor writes it; on a
/// ground-station rig the receive-side control-plane listener writes it. A given
/// rig runs only one of the two, so there is no write contention on the file.
/// Read cross-process by the API layer and the on-box channel-hops UI page.
pub const HOP_SUPERVISOR_JSON: &str = "/run/ados/hop-supervisor.json";

/// Mesh state snapshot: role, neighbours, gateway election, partition status.
pub const MESH_STATE_JSON: &str = "/run/ados/mesh-state.json";

/// Relay-role snapshot (carrier link health + forwarded streams).
pub const WFB_RELAY_JSON: &str = "/run/ados/wfb-relay.json";

/// Receiver-role snapshot (merged-stream FEC combine stats).
pub const WFB_RECEIVER_JSON: &str = "/run/ados/wfb-receiver.json";

/// Cross-process mesh-event journal: a newline-delimited JSON stream the
/// relay/receiver loops append to and the REST/OLED layer tails. Each line is
/// one event object (`{"bus","kind","timestamp_ms","payload"}`) matching the
/// in-process `MeshEvent` shape, so the tailer can republish it onto the
/// process-local asyncio bus the WebSocket + OLED already consume. Append-only,
/// best-effort; a reader seeks to end on start and follows new lines.
pub const MESH_EVENTS_JSONL: &str = "/run/ados/mesh-events.jsonl";

/// Last-locked WFB channel hint. Written by the receiver when a channel
/// acquisition sweep locks onto the transmitter so a restart can try that
/// channel first instead of sweeping from scratch. Runtime HINT only: it lives
/// on tmpfs (gone on reboot) and is NEVER the rendezvous home. The home channel
/// is the operator's immutable `video.wfb.channel` in config; the agent must
/// never auto-write that field. Single integer channel number as text; atomic
/// tmp+rename write; missing/corrupt tolerated.
pub const WFB_LOCKED_CHANNEL_HINT: &str = "/run/ados/wfb-locked-channel";

// ---------------------------------------------------------------------------
// Persistent mesh directory: /etc/ados/mesh/
// ---------------------------------------------------------------------------

/// Persistent mesh directory: identity, pre-shared key, role, and the sentinels
/// the mesh manager reads on boot.
pub const MESH_DIR: &str = "/etc/ados/mesh";
/// Mesh identity (stable per-node id).
pub const MESH_ID_PATH: &str = "/etc/ados/mesh/id";
/// Mesh pre-shared key.
pub const MESH_PSK_PATH: &str = "/etc/ados/mesh/psk.key";
/// Mesh role sentinel (`direct` / `relay` / `receiver`).
pub const MESH_ROLE_PATH: &str = "/etc/ados/mesh/role";
/// Elected cloud-gateway record.
pub const MESH_GATEWAY_JSON: &str = "/etc/ados/mesh/gateway.json";
/// Receiver-discovery record.
pub const MESH_RECEIVER_JSON: &str = "/etc/ados/mesh/receiver.json";
/// Revoked-invite list: membership tokens the node refuses to honour.
pub const MESH_REVOCATIONS_JSON: &str = "/etc/ados/mesh/revocations.json";

/// Return the run directory, honouring the `ADOS_RUN_DIR` env override so tests
/// (and a non-root dev host) can redirect the tmpfs layout.
pub fn run_dir() -> String {
    std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string())
}

/// Return the path to a run-dir file, honouring the env override.
pub fn run_path(name: &str) -> String {
    format!("{}/{}", run_dir(), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_path_honours_env_override() {
        // SAFETY: single-threaded test; no other thread reads the env here.
        unsafe {
            std::env::set_var("ADOS_RUN_DIR", "/tmp/ados-test-run");
        }
        assert_eq!(
            run_path("wfb-stats.json"),
            "/tmp/ados-test-run/wfb-stats.json"
        );
        unsafe {
            std::env::remove_var("ADOS_RUN_DIR");
        }
        assert_eq!(run_path("wfb-stats.json"), "/run/ados/wfb-stats.json");
    }

    #[test]
    fn contract_constants_match_python_layout() {
        assert_eq!(WFB_STATS_JSON, "/run/ados/wfb-stats.json");
        assert_eq!(HOP_SUPERVISOR_JSON, "/run/ados/hop-supervisor.json");
        assert_eq!(MESH_STATE_JSON, "/run/ados/mesh-state.json");
        assert_eq!(WFB_RELAY_JSON, "/run/ados/wfb-relay.json");
        assert_eq!(WFB_RECEIVER_JSON, "/run/ados/wfb-receiver.json");
        assert_eq!(WFB_LOCKED_CHANNEL_HINT, "/run/ados/wfb-locked-channel");
        assert_eq!(MESH_REVOCATIONS_JSON, "/etc/ados/mesh/revocations.json");
    }
}
