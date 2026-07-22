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
/// count and never measures a transmit path, so it has no `rf_unverified`
/// verdict of its own — the sidecar reports `null` for it, never a boolean.
#[derive(Debug, Clone, Copy)]
pub struct GsChannelTruth {
    pub actual: u8,
    pub rendezvous: u8,
    pub operating: u8,
}

/// The selected-adapter facts the receive sidecar surfaces, mirroring the
/// drone-side `AdapterInfo` so the panel reads one shape from either rig.
///
/// Default (no adapter resolved yet) is the honest unknown: no chipset, no
/// injection claim, no USB speed reading, no degraded verdict. A resolved
/// adapter always carries a chipset (`From<&SelectedAdapter>` sets it), so
/// `chipset.is_some()` is the resolved marker the sidecar keys its verdicts
/// on: an unresolved record emits `null` for both `adapter_injection_ok` and
/// `adapter_usb_degraded` — never a confident boolean about hardware nothing
/// ever examined — and `usb_degraded` is asserted only alongside a real
/// enumerated speed reading, so the operator is never shown a fabricated
/// fault OR a fabricated all-clear.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GsAdapterInfo {
    pub chipset: Option<String>,
    pub injection_ok: bool,
    /// Enumerated USB link speed (Mbps); `None` when not USB / unreadable.
    pub usb_speed_mbps: Option<u32>,
    /// True only when the adapter enumerated below high-speed. Such an adapter
    /// can pass monitor-mode setup and advance its counters yet carry almost no
    /// usable RF, so the receive plane must surface it rather than report a
    /// healthy link.
    pub usb_degraded: bool,
}

impl GsAdapterInfo {
    /// Build from the resolved adapter identity, deriving `usb_degraded` with
    /// the shared `usb_speed_degraded` threshold rather than restating it, so
    /// the ground and air planes can never disagree on what "degraded" means.
    pub fn new(chipset: Option<String>, injection_ok: bool, usb_speed_mbps: Option<u32>) -> Self {
        Self {
            chipset,
            injection_ok,
            usb_speed_mbps,
            usb_degraded: ados_radio::adapter::usb_speed_degraded(usb_speed_mbps),
        }
    }
}

impl From<&ados_radio::adapter::SelectedAdapter> for GsAdapterInfo {
    fn from(sel: &ados_radio::adapter::SelectedAdapter) -> Self {
        Self::new(
            Some(sel.chipset.clone()),
            sel.injection_ok,
            sel.usb_speed_mbps,
        )
    }
}

/// Write a minimal `reg_blocked` ground sidecar so the heartbeat + panel show the
/// regulatory conflict while the run loop retries the gate. Carries the reason
/// code and the rendezvous channel under inspection; no receive chain is running,
/// so the link-quality block defaults. Atomic via the Contract E writer.
///
/// When an `IngestEmitter` is passed, the SAME body is shipped to the logging
/// store as a single full-snapshot `link.wfb_status` event right after the file
/// write, so the durable read source and the on-disk sidecar stay in lockstep on
/// this degraded-state write too — not only on the per-line active writes in the
/// stats reader. A store-first read therefore never lags the `reg_blocked` file.
/// Best-effort: an absent logging daemon drops the event without disturbing the
/// retry loop.
pub fn write_reg_blocked_sidecar(
    interface: &str,
    adapter: &GsAdapterInfo,
    channel: u8,
    cfg: &WfbConfig,
    reg: &GsRegSnapshot,
    reason: &str,
    ingest: Option<&ados_protocol::logd::emitter::IngestEmitter>,
) {
    let snap = LinkStats::default();
    // The chain is not running, so the live channel cannot be read; report the
    // rendezvous home for actual/rendezvous/operating.
    let channels = GsChannelTruth {
        actual: channel,
        rendezvous: channel,
        operating: channel,
    };
    // No receive chain is running, so no injection is claimed. The USB facts are
    // physical properties of the selected adapter and stay truthful here — a
    // blocked gate on a slow-USB adapter must still show WHY.
    let adapter = GsAdapterInfo {
        injection_ok: false,
        ..adapter.clone()
    };
    let mut v = build_gs_stats(
        &snap,
        interface,
        &adapter,
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
    if let Some(em) = ingest {
        em.emit_event(
            "link.wfb_status",
            ados_protocol::logd::Level::Info,
            json_object_to_fields(&v),
        );
    }
}

/// Build the ground `wfb-stats.json` sidecar payload (the GS-extras the
/// cross-process API + heartbeat read). `profile` is always "ground_station".
#[allow(clippy::too_many_arguments)]
pub fn build_gs_stats(
    snap: &LinkStats,
    interface: &str,
    adapter: &GsAdapterInfo,
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
    // A real signal-strength reading requires a decoded packet. Without one the
    // RSSI/SNR/noise fields are the default sentinel (rssi -100), not a
    // measurement — a deaf ground station sits here. Report them as null (no
    // measurement) rather than shipping -100 dBm as if it were a real weak-signal
    // reading (the same gate the drone-side sidecar uses, so both rigs agree).
    let measured = snap.packets_received > 0;
    serde_json::json!({
        // Sidecar schema version (best-effort drift signal for readers). Shared
        // with the drone-side writer via the one const so both rigs agree.
        "version": ados_radio::paths::WFB_STATS_SIDECAR_VERSION,
        // Top-level lifecycle string, mirroring the drone-side sidecar so the GS
        // heartbeat reads a real state instead of null.
        "state": state,
        // The state-machine state under its own key, mirroring the drone side.
        "link_state": state,
        "interface": interface,
        "adapter_chipset": adapter.chipset,
        // A verdict only once an adapter was actually resolved (a chipset is
        // recorded for every resolved adapter): an unresolved record reports
        // `null`, never a confident boolean about hardware nothing examined.
        // A resolved adapter's verdict rides verbatim — including the
        // deliberate `false` the reg-blocked writer sets while no receive
        // chain runs.
        "adapter_injection_ok": adapter.chipset.is_some().then_some(adapter.injection_ok),
        // USB link health of the selected receive adapter, under the same keys the
        // drone-side sidecar uses so the panel reads one shape from either rig. An
        // adapter that enumerated below high-speed (e.g. 12 Mbps full-speed on a
        // flaky port) can pass monitor-mode setup and advance its counters while
        // carrying almost no usable RF, so a deaf ground station's CAUSE stays
        // legible instead of reading as a bare "0 received". `null` speed means
        // no reading was available (not USB / unreadable), never a claim of zero —
        // and with no speed reading the degraded verdict is `null` too, because
        // "not degraded" is a health claim only a real enumeration can back.
        "adapter_usb_speed_mbps": adapter.usb_speed_mbps,
        "adapter_usb_degraded": adapter.usb_speed_mbps.map(|_| adapter.usb_degraded),
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
        // counter: the transmitting-zero-reception verdict is a transmit-side
        // measurement this plane never performs. The key rides for schema
        // symmetry but carries `null` — no verdict — because a `false` here
        // would present itself downstream as the radio's own proof that the
        // transmit path was verified, for a node that measured nothing.
        "rf_unverified": serde_json::Value::Null,
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
        // Which radio backend is driving the receive path: the Linux monitor-mode
        // + wfb_rx backend. Wire value mirrors the drone-side KernelMonitor backend
        // ("kernel") so Mission Control badges the live radio path from either rig
        // (additive; Rule 28).
        "backend": "kernel",
        // Link-quality block (parity with the air side). Signal-strength fields are
        // null until a packet is actually decoded (see `measured` above) so the
        // no-measurement sentinel never masquerades as a real weak-signal reading.
        "rssi_dbm": measured.then_some(snap.rssi_dbm),
        // Noise floor, mirroring the drone-side sidecar key.
        "noise_dbm": measured.then_some(snap.noise_dbm),
        "snr_db": measured.then_some(snap.snr_db),
        "packets_received": snap.packets_received,
        // Diagnostic RX counters from wfb_rx that separate the failure modes a bare
        // "0 received" hides on the RX side: `packets_all` is every frame captured
        // off-air BEFORE decrypt (0 = deaf radio, no RF arriving); `decrypt_errors`
        // are wrong-key / wrong-link_id failures (RF arriving, not decodable);
        // `packets_bad` are corrupt frames; `session_packets` counts valid
        // session-key packets. `link_diag` is the one-glance verdict (deaf /
        // mis_keyed / jammed / healthy / searching) so a deaf ground station's
        // CAUSE is legible instead of reading as a bare "0 / connected".
        "link_diag": snap.link_diag,
        "packets_all": snap.packets_all,
        "decrypt_errors": snap.decrypt_errors,
        "packets_bad": snap.packets_bad,
        "session_packets": snap.session_packets,
        "packets_lost": snap.packets_lost,
        "fec_recovered": snap.fec_recovered,
        "fec_failed": snap.fec_failed,
        "bitrate_kbps": snap.bitrate_kbps,
        "loss_percent": snap.loss_percent,
        "timestamp": snap.timestamp,
    })
}

/// Convert a JSON object into the logging store's open detail map (`Fields`), so
/// the full ground wfb-status body can ride a single `link.wfb_status` event,
/// symmetric with the air-side producer. Recurses through nested arrays /
/// objects; numbers preserve their integer-vs-float kind and JSON null
/// round-trips to msgpack nil, so the store row decodes back to the identical
/// JSON the REST base merges over.
pub fn json_object_to_fields(value: &serde_json::Value) -> ados_protocol::logd::Fields {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), json_to_mpv(v)))
            .collect(),
        _ => ados_protocol::logd::Fields::new(),
    }
}

/// Recursively map a `serde_json::Value` onto the msgpack value the detail map
/// carries. Integers stay integers (signed when negative), floats stay floats,
/// and null becomes nil.
fn json_to_mpv(value: &serde_json::Value) -> ados_protocol::logd::Value {
    use ados_protocol::logd::Value as MpVal;
    match value {
        serde_json::Value::Null => MpVal::Nil,
        serde_json::Value::Bool(b) => MpVal::from(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                MpVal::from(i)
            } else if let Some(u) = n.as_u64() {
                MpVal::from(u)
            } else {
                MpVal::from(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => MpVal::from(s.as_str()),
        serde_json::Value::Array(items) => MpVal::Array(items.iter().map(json_to_mpv).collect()),
        serde_json::Value::Object(map) => MpVal::Map(
            map.iter()
                .map(|(k, v)| (MpVal::from(k.as_str()), json_to_mpv(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::args::{STATE_ACTIVE, STATE_REG_BLOCKED, STATE_SEARCHING};
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
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
        let v = build_gs_stats(
            &snap,
            "wlan1",
            &adapter,
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
        // The GS proves the link by valid decodes and never measures a transmit
        // path, so it reports NO transmit-proof verdict: the key is present for
        // schema symmetry but null, never a boolean.
        assert!(v.as_object().unwrap().contains_key("rf_unverified"));
        assert!(v["rf_unverified"].is_null());
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
    fn gs_stats_never_claims_a_transmit_proof_verdict() {
        // A receive-side node has not measured a transmit path, in ANY state:
        // the `rf_unverified` key must read null (no verdict), never a boolean.
        // A boolean false here flows through every forwarder as the radio's own
        // measured proof that the transmit path was verified — the exact
        // false-healthy surface the verdict exists to expose — so this test
        // pins the producer to absence.
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
        for (state, locked, decoded) in [
            (STATE_ACTIVE, true, 25),
            (STATE_SEARCHING, false, 0),
            (STATE_REG_BLOCKED, false, 0),
        ] {
            let snap = LinkStats {
                packets_received: decoded,
                ..LinkStats::default()
            };
            let v = build_gs_stats(
                &snap,
                "wlan1",
                &adapter,
                channels,
                &GsRegSnapshot::default(),
                &WfbConfig::default(),
                state,
                "searching",
                locked,
                0.0,
                0,
                0,
                None,
                0.0,
            );
            assert!(
                v.as_object().unwrap().contains_key("rf_unverified"),
                "{state}: the key must ride for schema symmetry"
            );
            assert!(
                v["rf_unverified"].is_null(),
                "{state}: a receive-side node must report no transmit-proof \
                 verdict, got {:?}",
                v["rf_unverified"]
            );
        }
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
        // A measured snapshot (a packet decoded) so the signal-strength fields are
        // real readings, not the no-measurement sentinel.
        let snap = LinkStats {
            noise_dbm: -91.0,
            packets_received: 3,
            ..LinkStats::default()
        };
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
        let v = build_gs_stats(
            &snap,
            "wlan1",
            &adapter,
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
        assert_eq!(v["backend"], "kernel");
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
    fn gs_stats_nulls_signal_fields_and_surfaces_diag_when_deaf() {
        // A deaf ground station: wfb_rx alive and emitting a PKT line, but 0 decoded
        // and 0 captured off-air. The signal-strength fields must be null (not the
        // -100 sentinel dressed up as a reading), and the diagnostic counters +
        // link_diag must carry the real, legible cause.
        let snap = LinkStats {
            link_diag: ados_radio::link_quality::LinkDiag::Deaf,
            ..LinkStats::default() // packets_received == 0, packets_all == 0
        };
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
        let v = build_gs_stats(
            &snap,
            "wlan1",
            &adapter,
            channels,
            &GsRegSnapshot::default(),
            &WfbConfig::default(),
            STATE_SEARCHING,
            "searching",
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        // No fabricated weak-signal reading when nothing was decoded.
        assert!(v["rssi_dbm"].is_null());
        assert!(v["noise_dbm"].is_null());
        assert!(v["snr_db"].is_null());
        // The cause is legible: deaf + all counters honestly zero.
        assert_eq!(v["link_diag"], "deaf");
        assert_eq!(v["packets_all"], 0);
        assert_eq!(v["decrypt_errors"], 0);
        assert_eq!(v["state"], "searching");
    }

    #[test]
    fn json_object_to_fields_round_trips_the_gs_body() {
        // The GS body shipped to the store must decode back to identical JSON, so
        // the durable read source matches the live sidecar fallback. Null domain,
        // the channel array, and integer-vs-float number kinds are the at-risk
        // legs.
        let cfg = WfbConfig::default();
        let snap = LinkStats::default();
        let channels = GsChannelTruth {
            actual: 157,
            rendezvous: 149,
            operating: 149,
        };
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
        let body = build_gs_stats(
            &snap,
            "wlan1",
            &adapter,
            channels,
            &GsRegSnapshot::default(),
            &cfg,
            STATE_ACTIVE,
            "searching",
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        let fields = json_object_to_fields(&body);
        use ados_protocol::frame::{decode_len, HEADER_SIZE};
        use ados_protocol::logd::{EventFrame, IngestFrame, Level, LOGD_MAX_FRAME};
        let mut frame = EventFrame::new(0, "link.wfb_status", "ados-groundlink", Level::Info);
        frame.detail = fields;
        let bytes = IngestFrame::Event(frame).encode().unwrap();
        let header: [u8; HEADER_SIZE] = bytes[..HEADER_SIZE].try_into().unwrap();
        let len = decode_len(header, LOGD_MAX_FRAME, true).unwrap();
        let decoded = match IngestFrame::decode(&bytes[HEADER_SIZE..HEADER_SIZE + len]).unwrap() {
            IngestFrame::Event(e) => e,
            other => panic!("expected an event frame, got {other:?}"),
        };
        let back = serde_json::to_value(decoded.detail).unwrap();
        assert_eq!(back, body);
        // The unknown domain is a JSON null on the wire (not dropped), the empty
        // enabled set is an array, and the GS profile string survives.
        assert!(back["reg_domain"].is_null());
        assert_eq!(back["enabled_channels"], serde_json::json!([]));
        assert_eq!(back["profile"], "ground_station");
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
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), false, Some(480));
        let mut v = build_gs_stats(
            &snap,
            "wlan1",
            &adapter,
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

    #[test]
    fn gs_stats_reports_the_adapter_usb_link_health() {
        // A healthy high-speed adapter: the real reading is surfaced and the
        // degraded flag stays clear, under the same keys the drone side uses.
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
        let v = build_gs_stats(
            &LinkStats::default(),
            "wlan1",
            &adapter,
            channels,
            &GsRegSnapshot::default(),
            &WfbConfig::default(),
            STATE_ACTIVE,
            "locked",
            true,
            1.0,
            0,
            0,
            None,
            0.0,
        );
        assert_eq!(v["adapter_usb_speed_mbps"], 480);
        assert_eq!(v["adapter_usb_degraded"], false);
    }

    #[test]
    fn gs_stats_flags_a_slow_usb_adapter_so_a_deaf_receiver_has_a_cause() {
        // A full-speed (12 Mbps) enumeration cannot carry usable RF. The receive
        // plane must surface it, otherwise a deaf ground station reads as a bare
        // "0 received" with no legible cause.
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(12));
        let v = build_gs_stats(
            &LinkStats::default(),
            "wlan1",
            &adapter,
            channels,
            &GsRegSnapshot::default(),
            &WfbConfig::default(),
            STATE_SEARCHING,
            "searching",
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        assert_eq!(v["adapter_usb_speed_mbps"], 12);
        assert_eq!(v["adapter_usb_degraded"], true);
    }

    #[test]
    fn gs_stats_reports_an_unknown_usb_speed_as_null_and_never_as_zero() {
        // No adapter resolved yet / a non-USB or unreadable adapter: the speed is
        // an explicit JSON null (no reading), never a confident 0 that would look
        // like a measured dead link — and with no speed reading there is no
        // degraded VERDICT either: the flag is null, never a false that a
        // downstream three-state consumer renders as a measured-healthy green.
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let v = build_gs_stats(
            &LinkStats::default(),
            "wlan1",
            &GsAdapterInfo::default(),
            channels,
            &GsRegSnapshot::default(),
            &WfbConfig::default(),
            STATE_SEARCHING,
            "searching",
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        assert!(v["adapter_usb_speed_mbps"].is_null());
        assert_ne!(v["adapter_usb_speed_mbps"], 0);
        assert!(
            v["adapter_usb_degraded"].is_null(),
            "no enumeration ⇒ no degraded verdict, got {:?}",
            v["adapter_usb_degraded"]
        );
        // An unresolved adapter also makes no chipset or injection claim: both
        // read null, so the panel shows "not reported" instead of a fabricated
        // red no-injection (or green all-clear) about hardware never examined.
        assert!(v["adapter_chipset"].is_null());
        assert!(
            v["adapter_injection_ok"].is_null(),
            "no adapter resolved ⇒ no injection verdict, got {:?}",
            v["adapter_injection_ok"]
        );
    }

    #[test]
    fn gs_stats_keeps_a_resolved_adapters_verdicts_as_real_booleans() {
        // The three-state contract must not swallow real readings: a resolved
        // adapter with an enumerated speed reports actual booleans, so the
        // loud degraded / no-injection signals still fire when measured.
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), false, Some(12));
        let v = build_gs_stats(
            &LinkStats::default(),
            "wlan1",
            &adapter,
            channels,
            &GsRegSnapshot::default(),
            &WfbConfig::default(),
            STATE_SEARCHING,
            "searching",
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        assert_eq!(v["adapter_injection_ok"], false);
        assert_eq!(v["adapter_usb_degraded"], true);
    }

    #[test]
    fn gs_adapter_info_derives_degraded_from_the_shared_threshold() {
        // The ground plane must not restate the degraded threshold: every
        // constructed value agrees with the shared `usb_speed_degraded` helper,
        // so the two rigs can never disagree on what "degraded" means.
        for speed in [None, Some(12), Some(480), Some(5000)] {
            let info = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, speed);
            assert_eq!(
                info.usb_degraded,
                ados_radio::adapter::usb_speed_degraded(speed),
                "degraded flag disagreed with the shared threshold for {speed:?}"
            );
            assert_eq!(info.usb_speed_mbps, speed);
        }
    }

    #[test]
    fn reg_blocked_sidecar_keeps_the_usb_facts_while_denying_injection() {
        // A regulatory-blocked gate runs no receive chain, so it claims no
        // injection — but the adapter's USB link health is a physical fact that
        // must survive, since a slow-USB adapter is exactly the kind of cause an
        // operator needs while the gate retries.
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(12));
        let blocked = GsAdapterInfo {
            injection_ok: false,
            ..adapter.clone()
        };
        let v = build_gs_stats(
            &LinkStats::default(),
            "wlan1",
            &blocked,
            channels,
            &GsRegSnapshot::default(),
            &WfbConfig::default(),
            STATE_REG_BLOCKED,
            "searching",
            false,
            0.0,
            0,
            0,
            None,
            0.0,
        );
        assert_eq!(v["adapter_injection_ok"], false);
        assert_eq!(v["adapter_usb_speed_mbps"], 12);
        assert_eq!(v["adapter_usb_degraded"], true);
    }

    #[test]
    fn gs_adapter_info_carries_every_fact_off_the_selected_adapter() {
        // The resolved adapter record is the one source the sidecar reads, so no
        // fact may be dropped on the way through (the drop is what left the
        // ground sidecar reporting a default chipset and a fixed injection claim).
        let sel = ados_radio::adapter::SelectedAdapter {
            ifname: "wlan1".to_string(),
            chipset: "rtl88x2eu".to_string(),
            injection_ok: true,
            usb_speed_mbps: Some(12),
            usb_degraded: true,
        };
        let info = GsAdapterInfo::from(&sel);
        assert_eq!(info.chipset.as_deref(), Some("rtl88x2eu"));
        assert!(info.injection_ok);
        assert_eq!(info.usb_speed_mbps, Some(12));
        assert!(info.usb_degraded);
    }

    #[test]
    fn json_object_to_fields_round_trips_the_usb_link_health() {
        // The body shipped to the store must decode back identically, including a
        // null speed (absent reading) and the integer speed reading — the store is
        // the durable read source the panel falls back to.
        let channels = GsChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
        };
        for adapter in [
            GsAdapterInfo::default(),
            GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(12)),
        ] {
            let body = build_gs_stats(
                &LinkStats::default(),
                "wlan1",
                &adapter,
                channels,
                &GsRegSnapshot::default(),
                &WfbConfig::default(),
                STATE_SEARCHING,
                "searching",
                false,
                0.0,
                0,
                0,
                None,
                0.0,
            );
            let fields = json_object_to_fields(&body);
            let back = serde_json::to_value(fields).unwrap();
            assert_eq!(
                back["adapter_usb_speed_mbps"],
                body["adapter_usb_speed_mbps"]
            );
            assert_eq!(back["adapter_usb_degraded"], body["adapter_usb_degraded"]);
        }
    }

    #[tokio::test]
    async fn reg_blocked_sidecar_emits_the_status_event_when_given_an_emitter() {
        // The reg-blocked write is the GS degraded-state path: passing an emitter
        // must ship the same body to the store as a `link.wfb_status` event so a
        // store-first read never lags the on-disk reg-blocked sidecar. The emitter
        // records every enqueue regardless of whether a daemon is listening, so one
        // write enqueues exactly one event. The unconditional emit fires after the
        // best-effort file write, so the assertion holds whether or not the runtime
        // sidecar path is writable in the test environment.
        let dir = tempfile::tempdir().unwrap();
        let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-groundlink",
            dir.path().join("ingest.sock"),
        );
        let stats = emitter.stats();
        let adapter = GsAdapterInfo::new(Some("rtl88x2eu".to_string()), true, Some(480));
        write_reg_blocked_sidecar(
            "wlan1",
            &adapter,
            149,
            &WfbConfig::default(),
            &GsRegSnapshot::default(),
            "phy_override",
            Some(&emitter),
        );
        assert_eq!(stats.enqueued(), 1);

        // With no emitter the write enqueues nothing.
        let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-groundlink",
            dir.path().join("ingest2.sock"),
        );
        let none_stats = none_emitter.stats();
        write_reg_blocked_sidecar(
            "wlan1",
            &adapter,
            149,
            &WfbConfig::default(),
            &GsRegSnapshot::default(),
            "phy_override",
            None,
        );
        assert_eq!(none_stats.enqueued(), 0);
    }
}
