//! The ground `wfb-stats.json` sidecar builder + the reg-blocked sidecar writer.
//!
//! `build_gs_stats` assembles the GS-extras payload the cross-process API +
//! heartbeat read, symmetric with the drone-side sidecar so the panel reads one
//! shape from either rig. `write_reg_blocked_sidecar` writes a minimal blocked
//! sidecar while the run loop retries the regulatory gate.

mod mpv;

use ados_radio::config::WfbConfig;
use ados_radio::link_quality::LinkStats;
use ados_radio::reg_gate::Posture;

use super::args::{
    STATE_BLOCKED_UNPAIRED, STATE_NO_ADAPTER, STATE_NO_INJECTION, STATE_REG_BLOCKED,
};

use mpv::json_to_mpv;

mod sidecars;
pub use sidecars::{
    write_blocked_unpaired_sidecar, write_no_adapter_sidecar, write_no_injection_sidecar,
    write_reg_blocked_sidecar,
};

/// The regulatory picture the receive sidecar surfaces, symmetric with the
/// drone side. `domain` is the LIVE global country (`None` when unreadable);
/// `verified` is true only when it matched the wanted domain; `enabled_channels`
/// is the domain's permitted set (empty = could not determine); `posture` is
/// the regulatory posture the receive plane runs under (the same resolver the
/// drone uses), `None` when it was not resolved. Resolved by
/// `prepare_interfaces` and stored on the manager so every sidecar write carries
/// the same regulatory truth instead of nothing.
#[derive(Debug, Clone, Default)]
pub struct GsRegSnapshot {
    pub domain: Option<String>,
    pub verified: bool,
    pub enabled_channels: Vec<u8>,
    pub posture: Option<Posture>,
}

/// The `reg_posture` sidecar value: `{"domain", "strict"}` where `domain` is
/// the domain the posture forces on the PHY (`null` under the unrestricted
/// posture), or `null` when no posture was resolved.
fn reg_posture_json(posture: Option<&Posture>) -> serde_json::Value {
    match posture {
        Some(p) => serde_json::json!({
            "domain": (!p.is_unrestricted()).then(|| p.domain_to_set.clone()),
            "strict": p.strict,
        }),
        None => serde_json::Value::Null,
    }
}

/// The posture from the agent config, for the sidecar writers that run before
/// `prepare_interfaces` resolved one.
fn configured_posture() -> GsRegSnapshot {
    GsRegSnapshot {
        posture: Some(Posture::load(std::path::Path::new(
            crate::paths::CONFIG_YAML,
        ))),
        ..GsRegSnapshot::default()
    }
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
/// adapter always carries a chipset (`From<&DetectedAdapter>` sets it), so
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

impl From<&ados_radio::adapter::DetectedAdapter> for GsAdapterInfo {
    fn from(adapter: &ados_radio::adapter::DetectedAdapter) -> Self {
        Self::new(
            Some(adapter.chipset.clone()),
            adapter.verified,
            adapter.usb_speed_mbps,
        )
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
    let mut body = serde_json::json!({
        // Sidecar schema version (best-effort drift signal for readers). Shared
        // with the drone-side writer via the one const so both rigs agree.
        "version": ados_radio::paths::WFB_STATS_SIDECAR_VERSION,
        // Top-level lifecycle string, mirroring the drone-side sidecar so the GS
        // heartbeat reads a real state instead of null.
        "state": state,
        // The state-machine state under its own key, mirroring the drone side.
        "link_state": state,
        // The transmit (primary) adapter. Every adapter, receive-only ones
        // included, is listed under `adapters`.
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
        // The LIVE interface channel, the rendezvous home and the channel the
        // pair is operating on after any committed hop.
        "actual_channel": channels.actual,
        "rendezvous_channel": channels.rendezvous,
        "operating_channel": channels.operating,
        // Live regulatory picture, symmetric with the drone side.
        "reg_domain": reg.domain,
        "reg_verified": reg.verified,
        "enabled_channels": reg.enabled_channels,
        // The regulatory posture this plane runs under, resolved from the same
        // config by the same resolver as the drone, so both ends can be
        // compared field for field.
        "reg_posture": reg_posture_json(reg.posture.as_ref()),
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
        // The MCS the link is ACTUALLY running at, measured off-air from the
        // receiving driver's radiotap header (`RX_ANT`'s `<freq>:<mcs>:<bw>`
        // group) — i.e. the drone's real transmit rung, not this node's own
        // configured uplink rate. Every other field in the link block a reader
        // pairs this with (`snr_db`, `rssi_dbm`, `loss_percent`) is a receive-side
        // measurement of the drone's downlink, so a config echo here would report
        // the ground station's uplink rung as the video link's and would sit at
        // the boot value forever while the drone's adaptive ladder moved.
        //
        // Not wrapped in `measured` like its neighbours: it carries its own
        // honesty gate (`None` until a frame decoded and reported a PHY group),
        // and an `RX_ANT` line only exists for frames actually received.
        //
        // Single-drone truth: only the primary slot's `wfb_rx` feeds the stats
        // reader, so on a multi-slot fleet this is the PRIMARY drone's rung, not
        // a fleet aggregate. Per-drone measured MCS needs a stats stream per slot.
        "mcs_index": snap.mcs_index,
        // The channel width frames arrived at. Pinned at 20 MHz by every ADOS
        // transmitter, so anything else is a peer worth noticing.
        "bandwidth_mhz": snap.bandwidth_mhz,
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
        // (additive).
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
    });
    // Version 2: the per-adapter list and the diversity block. Empty here; the
    // stats reader overwrites both from the live stream, so every other writer
    // (blocked, no adapter, reg-blocked) carries the honest empty shape.
    super::adapter_stats::insert_body(
        &mut body,
        &ados_protocol::radio_adapters::RadioAdaptersBody::default(),
    );
    body
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

#[cfg(test)]
mod tests;
