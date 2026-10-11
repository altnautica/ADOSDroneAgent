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

use crate::bitrate::BitrateSnapshot;
use crate::config::WfbConfig;
use crate::link_quality::LinkStats;
use crate::paths::{run_path, write_sidecar, WFB_TX_KEY};
use crate::watchdog::WatchdogCounters;

use crate::daemon::txrate::TxRates;

mod fields;
pub(crate) use fields::json_object_to_fields;

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
pub(crate) fn write_adapters_sidecar(adapters: &[crate::adapter::WifiAdapterInfo]) {
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
    use blake2::digest::{consts::U8, Digest};
    use blake2::Blake2b;
    const WFB_KEY_FILE_BYTES: usize = 64;
    const WFB_PUBLIC_HALF_OFFSET: usize = 32;
    let data = std::fs::read(path).ok()?;
    if data.len() != WFB_KEY_FILE_BYTES {
        return None;
    }
    Some(hex::encode(Blake2b::<U8>::digest(
        &data[WFB_PUBLIC_HALF_OFFSET..],
    )))
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
    // The predicate lives on `LinkStats` so the durable link-history emitters
    // apply the SAME gate: a sidecar that nulls a sentinel while the telemetry
    // series records it is worse than either alone, because the two surfaces
    // then disagree about whether the reading exists.
    let measured = link.is_measured();
    let v = json!({
        // Sidecar schema version (best-effort drift signal for readers). Shared
        // with the ground-station writer via the one const so both agree.
        "version": crate::paths::WFB_STATS_SIDECAR_VERSION,
        "state": state,
        // The state-machine state, surfaced under its own key so the panel can
        // show the recovery state directly. It carries the same wire vocabulary
        // as `state` above, including `reg_blocked`; kept distinct so a future
        // state-machine value never collides with the legacy `state` consumers.
        "link_state": state,
        "interface": interface,
        // The LIVE interface channel, the rendezvous home and the channel the
        // pair is operating on after any committed hop.
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
        // The measured TX power (dBm) from the live `iw` readback; `null` when
        // it was not read. Lets the panel show a driver that runs at another
        // power than the one it accepted.
        "tx_power_readback_dbm": counters.tx_power_readback_dbm,
        "tx_power_max_dbm": cfg.tx_power_max_dbm,
        "topology": cfg.topology,
        // LIVE radio rate + redundancy from the controller snapshot (seeded from
        // config, refreshed each tick from the running data plane). Reports what
        // wfb_tx is actually transmitting after a runtime MCS/FEC/preset change,
        // not the boot-time config value.
        "mcs_index": bitrate.mcs_index,
        "fec_k": bitrate.fec_k,
        "fec_n": bitrate.fec_n,
        // The adaptive SNR/MCS ladder's configured top rung. Without it the
        // operator cannot tell a link-limited rung from a policy-limited one:
        // MCS 3 at 35 dB reads as "the link is bad" unless the panel can also
        // say "the ladder is capped at 3".
        "mcs_ladder_cap": bitrate.mcs_ladder_cap,
        // Received-side lock proof, never hardcoded: a transmit-only end has no
        // decode stats of its own, so this is true only when a verified return
        // signal (a control-plane ack or a peer beacon) was heard recently.
        "channel_locked": channels.locked,
        // Which radio backend is driving the link. Always "kernel": the Linux
        // monitor-mode + wfb_tx/wfb_rx path, so Mission Control can badge the
        // live radio path.
        "backend": "kernel",
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
        // badge so a silent dead link is a one-glance signal.
        "phy_muted": counters.phy_muted,
        // Smoothed radio transmit rate; valid_rx_packets_per_s is the uplink
        // valid-decode rate (0 on a drone-only rig with no rx.key).
        "tx_bytes_per_s": (rates.tx_bytes_per_s * 10.0).round() / 10.0,
        "valid_rx_packets_per_s": (rates.valid_rx_packets_per_s * 100.0).round() / 100.0,
        // Adaptive bitrate / FEC / MCS controller. `recommended_bitrate_kbps` is
        // the rung the controller chose; `encoder_bitrate_kbps` is what
        // `ados-video` reports actually running (the rung applied as a ceiling
        // under the hero/thumbnail profile, i.e. `min(profile, ceiling)`), so the
        // panel can tell intent from reality. `null` there means `ados-video`
        // published no state — an unreachable encoder, NOT an unclamped one.
        // `link_preset` is the operator-facing preset that seeded the MCS/FEC
        // trio at bring-up.
        "link_preset": bitrate.link_preset,
        "adaptive_bitrate_enabled": bitrate.adaptive_bitrate_enabled,
        "recommended_bitrate_kbps": bitrate.recommended_bitrate_kbps,
        "encoder_bitrate_kbps": bitrate.encoder_bitrate_kbps,
        // The rung the controller currently recommends (the GCS shows the ladder
        // position alongside the live FEC so an adaptive step is legible).
        "recommended_tier_idx": bitrate.tier_idx,
        "recommended_tier_name": bitrate.tier_name,
        // Which measurement drove the rung: "peer" (the receiving station's
        // report of what it decoded, the only measurement of this node's
        // downlink) or "none" (no usable sample, so the modulation rung falls to
        // its floor and the bitrate rung follows queue congestion). Without this
        // a rung chosen for want of any signal is indistinguishable from one
        // chosen on a good sample.
        "sample_source": bitrate.sample_source,
        // The downlink loss the `sample_source` measurement carries, or null
        // when there is no usable sample.
        //
        // `loss_percent` below is this node's OWN receive counter, which
        // measures the uplink, the other direction. The ground station's
        // measurement of the downlink is published here beside the source that
        // identifies who took it — a loss figure whose provenance is invisible
        // is one an operator cannot act on.
        "measured_loss_percent": bitrate.sample_loss_percent,
        // How the live FEC/MCS retunes were applied. `tx_cmd_applies` went over
        // the running transmitter's wfb-ng 24.08 management socket (no video
        // gap); `respawn_applies` fell back to kill-and-respawn (a 1-2 s gap).
        // A rig where the respawn counter grows is a rig where `-C` never
        // reached wfb_tx — the single most useful signal for diagnosing "why
        // does video stutter on every tier change".
        "tx_cmd_applies": bitrate.tx_cmd_applies,
        "respawn_applies": bitrate.respawn_applies,
        "tx_cmd_failures": bitrate.tx_cmd_failures,
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
        // Which uplink receiver (control or aux) decoded the interval behind this
        // block, and how old that interval is. A block held across intervals that
        // decoded nothing keeps its reading and shows its age instead of
        // flapping to null between the control plane's sparse beacons.
        "uplink_plane": link.uplink_plane,
        "age_ms": link.age_ms(std::time::Instant::now()),
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

#[cfg(test)]
mod tests;
