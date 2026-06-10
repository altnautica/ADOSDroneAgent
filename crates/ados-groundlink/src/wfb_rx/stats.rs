//! The ground `wfb-stats.json` sidecar builder + the reg-blocked sidecar writer.
//!
//! `build_gs_stats` assembles the GS-extras payload the cross-process API +
//! heartbeat read, symmetric with the drone-side sidecar so the panel reads one
//! shape from either rig. `write_reg_blocked_sidecar` writes a minimal blocked
//! sidecar while the run loop retries the regulatory gate.

use ados_radio::config::WfbConfig;
use ados_radio::link_quality::LinkStats;

use super::args::STATE_REG_BLOCKED;

/// The regulatory picture the receive sidecar surfaces, symmetric with the
/// drone side. `domain` is the LIVE global country (`None` when unreadable);
/// `verified` is true only when it matched the wanted domain; `enabled_channels`
/// is the domain's permitted set (empty = could not determine). Resolved by
/// `prepare_interface` and stored on the manager so every sidecar write carries
/// the same regulatory truth instead of nothing.
#[derive(Debug, Clone, Default)]
pub struct GsRegSnapshot {
    pub domain: Option<String>,
    pub verified: bool,
    pub enabled_channels: Vec<u8>,
}

/// The truthful channel picture the receive sidecar surfaces, symmetric with the
/// drone side. `actual` is the LIVE interface channel; `rendezvous` is the
/// operator's home; `operating` is the runtime channel (== rendezvous unless a
/// coordinated move committed). The GS proves the link by its own valid-decode
/// count, so it has no `rf_unverified` of its own (always false here).
#[derive(Debug, Clone, Copy)]
pub struct GsChannelTruth {
    pub actual: u8,
    pub rendezvous: u8,
    pub operating: u8,
}

/// Write a minimal `reg_blocked` ground sidecar so the heartbeat + panel show the
/// regulatory conflict while the run loop retries the gate. Carries the reason
/// code and the rendezvous channel under inspection; no receive chain is running,
/// so the link-quality block defaults. Atomic via the Contract E writer.
pub fn write_reg_blocked_sidecar(
    interface: &str,
    chipset: Option<&str>,
    channel: u8,
    cfg: &WfbConfig,
    reg: &GsRegSnapshot,
    reason: &str,
) {
    let snap = LinkStats::default();
    // The chain is not running, so the live channel cannot be read; report the
    // rendezvous home for actual/rendezvous/operating.
    let channels = GsChannelTruth {
        actual: channel,
        rendezvous: channel,
        operating: channel,
    };
    let mut v = build_gs_stats(
        &snap,
        interface,
        chipset,
        false, // no injection while blocked
        channels,
        reg,
        cfg,
        STATE_REG_BLOCKED,
        crate::acquire::AcquireState::Searching.as_str(),
        false, // not channel-locked
        0.0,   // no valid decodes
        0,     // no reacquire kills
        0,     // no zombie kills
        None,  // no silence window (the chain is not running)
        0.0,   // no inbound video
    );
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "reg_block_reason".to_string(),
            serde_json::Value::String(reason.to_string()),
        );
    }
    let _ = crate::sidecars::write_json_atomic(
        std::path::Path::new(crate::paths::WFB_STATS_JSON),
        &v,
        0o644,
    );
}

/// Build the ground `wfb-stats.json` sidecar payload (the GS-extras the
/// cross-process API + heartbeat read). `profile` is always "ground_station".
#[allow(clippy::too_many_arguments)]
pub fn build_gs_stats(
    snap: &LinkStats,
    interface: &str,
    adapter_chipset: Option<&str>,
    adapter_injection_ok: bool,
    channels: GsChannelTruth,
    reg: &GsRegSnapshot,
    cfg: &WfbConfig,
    state: &str,
    acquire_state: &str,
    channel_locked: bool,
    valid_rx_packets_per_s: f64,
    reacquire_kills: u32,
    rx_zombie_kills: u32,
    rx_silent_seconds: Option<f64>,
    video_inbound_bytes_per_s: f64,
) -> serde_json::Value {
    serde_json::json!({
        // Top-level lifecycle string, mirroring the drone-side sidecar so the GS
        // heartbeat reads a real state instead of null.
        "state": state,
        // The state-machine state under its own key, mirroring the drone side.
        "link_state": state,
        "interface": interface,
        "adapter_chipset": adapter_chipset,
        "adapter_injection_ok": adapter_injection_ok,
        // Back-compat alias: `channel` now reflects the LIVE interface channel.
        "channel": channels.actual,
        "actual_channel": channels.actual,
        "rendezvous_channel": channels.rendezvous,
        "operating_channel": channels.operating,
        // Live regulatory picture, symmetric with the drone side.
        "reg_domain": reg.domain,
        "reg_verified": reg.verified,
        "enabled_channels": reg.enabled_channels,
        // The GS proves the link by its own valid-decode count, not by a TX
        // counter, so it is never `rf_unverified` (the transmitting-zero-
        // reception flag is a transmit-side concept). Surfaced for schema
        // symmetry so the panel reads one shape from either rig.
        "rf_unverified": false,
        "tx_power_dbm": cfg.tx_power_dbm,
        // The TX-power ceiling, mirroring the drone-side sidecar key so the panel
        // renders the headroom from either rig's stats.
        "tx_power_max_dbm": cfg.tx_power_max_dbm,
        "topology": cfg.topology,
        "mcs_index": cfg.mcs_index,
        "profile": "ground_station",
        "rx_silent_seconds": rx_silent_seconds,
        "rx_zombie_kills": rx_zombie_kills,
        "acquire_state": acquire_state,
        "channel_locked": channel_locked,
        "valid_rx_packets_per_s": (valid_rx_packets_per_s * 100.0).round() / 100.0,
        "reacquire_kills": reacquire_kills,
        "video_inbound_bytes_per_s": (video_inbound_bytes_per_s * 10.0).round() / 10.0,
        // Link-quality block (parity with the air side).
        "rssi_dbm": snap.rssi_dbm,
        // Noise floor, mirroring the drone-side sidecar key.
        "noise_dbm": snap.noise_dbm,
        "snr_db": snap.snr_db,
        "packets_received": snap.packets_received,
        "packets_lost": snap.packets_lost,
        "fec_recovered": snap.fec_recovered,
        "fec_failed": snap.fec_failed,
        "bitrate_kbps": snap.bitrate_kbps,
        "loss_percent": snap.loss_percent,
        "timestamp": snap.timestamp,
    })
}

#[cfg(test)]
mod tests {
    use super::super::args::{STATE_ACTIVE, STATE_REG_BLOCKED};
    use super::*;

    #[test]
    fn gs_stats_carries_ground_station_profile_and_extras() {
        let cfg = WfbConfig::default();
        let snap = LinkStats::default();
        // A locked link on a live channel that drifted from the configured home
        // (the acquirer swept to 157), under a verified domain.
        let channels = GsChannelTruth {
            actual: 157,
            rendezvous: 149,
            operating: 149,
        };
        let reg = GsRegSnapshot {
            domain: Some("US".to_string()),
            verified: true,
            enabled_channels: vec![149, 153, 157, 161, 165],
        };
        let v = build_gs_stats(
            &snap,
            "wlan1",
            Some("rtl88x2eu"),
            true,
            channels,
            &reg,
            &cfg,
            STATE_ACTIVE,
            "locked",
            true,
            12.5,
            2,
            1,
            Some(0.3),
            508_000.0,
        );
        assert_eq!(v["profile"], "ground_station");
        assert_eq!(v["interface"], "wlan1");
        assert_eq!(v["adapter_chipset"], "rtl88x2eu");
        assert_eq!(v["adapter_injection_ok"], true);
        // `channel` is the back-compat alias for the LIVE actual channel.
        assert_eq!(v["channel"], 157);
        assert_eq!(v["actual_channel"], 157);
        assert_eq!(v["rendezvous_channel"], 149);
        assert_eq!(v["operating_channel"], 149);
        assert_eq!(v["reg_domain"], "US");
        assert_eq!(v["reg_verified"], true);
        assert_eq!(
            v["enabled_channels"],
            serde_json::json!([149, 153, 157, 161, 165])
        );
        // The GS proves the link by valid decodes, so rf_unverified is never set.
        assert_eq!(v["rf_unverified"], false);
        assert_eq!(v["link_state"], "active");
        assert_eq!(v["acquire_state"], "locked");
        assert_eq!(v["channel_locked"], true);
        assert_eq!(v["reacquire_kills"], 2);
        assert_eq!(v["rx_zombie_kills"], 1);
        assert_eq!(v["valid_rx_packets_per_s"], 12.5);
        assert_eq!(v["video_inbound_bytes_per_s"], 508000.0);
        // mcs/topology/tx_power mirrored from config.
        assert_eq!(v["mcs_index"], cfg.mcs_index);
        assert_eq!(v["topology"], cfg.topology);
    }

    #[test]
    fn gs_stats_carries_top_level_state_noise_and_tx_power_ceiling() {
        // The drone-side sidecar writes a top-level `state`, a `noise_dbm`, and a
        // `tx_power_max_dbm`; the GS heartbeat reads the sidecar raw, so these
        // must be present on the ground sidecar too or the link block reports
        // null for them.
        let cfg = WfbConfig {
            tx_power_max_dbm: 30,
            ..WfbConfig::default()
        };
        let snap = LinkStats {
            noise_dbm: -91.0,
            ..LinkStats::default()
        };
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let v = build_gs_stats(
            &snap,
            "wlan1",
            Some("rtl88x2eu"),
            true,
            channels,
            &GsRegSnapshot::default(),
            &cfg,
            STATE_ACTIVE,
            "searching",
            false,
            0.0,
            0,
            0,
            Some(7.5),
            0.0,
        );
        assert_eq!(v["state"], "active");
        assert_eq!(v["noise_dbm"], -91.0);
        assert_eq!(v["tx_power_max_dbm"], 30);
        assert_eq!(v["rx_silent_seconds"], 7.5);
        // The new keys must never be null on the ground sidecar.
        assert!(!v["state"].is_null());
        assert!(!v["noise_dbm"].is_null());
        assert!(!v["tx_power_max_dbm"].is_null());
        // An unknown regulatory snapshot reports a JSON null domain (not absent)
        // + an empty enabled set, never a fabricated value.
        assert!(v["reg_domain"].is_null());
        assert_eq!(v["reg_verified"], false);
        assert_eq!(v["enabled_channels"], serde_json::json!([]));
    }

    #[test]
    fn reg_blocked_state_string_is_bland_and_stable() {
        // The sidecar surfaces this verbatim; keep it stable and tag-free.
        assert_eq!(STATE_REG_BLOCKED, "reg_blocked");
    }

    #[test]
    fn reg_blocked_sidecar_carries_state_reason_and_no_injection() {
        // The reg-blocked sidecar is written from the run loop when the gate
        // fails; it must surface the bland reason + the blocked state and never
        // claim injection while no receive chain is running. Write to a tmp dir
        // to verify the JSON shape without touching /run.
        let cfg = WfbConfig::default();
        let snap = LinkStats::default();
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        // The forbidden domain the global set could not displace, surfaced.
        let reg = GsRegSnapshot {
            domain: Some("BO".to_string()),
            verified: false,
            enabled_channels: vec![],
        };
        let mut v = build_gs_stats(
            &snap,
            "wlan1",
            Some("rtl88x2eu"),
            false,
            channels,
            &reg,
            &cfg,
            STATE_REG_BLOCKED,
            crate::acquire::AcquireState::Searching.as_str(),
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "reg_block_reason".to_string(),
                serde_json::Value::String("phy_override".to_string()),
            );
        }
        assert_eq!(v["state"], "reg_blocked");
        assert_eq!(v["reg_block_reason"], "phy_override");
        assert_eq!(v["adapter_injection_ok"], false);
        assert_eq!(v["channel_locked"], false);
        assert_eq!(v["valid_rx_packets_per_s"], 0.0);
        assert_eq!(v["profile"], "ground_station");
        // The live conflict is visible in the reg block sidecar.
        assert_eq!(v["reg_domain"], "BO");
        assert_eq!(v["reg_verified"], false);
    }
}
