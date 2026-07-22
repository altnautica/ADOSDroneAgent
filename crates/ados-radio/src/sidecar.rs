//! Contract E sidecar truth structs + writers for the radio service.
//!
//! The truth structs ([`ChannelTruth`], [`RegSnapshot`], [`RegPosture`],
//! [`AdapterInfo`]) carry the honest channel / regulatory / adapter picture the
//! `wfb-stats.json` sidecar surfaces, so the GCS / OLED / webapp see where the
//! radio ACTUALLY is, not where it was configured to be. `write_stats_sidecar`
//! renders the full schema the REST handler merges over its base;
//! `write_adapters_sidecar` writes the detected-adapter list; the key fingerprint
//! + device-id readers feed the pair-identity block.

use std::path::Path;

use serde_json::json;

use ados_radio::bitrate::BitrateSnapshot;
use ados_radio::config::WfbConfig;
use ados_radio::link_quality::LinkStats;
use ados_radio::paths::{run_path, write_sidecar, WFB_TX_KEY};
use ados_radio::watchdog::WatchdogCounters;

use crate::txrate::TxRates;

/// The adapter facts the sidecar surfaces (None until an adapter is selected).
#[derive(Clone, Default)]
pub(crate) struct AdapterInfo {
    pub(crate) interface: String,
    pub(crate) chipset: String,
    pub(crate) injection_ok: bool,
    /// Enumerated USB link speed (Mbps); None when not USB / unreadable.
    pub(crate) usb_speed_mbps: Option<u32>,
    /// True when the adapter is on a slow USB link (full-speed) and so may
    /// advance tx_bytes while emitting no usable RF.
    pub(crate) usb_degraded: bool,
}

impl AdapterInfo {
    /// The scanned-and-none-found record: the adapter scan RAN and proved no
    /// injection-capable radio exists. Passing this (instead of `None`) keeps
    /// the injection verdict a MEASURED `false` — the loud stranded-radio
    /// signal — while the USB fields stay absent, because nothing enumerated
    /// and there is no link-health reading to report. `None` means the scan
    /// has not happened (unpaired / reg-blocked before selection), where no
    /// verdict of any kind exists yet.
    pub(crate) fn none_found() -> Self {
        Self::default()
    }
}

/// The truthful channel picture the sidecar surfaces, so the operator and the
/// GCS see where the radio ACTUALLY is, not where it was configured to be.
///
/// - `actual` is the LIVE channel read from `iw dev` this tick. Under a
///   forbidden domain the driver can land the interface on an in-band fallback
///   frequency; reporting the live value surfaces that instead of masking it
///   behind the configured channel.
/// - `rendezvous` is the operator's home / meeting channel (the immutable
///   `video.wfb.channel`, or the optional rendezvous pin). Both rigs derive it
///   identically, so it is the guaranteed meeting point.
/// - `operating` is the runtime channel (tmpfs); it equals `rendezvous` unless a
///   coordinated channel move committed.
/// - `locked` is the received-side lock proof — true only when a verified return
///   signal was heard, never hardcoded.
/// - `rf_unverified` is raised when the transmit counter is advancing yet no
///   return signal has been heard within the grace window (the
///   transmitting-zero-reception case).
#[derive(Clone, Copy, Default)]
pub(crate) struct ChannelTruth {
    pub(crate) actual: u8,
    pub(crate) rendezvous: u8,
    pub(crate) operating: u8,
    pub(crate) locked: bool,
    pub(crate) rf_unverified: bool,
}

impl ChannelTruth {
    /// Pre-bring-up truth, before the interface exists to read a live channel
    /// from: all three channels report the rendezvous home, the link is not yet
    /// proven, and the transmit counter is not advancing. Used for the
    /// unpaired / reg-blocked / no-adapter / connecting states the heartbeat has
    /// not yet refined with a live read.
    pub(crate) fn configured(rendezvous: u8) -> Self {
        Self {
            actual: rendezvous,
            rendezvous,
            operating: rendezvous,
            locked: false,
            rf_unverified: false,
        }
    }
}

/// The regulatory picture the sidecar surfaces, so a domain the global set could
/// not displace (the forbidden-band case) is visible in one glance instead of
/// masked. `domain` is the LIVE global country (`None` when unreadable);
/// `verified` is true only when it matched the wanted domain; `enabled_channels`
/// is the domain's permitted channel set (empty = could not determine).
///
/// `posture` is the operator-facing operating-region posture ("unrestricted" |
/// "region") and `pinned_region` the pinned country when in region mode (None
/// under unrestricted). These additive fields drive the honest "unrestricted —
/// operator responsible for local RF compliance" surfacing on the GCS / OLED /
/// webapp; the rest of the snapshot is unchanged.
#[derive(Clone, Default)]
pub(crate) struct RegSnapshot {
    pub(crate) domain: Option<String>,
    pub(crate) verified: bool,
    pub(crate) enabled_channels: Vec<u8>,
    pub(crate) posture: RegPosture,
}

/// The operator-facing operating-region posture, surfaced on every sidecar so the
/// GCS / OLED / webapp can show the amber unrestricted badge. Default unrestricted
/// with no pinned region (the fresh-box posture).
#[derive(Clone)]
pub(crate) struct RegPosture {
    /// "unrestricted" (default) | "region".
    pub(crate) mode: &'static str,
    /// The pinned ISO 3166-1 alpha-2 country, or None under unrestricted.
    pub(crate) pinned_region: Option<String>,
}

impl Default for RegPosture {
    fn default() -> Self {
        // The fresh-box posture: unrestricted, no pinned region.
        RegPosture {
            mode: "unrestricted",
            pinned_region: None,
        }
    }
}

impl RegPosture {
    /// Build from the resolved operating mode + the wanted region domain.
    pub(crate) fn new(unrestricted: bool, region_domain: &str) -> Self {
        if unrestricted {
            RegPosture {
                mode: "unrestricted",
                pinned_region: None,
            }
        } else {
            RegPosture {
                mode: "region",
                pinned_region: Some(region_domain.to_string()),
            }
        }
    }
}

/// Write the full detected-adapter list to `/run/ados/wfb-adapters.json`
/// (Contract: the seam permanent-Python + the GCS panel read). Atomic
/// tmp+rename via `write_sidecar`.
pub(crate) fn write_adapters_sidecar(adapters: &[ados_radio::adapter::WifiAdapterInfo]) {
    let v = serde_json::to_value(adapters).unwrap_or_else(|_| serde_json::Value::Array(vec![]));
    let _ = write_sidecar(&run_path("wfb-adapters.json"), &v);
}

/// Compute the 16-hex-char public-key fingerprint of the drone TX key, or `None`
/// when the key is absent or not exactly 64 bytes. The peer-public half is the
/// second 32 bytes of the WFB key file; the fingerprint is `blake2b(pub,
/// digest_size=8)` rendered as 16 lowercase hex chars. Both rigs of a pair
/// compute the same value from their respective key files, so heartbeat
/// cross-checks reduce to a string compare. Byte-identical to
/// `key_mgr.read_public_fingerprint`.
pub(crate) fn read_public_fingerprint(path: &Path) -> Option<String> {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;
    const WFB_KEY_FILE_BYTES: usize = 64;
    const WFB_PUBLIC_HALF_OFFSET: usize = 32;
    let data = std::fs::read(path).ok()?;
    if data.len() != WFB_KEY_FILE_BYTES {
        return None;
    }
    let mut hasher = Blake2bVar::new(8).ok()?;
    hasher.update(&data[WFB_PUBLIC_HALF_OFFSET..]);
    let mut out = [0u8; 8];
    hasher.finalize_variable(&mut out).ok()?;
    Some(hex::encode(out))
}

/// Build the `wfb-stats.json` Contract E body (the full schema the REST handler
/// at `api/routes/wfb.py` merges over its base, so the GCS/LCD/dashboard radio
/// panel renders correctly). The link-quality fields (rssi/snr/packets/loss/
/// bitrate) are left to the REST base defaults until the link-quality monitor
/// lands; `adapter_chipset`/`adapter_injection_ok`/`tx_power_dbm` must be
/// present here or the panel shows a false "stranded radio" warning.
///
/// Split out from the file writer so the same body can be shipped to the logging
/// store as a single full-snapshot event (the durable read source) without a
/// second assembly pass: the sidecar file and the store row carry byte-identical
/// content.
///
/// Carries the pair block (`paired` + identity) read from the same on-disk
/// sources the Python `get_status` reads — the TX key for `paired` +
/// `public_key_fingerprint`, the `video.wfb` config for the peer id / paired-at
/// / auto-pair flag — plus the watchdog kill/stall counters. All key names match
/// `manager.get_status` exactly.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_stats_value(
    state: &str,
    channels: &ChannelTruth,
    reg: &RegSnapshot,
    effective_tx_dbm: i8,
    adapter: Option<&AdapterInfo>,
    link: &LinkStats,
    cfg: &WfbConfig,
    restart_count: u64,
    counters: &WatchdogCounters,
    rates: &TxRates,
    bitrate: &BitrateSnapshot,
) -> serde_json::Value {
    let (interface, chipset) = match adapter {
        Some(a) => (a.interface.as_str(), a.chipset.as_str()),
        None => ("", ""),
    };
    // Adapter verdicts are three-state on the wire. `injection_ok` is a claim
    // only when an adapter record exists (a real selection outcome, including
    // the scanned-and-none-found record); `usb_degraded` only when a USB speed
    // was actually enumerated. With no reading both are `null` — never a
    // confident `false`, which would let an unpaired / pre-scan rig render a
    // healthy green USB link (or a red no-injection claim) it never measured.
    let adapter_injection_ok = adapter.map(|a| a.injection_ok);
    let adapter_usb_speed_mbps = adapter.and_then(|a| a.usb_speed_mbps);
    let adapter_usb_degraded = adapter.and_then(|a| a.usb_speed_mbps.map(|_| a.usb_degraded));
    // Pair identity: the fingerprint + paired flag come from the TX key on disk,
    // the peer id / paired-at / auto-pair flag from the persisted config block.
    let fingerprint = read_public_fingerprint(Path::new(WFB_TX_KEY));
    let paired = fingerprint.is_some();
    // A real return-signal measurement requires a decoded packet. Without one
    // the signal-strength fields are the default sentinel (rssi -100), not a
    // reading — a transmit-dominant drone whose video reaches the peer but
    // decodes no inbound stream sits here. Reporting them as null (no
    // measurement) rather than -100 dBm keeps a healthy injecting drone from
    // looking like a weak-signal link, and lets the GCS show its air-side hint.
    // Same real-decode gate `derive_link_state` uses, so the state and the
    // numbers agree.
    let measured = link.packets_received > 0;
    let v = json!({
        // Sidecar schema version (best-effort drift signal for readers). Shared
        // with the ground-station writer via the one const so both agree.
        "version": ados_radio::paths::WFB_STATS_SIDECAR_VERSION,
        "state": state,
        // The state-machine state, surfaced under its own key so the panel can
        // show the recovery state directly. Mirrors `state` (the same wire
        // vocabulary, including `reg_blocked`); kept distinct so a future
        // state-machine value never collides with the legacy `state` consumers.
        "link_state": state,
        "interface": interface,
        // Back-compat alias: `channel` now reflects the LIVE interface channel
        // (was the configured value). Readers that only know the old key still
        // get reality. The split-out actual/rendezvous/operating fields below
        // carry the full truth.
        "channel": channels.actual,
        "actual_channel": channels.actual,
        "rendezvous_channel": channels.rendezvous,
        "operating_channel": channels.operating,
        // Live regulatory picture: the domain actually in force, whether it
        // matched the wanted domain, and the permitted channel set. A forbidden
        // domain the global set could not displace shows here instead of being
        // masked by a configured-channel-and-locked report.
        "reg_domain": reg.domain,
        "reg_verified": reg.verified,
        "enabled_channels": reg.enabled_channels,
        // Operating-region posture (additive): "unrestricted" (radio radiates on
        // the home channel with no pinned region; operator owns local compliance)
        // or "region" (the strict gate is enforced for the pinned country). The
        // GCS / OLED / webapp surface the amber unrestricted badge from these. The
        // camelCase keys match the cross-surface heartbeat contract; `regVerified`
        // mirrors `reg_verified` for the same readers.
        "regPosture": reg.posture.mode,
        "pinnedRegion": reg.posture.pinned_region,
        "regVerified": reg.verified,
        // Transmitting yet no confirmed reception within the grace window — the
        // loose-antenna / forbidden-band-cap / dead-peer case. False while the
        // link is proven OR while the transmit counter is flat (idle).
        "rf_unverified": channels.rf_unverified,
        "adapter_chipset": chipset,
        "adapter_injection_ok": adapter_injection_ok,
        // USB link health of the selected adapter. A full-speed (12 Mbps)
        // enumeration on an RTL adapter means it can advance tx_bytes yet emit
        // no usable RF — surfaced so the GCS warns instead of showing "connected".
        // `null` degraded = no enumeration happened, not a healthy link.
        "adapter_usb_speed_mbps": adapter_usb_speed_mbps,
        "adapter_usb_degraded": adapter_usb_degraded,
        "tx_power_dbm": effective_tx_dbm,
        "tx_power_max_dbm": cfg.tx_power_max_dbm,
        "topology": cfg.topology,
        // LIVE radio rate + redundancy from the controller snapshot (seeded from
        // config, refreshed each tick from the running data plane). Reports what
        // wfb_tx is actually transmitting after a runtime MCS/FEC/preset change,
        // not the boot-time config value.
        "mcs_index": bitrate.mcs_index,
        "fec_k": bitrate.fec_k,
        "fec_n": bitrate.fec_n,
        // Received-side lock proof, never hardcoded: a transmit-only end has no
        // decode stats of its own, so this is true only when a verified return
        // signal (a control-plane ack or a peer beacon) was heard recently.
        "channel_locked": channels.locked,
        // Which radio backend is driving the link. Always
        // "kernel" today — the Linux monitor-mode + wfb_tx/wfb_rx backend — so
        // Mission Control can badge the live radio path (additive; Rule 28).
        "backend": crate::backend::BackendKind::KernelMonitor.as_wire(),
        "profile": "drone",
        // Count of radio-group respawns since service start (watchdog kills,
        // hop restarts, return-home restarts) — surfaces churn to the panel.
        "restart_count": restart_count,
        // Pair identity block (matches manager.get_status key-for-key) so the
        // GCS radio panel renders pair identity without the cloud relay.
        "paired": paired,
        "paired_with_device_id": cfg.paired_with_device_id,
        "paired_at": cfg.paired_at,
        "public_key_fingerprint": fingerprint,
        "auto_pair_enabled": cfg.auto_pair_enabled,
        // Watchdog kill/stall counters (the watchdogs detect these; surfaced
        // here so the panel sees the same churn the Python heartbeat reports).
        "tx_zombie_kills": counters.tx_zombie_kills,
        "tx_video_stalled": counters.tx_video_stalled,
        "tx_video_stall_kills": counters.tx_video_stall_kills,
        "tx_video_recvq_bytes": counters.tx_video_recvq_bytes,
        // The TX PHY reads back at the muted not-permitted floor: it injects
        // frames but radiates nothing. Mission Control renders a "PHY muted"
        // badge so a silent dead link is a one-glance signal (Rule 28).
        "phy_muted": counters.phy_muted,
        // Smoothed radio transmit rate; valid_rx_packets_per_s is the uplink
        // valid-decode rate (0 on a drone-only rig with no rx.key).
        "tx_bytes_per_s": (rates.tx_bytes_per_s * 10.0).round() / 10.0,
        "valid_rx_packets_per_s": (rates.valid_rx_packets_per_s * 100.0).round() / 100.0,
        // Adaptive bitrate / FEC controller intent. `recommended_bitrate_kbps`
        // is the controller's chosen rung bitrate; the actual encoder restart is
        // a cross-process no-op here (the encoder lives in another service), so
        // the panel shows controller intent regardless. `link_preset` is the
        // operator-facing preset that seeded the MCS/FEC trio at bring-up.
        "link_preset": bitrate.link_preset,
        "adaptive_bitrate_enabled": bitrate.adaptive_bitrate_enabled,
        "recommended_bitrate_kbps": bitrate.recommended_bitrate_kbps,
        // The rung the controller currently recommends (the GCS shows the ladder
        // position alongside the live FEC so an adaptive step is legible).
        "recommended_tier_idx": bitrate.tier_idx,
        "recommended_tier_name": bitrate.tier_name,
        // Link-quality block (from the stats wfb_rx). Signal-strength fields are
        // null until a return signal is actually decoded (see `measured` above)
        // so the no-measurement sentinel never masquerades as a real reading;
        // the counters stay 0 (honestly zero received).
        "rssi_dbm": measured.then_some(link.rssi_dbm),
        "rssi_min": measured.then_some(link.rssi_min),
        "rssi_max": measured.then_some(link.rssi_max),
        "noise_dbm": measured.then_some(link.noise_dbm),
        "snr_db": measured.then_some(link.snr_db),
        "packets_received": link.packets_received,
        // Diagnostic RX counters from wfb_rx that separate the failure modes a bare
        // "0 received" hides: `packets_all` is every frame captured off-air BEFORE
        // decrypt (0 = deaf radio, no RF arriving); `decrypt_errors` are wrong-key /
        // wrong-link_id failures (RF arriving, not decodable); `packets_bad` are
        // corrupt frames; `session_packets` counts valid session-key packets.
        // `link_diag` is the one-glance verdict derived from them (deaf / mis_keyed /
        // jammed / healthy / searching) so a dead link's CAUSE is legible.
        "link_diag": link.link_diag,
        "packets_all": link.packets_all,
        "decrypt_errors": link.decrypt_errors,
        "packets_bad": link.packets_bad,
        "session_packets": link.session_packets,
        "packets_lost": link.packets_lost,
        "fec_recovered": link.fec_recovered,
        "fec_failed": link.fec_failed,
        "bitrate_kbps": link.bitrate_kbps,
        "loss_percent": link.loss_percent,
        "timestamp": link.timestamp,
    });
    v
}

/// Write the `wfb-stats.json` Contract E sidecar (atomic tmp+rename). Re-written
/// on a 2 s cadence so the handler's `mtime > 10 s → state="stale"` never trips.
/// Thin wrapper over [`build_stats_value`]; the heartbeat builds the body once
/// and ships the same value to the logging store alongside this file write.
///
/// When an `IngestEmitter` is passed, the SAME body is shipped to the logging
/// store as a single full-snapshot `link.wfb_status` event right after the file
/// write, so the durable read source and the on-disk sidecar stay in lockstep on
/// EVERY write — not just the heartbeat tick. A store-first read therefore never
/// returns a body older than the degraded-state file (bind-in-progress,
/// reg-blocked, no-adapter, unpaired, respawn-transient). Best-effort: an absent
/// or saturated logging daemon drops the event without disturbing the radio loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_stats_sidecar(
    state: &str,
    channels: &ChannelTruth,
    reg: &RegSnapshot,
    effective_tx_dbm: i8,
    adapter: Option<&AdapterInfo>,
    link: &LinkStats,
    cfg: &WfbConfig,
    restart_count: u64,
    counters: &WatchdogCounters,
    rates: &TxRates,
    bitrate: &BitrateSnapshot,
    metrics: Option<&ados_protocol::logd::emitter::IngestEmitter>,
) {
    let v = build_stats_value(
        state,
        channels,
        reg,
        effective_tx_dbm,
        adapter,
        link,
        cfg,
        restart_count,
        counters,
        rates,
        bitrate,
    );
    let path = run_path("wfb-stats.json");
    let _ = write_sidecar(&path, &v);
    if let Some(metrics) = metrics {
        metrics.emit_event(
            "link.wfb_status",
            ados_protocol::logd::Level::Info,
            json_object_to_fields(&v),
        );
    }
}

/// Convert a JSON object into the logging store's open detail map (`Fields`), so
/// the full wfb-status body can ride a single `link.wfb_status` event. Recurses
/// through nested arrays / objects; numbers preserve their integer-vs-float kind,
/// and JSON null round-trips to msgpack nil. A non-object input yields an empty
/// map (the body is always an object, so this only guards against a future
/// change). The map decodes back to the identical JSON in the query path, so the
/// store row is a faithful copy of the sidecar.
pub(crate) fn json_object_to_fields(value: &serde_json::Value) -> ados_protocol::logd::Fields {
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
/// and null becomes nil, so the round-trip through the store preserves the exact
/// JSON shape the REST base merges over.
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

/// Read the device-id from the canonical agent location (`/etc/ados/device-id`,
/// hyphen — matches `core/paths.py:122 DEVICE_ID_PATH`).
pub(crate) fn read_device_id() -> String {
    std::fs::read_to_string("/etc/ados/device-id")
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reg_gate::STATE_REG_BLOCKED;

    #[test]
    fn reg_posture_default_is_unrestricted() {
        let p = RegPosture::default();
        assert_eq!(p.mode, "unrestricted");
        assert!(p.pinned_region.is_none());
    }

    #[test]
    fn reg_posture_new_reflects_mode() {
        let unrestricted = RegPosture::new(true, "US");
        assert_eq!(unrestricted.mode, "unrestricted");
        assert!(unrestricted.pinned_region.is_none());

        let region = RegPosture::new(false, "DE");
        assert_eq!(region.mode, "region");
        assert_eq!(region.pinned_region.as_deref(), Some("DE"));
    }

    #[test]
    fn fingerprint_none_when_key_absent_or_wrong_size() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file → None.
        assert!(read_public_fingerprint(&dir.path().join("nope.key")).is_none());
        // Wrong size (not 64 bytes) → None.
        let short = dir.path().join("short.key");
        std::fs::write(&short, vec![0u8; 32]).unwrap();
        assert!(read_public_fingerprint(&short).is_none());
    }

    #[test]
    fn fingerprint_is_16_hex_of_blake2b_8_over_public_half() {
        use blake2::digest::{Update, VariableOutput};
        use blake2::Blake2bVar;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("tx.key");
        // 64-byte key: first 32 are the secret half, second 32 the public half.
        let mut data = vec![0u8; 64];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        std::fs::write(&key, &data).unwrap();
        let got = read_public_fingerprint(&key).unwrap();
        // Recompute independently over the second 32 bytes.
        let mut h = Blake2bVar::new(8).unwrap();
        h.update(&data[32..]);
        let mut out = [0u8; 8];
        h.finalize_variable(&mut out).unwrap();
        assert_eq!(got, hex::encode(out));
        assert_eq!(got.len(), 16);
        assert!(got
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn channel_truth_configured_reports_rendezvous_for_all_channels() {
        // Before the interface exists, the truth reports the rendezvous home for
        // actual/rendezvous/operating, with the link not proven and no tx.
        let t = ChannelTruth::configured(149);
        assert_eq!(t.actual, 149);
        assert_eq!(t.rendezvous, 149);
        assert_eq!(t.operating, 149);
        assert!(!t.locked);
        assert!(!t.rf_unverified);
    }

    /// Serialize tests that mutate the process-global `ADOS_RUN_DIR` env var.
    static SIDECAR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn read_sidecar(dir: &std::path::Path) -> serde_json::Value {
        let body = std::fs::read(dir.join("wfb-stats.json")).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[test]
    fn sidecar_carries_truthful_channel_and_reg_fields() {
        let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());

        // A locked link on a live fallback channel under a verified domain: the
        // actual channel differs from the rendezvous home (the fallback-frequency
        // case), the link is proven, and rf_unverified is clear.
        let channels = ChannelTruth {
            actual: 161,
            rendezvous: 149,
            operating: 157,
            locked: true,
            rf_unverified: false,
        };
        let reg = RegSnapshot {
            domain: Some("US".to_string()),
            verified: true,
            enabled_channels: vec![149, 153, 157, 161, 165],
            ..RegSnapshot::default()
        };
        write_stats_sidecar(
            "connected",
            &channels,
            &reg,
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
            None,
        );
        let v = read_sidecar(dir.path());

        // The back-compat `channel` alias now equals the LIVE actual channel.
        assert_eq!(v["channel"], 161);
        assert_eq!(v["actual_channel"], 161);
        assert_eq!(v["rendezvous_channel"], 149);
        assert_eq!(v["operating_channel"], 157);
        assert_eq!(v["reg_domain"], "US");
        assert_eq!(v["reg_verified"], true);
        assert_eq!(
            v["enabled_channels"],
            serde_json::json!([149, 153, 157, 161, 165])
        );
        // channel_locked is the proof-derived value, not hardcoded true.
        assert_eq!(v["channel_locked"], true);
        assert_eq!(v["rf_unverified"], false);
        // The radio backend is the kernel monitor-mode path today (ADOS Direct
        // Link: the userspace USB backend is a later wave).
        assert_eq!(v["backend"], "kernel");
        // link_state mirrors the lifecycle state string. The default LinkStats
        // sentinel (0 decoded packets) is a transmit-dominant drone injecting
        // RF, not a degraded link, so it surfaces as connected.
        assert_eq!(v["link_state"], "connected");
        // No return signal decoded → the signal-strength fields are null, not
        // the -100 dBm sentinel, so a healthy injecting drone is never shown as
        // a weak-signal link. The counters stay 0 (honestly zero received).
        assert!(v["rssi_dbm"].is_null());
        assert!(v["snr_db"].is_null());
        assert!(v["noise_dbm"].is_null());
        assert_eq!(v["packets_received"], 0);
        // Default RegSnapshot posture: unrestricted with no pinned region. The
        // cross-surface contract keys carry the honest operating-region state.
        assert_eq!(v["regPosture"], "unrestricted");
        assert!(v["pinnedRegion"].is_null());
        assert_eq!(v["regVerified"], true);

        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn sidecar_surfaces_pinned_region_posture() {
        let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());

        let reg = RegSnapshot {
            domain: Some("DE".to_string()),
            verified: true,
            enabled_channels: vec![149],
            posture: RegPosture::new(false, "DE"),
        };
        write_stats_sidecar(
            "connected",
            &ChannelTruth::configured(149),
            &reg,
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
            None,
        );
        let v = read_sidecar(dir.path());
        assert_eq!(v["regPosture"], "region");
        assert_eq!(v["pinnedRegion"], "DE");
        assert_eq!(v["regVerified"], true);

        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn json_object_to_fields_round_trips_nulls_arrays_and_number_kinds() {
        // The store decodes the detail map back to JSON via `Fields` ->
        // serde_json, so a value built here must equal the original after the
        // round-trip: null stays null, the channel array stays an array, an int
        // stays an int (not a float), and a negative reading keeps its sign.
        let body = serde_json::json!({
            "pinnedRegion": serde_json::Value::Null,
            "enabled_channels": [149, 153, 157, 161, 165],
            "channel": 161,
            "rssi_dbm": -53.0,
            "paired": true,
            "adapter_chipset": "RTL8812EU",
        });
        let fields = json_object_to_fields(&body);
        // Round-trip the detail map through the real wire path: frame it as an
        // event, encode it (the length-prefixed msgpack the store ingests), strip
        // the 4-byte length header the same way the ingest reader does, decode,
        // then map the decoded detail back to JSON the way the query layer does
        // (`serde_json::to_value` over the decoded `Fields`).
        use ados_protocol::frame::{decode_len, HEADER_SIZE};
        use ados_protocol::logd::{EventFrame, IngestFrame, Level, LOGD_MAX_FRAME};
        let mut frame = EventFrame::new(0, "link.wfb_status", "ados-radio", Level::Info);
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
        // The null channel must survive as a JSON null, not be dropped.
        assert!(back["pinnedRegion"].is_null());
        // The array round-trips as an array.
        assert_eq!(
            back["enabled_channels"],
            serde_json::json!([149, 153, 157, 161, 165])
        );
        // The integer channel stays an integer (a float would render 161.0).
        assert_eq!(back["channel"], serde_json::json!(161));
    }

    #[test]
    fn build_stats_value_matches_the_sidecar_file_body() {
        // The body shipped to the store must be the exact value written to the
        // sidecar file, so the durable read source is byte-identical to the live
        // fallback.
        let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());
        let channels = ChannelTruth {
            actual: 161,
            rendezvous: 149,
            operating: 157,
            locked: true,
            rf_unverified: false,
        };
        let reg = RegSnapshot {
            domain: Some("US".to_string()),
            verified: true,
            enabled_channels: vec![149, 153, 157, 161, 165],
            ..RegSnapshot::default()
        };
        let built = build_stats_value(
            "connected",
            &channels,
            &reg,
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
        );
        write_stats_sidecar(
            "connected",
            &channels,
            &reg,
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
            None,
        );
        let on_disk = read_sidecar(dir.path());
        assert_eq!(built, on_disk);
        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[tokio::test]
    async fn write_stats_sidecar_emits_the_status_event_on_a_degraded_write() {
        // A degraded-state write (here `reg_blocked`, the bind/error class of
        // call site, not the heartbeat) must ship the same body to the store as a
        // `link.wfb_status` event so a store-first read never lags the file. The
        // emitter records every enqueue, so a single write with an emitter present
        // increments the enqueued counter by exactly one regardless of whether a
        // logging daemon is listening on the socket.
        let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());

        let emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-radio",
            dir.path().join("ingest.sock"),
        );
        let stats = emitter.stats();
        let before = stats.enqueued();
        write_stats_sidecar(
            STATE_REG_BLOCKED,
            &ChannelTruth::configured(149),
            &RegSnapshot::default(),
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
            Some(&emitter),
        );
        // Exactly one event was enqueued by the degraded-state write.
        assert_eq!(stats.enqueued(), before + 1);
        // The file write is still byte-identical to the build (the live fallback).
        let on_disk = read_sidecar(dir.path());
        assert_eq!(on_disk["state"], "reg_blocked");

        // With no emitter the write still produces the file but enqueues nothing.
        let none_emitter = ados_protocol::logd::emitter::IngestEmitter::with_socket(
            "ados-radio",
            dir.path().join("ingest2.sock"),
        );
        let none_stats = none_emitter.stats();
        write_stats_sidecar(
            STATE_REG_BLOCKED,
            &ChannelTruth::configured(149),
            &RegSnapshot::default(),
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
            None,
        );
        assert_eq!(none_stats.enqueued(), 0);

        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn stats_report_no_adapter_verdicts_before_a_scan() {
        // No adapter resolved (the unpaired / pre-scan writes pass `None`):
        // the injection and USB-degraded verdicts must be JSON null — no
        // reading — never a confident false. A false here is exactly what an
        // unpaired rig used to write, and a downstream three-state consumer
        // rendered it as a measured-healthy green USB link for hardware
        // nothing ever enumerated.
        let body = build_stats_value(
            "unpaired",
            &ChannelTruth::configured(149),
            &RegSnapshot::default(),
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
        );
        assert!(
            body["adapter_injection_ok"].is_null(),
            "no scan ⇒ no injection verdict, got {:?}",
            body["adapter_injection_ok"]
        );
        assert!(
            body["adapter_usb_degraded"].is_null(),
            "no enumeration ⇒ no USB-degraded verdict, got {:?}",
            body["adapter_usb_degraded"]
        );
        assert!(body["adapter_usb_speed_mbps"].is_null());
    }

    #[test]
    fn stats_report_a_measured_no_adapter_verdict_after_a_scan() {
        // The scan ran and proved no injection-capable radio: the injection
        // verdict is a MEASURED false (the loud stranded-radio signal must
        // still fire), while the USB fields stay null — nothing enumerated.
        let body = build_stats_value(
            "no_adapter",
            &ChannelTruth::configured(149),
            &RegSnapshot::default(),
            5,
            Some(&AdapterInfo::none_found()),
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
        );
        assert_eq!(body["adapter_injection_ok"], false);
        assert!(body["adapter_usb_degraded"].is_null());
        assert!(body["adapter_usb_speed_mbps"].is_null());
    }

    #[test]
    fn stats_report_a_resolved_adapters_verdicts_as_real_booleans() {
        // The three-state contract must not swallow real readings: a selected
        // adapter with an enumerated speed reports actual booleans in both
        // directions, so the degraded warning and the healthy reading both
        // stay live.
        for (speed, degraded) in [(Some(12u32), true), (Some(480u32), false)] {
            let adapter = AdapterInfo {
                interface: "wlan1".to_string(),
                chipset: "RTL8812EU".to_string(),
                injection_ok: true,
                usb_speed_mbps: speed,
                usb_degraded: degraded,
            };
            let body = build_stats_value(
                "connected",
                &ChannelTruth::configured(149),
                &RegSnapshot::default(),
                5,
                Some(&adapter),
                &LinkStats::default(),
                &WfbConfig::default(),
                0,
                &WatchdogCounters::default(),
                &TxRates::default(),
                &BitrateSnapshot::default(),
            );
            assert_eq!(body["adapter_injection_ok"], true);
            assert_eq!(body["adapter_usb_degraded"], degraded);
            assert_eq!(body["adapter_usb_speed_mbps"], speed.unwrap());
        }
        // An adapter whose USB speed could not be read has no degraded
        // verdict, even though it was resolved: `usb_degraded` derives from
        // the speed reading and must not outlive its absence.
        let unread = AdapterInfo {
            interface: "wlan1".to_string(),
            chipset: "RTL8812EU".to_string(),
            injection_ok: true,
            usb_speed_mbps: None,
            usb_degraded: false,
        };
        let body = build_stats_value(
            "connected",
            &ChannelTruth::configured(149),
            &RegSnapshot::default(),
            5,
            Some(&unread),
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
        );
        assert_eq!(body["adapter_injection_ok"], true);
        assert!(body["adapter_usb_degraded"].is_null());
    }

    #[test]
    fn sidecar_reports_rf_unverified_and_unlocked_when_transmitting_blind() {
        let _guard = SIDECAR_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ADOS_RUN_DIR", dir.path());

        // Transmitting (tx advancing) but no confirmed reception: locked false,
        // rf_unverified true — the exact transmitting-zero-reception case.
        let channels = ChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
            locked: false,
            rf_unverified: true,
        };
        let reg = RegSnapshot {
            domain: Some("BO".to_string()),
            verified: false,
            enabled_channels: vec![],
            ..RegSnapshot::default()
        };
        write_stats_sidecar(
            STATE_REG_BLOCKED,
            &channels,
            &reg,
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
            None,
        );
        let v = read_sidecar(dir.path());
        assert_eq!(v["channel_locked"], false);
        assert_eq!(v["rf_unverified"], true);
        // The forbidden domain the global set could not displace is visible.
        assert_eq!(v["reg_domain"], "BO");
        assert_eq!(v["reg_verified"], false);
        assert_eq!(v["enabled_channels"], serde_json::json!([]));
        assert_eq!(v["state"], "reg_blocked");

        std::env::remove_var("ADOS_RUN_DIR");
    }

    #[test]
    fn sidecar_state_agrees_with_the_rf_unverified_boolean() {
        // The derived state string and the standalone boolean are two views of
        // one verdict; a body that says `rf_unverified: true` while the state
        // reads `connected` is the false-healthy surface this pairing exists to
        // prevent, so the two are asserted together on one body.
        let state = ados_radio::link_state::derive_link_state(
            true,
            false,
            &LinkStats::default(),
            true,  // transmitting
            false, // no confirmed reception
        );
        assert_eq!(state, ados_radio::link_state::LinkState::RfUnverified);

        let channels = ChannelTruth {
            actual: 149,
            rendezvous: 149,
            operating: 149,
            locked: false,
            rf_unverified: true,
        };
        let body = build_stats_value(
            state.as_str(),
            &channels,
            &RegSnapshot::default(),
            5,
            None,
            &LinkStats::default(),
            &WfbConfig::default(),
            0,
            &WatchdogCounters::default(),
            &TxRates::default(),
            &BitrateSnapshot::default(),
        );
        assert_eq!(body["state"], "rf_unverified");
        assert_eq!(body["link_state"], "rf_unverified");
        assert_eq!(body["rf_unverified"], true);
        assert_eq!(body["channel_locked"], false);
    }
}
