#![doc = include_str!("../README.md")]

pub mod ap_country;
pub mod armed;
pub mod aux_egress;
pub mod aux_mux;
pub mod aux_ports;
pub mod aux_rpc;
pub mod aux_rpc_proxy;
pub mod buttons;
pub mod camera_control;
pub mod capabilities;
pub mod cloud_link;
pub mod cloud_publish;
pub mod config_tunnel_ingest;
pub mod contracts;
#[cfg(feature = "tls")]
pub mod crypto;
pub mod dashboard_session;
pub mod dispatch;
pub mod fleet_identity;
pub mod flight_modes;
pub mod frame;
pub mod framebus;
pub mod hwcaps;
pub mod identity;
pub mod ipc;
pub mod launchd;
pub mod link_feedback;
pub mod logd;
#[cfg(feature = "mavlink")]
pub mod mavlink;
pub mod mavlink_ingest;
pub mod mavlink_socket;
pub mod mcp_token;
pub mod media_relay;
pub mod msp;
pub mod netif;
pub mod node_info;
pub mod node_presence;
pub mod node_status;
pub mod nonce_cache;
pub mod offload_link;
pub mod pair_proof;
pub mod pairing_posture;
pub mod panel_ui;
pub mod param_codec;
pub mod paths;
pub mod plugin;
pub mod plugin_loopback_guard;
pub mod plugin_mdns;
pub mod radio_adapters;
pub mod reach;
pub mod relay_ticket;
pub mod rest;
pub mod retry;
pub mod secret_gen;
pub mod shutdown;
pub mod sidecar;
pub mod state;
pub mod tap;
pub mod units;
pub mod vision_rpc;
pub mod vision_status;
pub mod wfb_status;
pub mod wfb_tables;
pub mod ws_ticket;

/// Whether a flight-mode name is one that accepts offboard position setpoints.
///
/// Which mode a vehicle must be in to be commanded is firmware-specific, and the
/// name is all that survives the decode: ArduPilot calls it GUIDED, PX4 calls it
/// OFFBOARD, and PX4's mode table contains no GUIDED entry at all. A gate that
/// compared against the single string "GUIDED" could therefore never be
/// satisfied by a PX4 vehicle no matter what its operator did.
///
/// Deliberately NOT widened to the AUTO modes. PX4 rejects offboard setpoints
/// under those, so admitting them would move the failure from commanding
/// nothing to commanding something that is quietly ignored, which is worse
/// because it looks like it is working.
pub fn accepts_offboard_setpoints(mode: &str) -> bool {
    matches!(mode, "GUIDED" | "GUIDED_NOGPS" | "OFFBOARD")
}

#[cfg(test)]
mod offboard_mode_tests {
    use super::accepts_offboard_setpoints;

    #[test]
    fn each_firmware_names_the_commandable_mode_differently() {
        assert!(accepts_offboard_setpoints("GUIDED"), "ArduPilot");
        assert!(
            accepts_offboard_setpoints("GUIDED_NOGPS"),
            "ArduPilot without GPS"
        );
        assert!(accepts_offboard_setpoints("OFFBOARD"), "PX4");
    }

    #[test]
    fn a_px4_auto_mode_is_not_commandable() {
        // PX4 ignores offboard setpoints under AUTO. Admitting these would turn
        // a visible refusal into a silent no-op.
        for mode in ["AUTO.LOITER", "AUTO.MISSION", "AUTO.RTL", "AUTO.TAKEOFF"] {
            assert!(!accepts_offboard_setpoints(mode), "{mode}");
        }
    }

    #[test]
    fn a_pilot_flown_mode_is_not_commandable() {
        for mode in ["MANUAL", "STABILIZED", "ACRO", "POSCTL", "ALTCTL", "LOITER"] {
            assert!(!accepts_offboard_setpoints(mode), "{mode}");
        }
    }
}
