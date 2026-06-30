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

/// Atlas aux-lane relay snapshot: the relay loop's forward counters for the
/// world-model events it bridges off the WFB aux stream onto the LAN. Written by
/// the relay loop, read cross-process by the API layer for the GS Atlas relay
/// card. Resolve at write/read time via [`run_path`] so `ADOS_RUN_DIR` is honored.
pub const ATLAS_RELAY_JSON: &str = "/run/ados/atlas-relay.json";

/// Cross-process mesh-event journal: a newline-delimited JSON stream the
/// relay/receiver loops append to and the REST/OLED layer tails. Each line is
/// one event object (`{"bus","kind","timestamp_ms","payload"}`) matching the
/// in-process `MeshEvent` shape, so the tailer can republish it onto the
/// process-local asyncio bus the WebSocket + OLED already consume. Append-only,
/// best-effort; a reader seeks to end on start and follows new lines.
pub const MESH_EVENTS_JSONL: &str = "/run/ados/mesh-events.jsonl";

/// Cross-process field-pairing event journal: the sibling of
/// [`MESH_EVENTS_JSONL`] for the field tap-to-pair flow. The pairing manager
/// (in the API process) appends each pair event here so the native control
/// surface, in a different process, can tail it and fan it into the mesh event
/// stream. Same line envelope (`{"bus":"pair","kind","timestamp_ms","payload"}`),
/// append-only, best-effort; a reader seeks to end on start and follows new
/// lines.
pub const PAIR_EVENTS_JSONL: &str = "/run/ados/pair-events.jsonl";

/// Last-locked WFB channel hint. Written by the receiver when a channel
/// acquisition sweep locks onto the transmitter so a restart can try that
/// channel first instead of sweeping from scratch. Runtime HINT only: it lives
/// on tmpfs (gone on reboot) and is NEVER the rendezvous home. The home channel
/// is the operator's immutable `video.wfb.channel` in config; the agent must
/// never auto-write that field. Single integer channel number as text; atomic
/// tmp+rename write; missing/corrupt tolerated.
pub const WFB_LOCKED_CHANNEL_HINT: &str = "/run/ados/wfb-locked-channel";

/// The ground-station data-plane operator command socket. The native front has
/// no in-process Python pair/role manager to call, so it forwards a
/// newline-JSON `{"op":...}` request here and the running `ados-groundlink`
/// service applies it (role transition, gateway preference, pair-key install /
/// unpair). Mirrors the radio + Wi-Fi command sockets' framing: one
/// newline-terminated JSON request, one newline-terminated JSON reply, close.
pub const GROUNDLINK_CMD_SOCK: &str = "/run/ados/groundlink-cmd.sock";

/// The setup-complete sentinel. Dropped on a successful pair so the captive DNS
/// redirect stands down. Persistent (under `/var/lib/ados`), mode 0644.
pub const SETUP_COMPLETE_PATH: &str = "/var/lib/ados/setup-complete";

/// The hotspot passphrase file. Wiped on factory-reset so `hostapd_manager`
/// regenerates a fresh key on the next boot.
pub const AP_PASSPHRASE_PATH: &str = "/etc/ados/ap-passphrase";

/// The agent config file the role/pair persist paths round-trip.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

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
