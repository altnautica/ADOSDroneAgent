//! The radio and CRSF link blocks of the heartbeat payload.

use super::*;

/// The snake_case `radio` link block (`state`, `iface`, `driver`, `channel`,
/// `freq_mhz`, ...). This is the BASE block the native loop emits; the radio
/// enrichment producer replaces the whole `radio` key when it has a live view,
/// so this carries a subset of that producer's field set rather than mirroring
/// it one-for-one. The Python builder keeps `None` values in this nested dict
/// (only top-level keys are stripped), so these `Option`s serialize as JSON
/// `null` when absent — that is intentional parity. The block is always present
/// in the payload (an all-`absent` block when no radio status is available).
/// `Deserialize` + a container `default` so the block can be built by
/// deserializing `ados_protocol::wfb_status::build_radio_block`'s output — the one
/// derivation, shared with the LAN routes, rather than a second one written here.
/// The container default is what lets the three `skip_serializing_if` fields and
/// the non-`Option` `paired` come back from the 37-key absent form.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RadioBlock {
    pub state: Option<String>,
    pub iface: Option<String>,
    pub driver: Option<String>,
    pub channel: Option<i64>,
    pub freq_mhz: Option<i64>,
    pub bandwidth_mhz: Option<i64>,
    // tx power is an integer dBm on the source (the wfb manager's effective
    // power is `int | None`); model as i64 so the wire is `20`, not `20.0`.
    pub tx_power_dbm: Option<i64>,
    pub tx_power_max_dbm: Option<i64>,
    pub topology: Option<String>,
    pub rssi_dbm: Option<f64>,
    pub snr_db: Option<f64>,
    pub noise_dbm: Option<f64>,
    pub bitrate_kbps: Option<i64>,
    pub fec_recovered: Option<i64>,
    pub fec_lost: Option<i64>,
    pub packets_lost: Option<i64>,
    pub loss_percent: Option<f64>,
    pub mcs_index: Option<i64>,
    pub rx_silent_seconds: Option<f64>,
    pub paired: bool,
    pub paired_with_device_id: Option<String>,
    pub paired_at: Option<String>,
    pub public_key_fingerprint: Option<String>,
    pub auto_pair_enabled: Option<bool>,
    pub tx_video_stalled: Option<bool>,
    pub tx_video_stall_kills: Option<i64>,
    pub tx_video_recvq_bytes: Option<i64>,
    pub acquire_state: Option<String>,
    pub channel_locked: Option<bool>,
    /// The two halves of the received-side proof. `channel_locked` is true once
    /// a verified return signal was heard; `rf_unverified` is true when the
    /// transmit counter advances while none has been. `None` when there is no
    /// verdict to report — never a confident `false`, which would claim an
    /// unproven transmit path had been proven.
    pub rf_unverified: Option<bool>,
    pub reacquire_kills: Option<i64>,
    pub valid_rx_packets_per_s: Option<f64>,
    pub adapter_chipset: Option<String>,
    /// The adapter injection verdict. `Some(false)` is a MEASURED outcome (the
    /// scan ran and proved no injection-capable adapter — the loud stranded
    /// radio signal); `None` means no verdict exists (no radio view, a
    /// never-scanned rig) and serializes as JSON `null` so the three-state
    /// consumer reads "unknown" instead of a fabricated scan result.
    pub adapter_injection_ok: Option<bool>,
    /// The selected adapter's USB link health. A full-speed (12 Mbps) RTL
    /// enumeration advances `tx_bytes_per_s` yet emits no usable RF; the speed +
    /// degraded flag let a consumer warn instead of reading the advancing counter
    /// as a healthy link. `None` (never a confident `false` for `_degraded`) when
    /// no enumeration happened — a `false` there claims a measured-healthy USB link
    /// nothing examined.
    pub adapter_usb_speed_mbps: Option<i64>,
    pub adapter_usb_degraded: Option<bool>,
    /// The TX PHY pinned at the muted not-permitted floor: it injects frames but
    /// radiates nothing. `None` on a view with no TX PHY reading (the receive side,
    /// an older sidecar) — never a fabricated `false` asserting a healthy PHY.
    pub phy_muted: Option<bool>,
    /// Radio-data-plane churn + transmit-rate observability, forwarded so a
    /// cloud-reached node surfaces a thrashing or zombie transmitter. These three
    /// are SIDECAR-ONLY: the wire contract OMITS them from the absent
    /// block (they only mean anything with a live radio view), so they carry
    /// `skip_serializing_if` to mirror that exactly — omitted (not `null`) on
    /// `absent()`, present (value or null) once a live view populates them. A `None`
    /// on a populated block still reads as "no reading" (never a fabricated zero).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_zombie_kills: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_bytes_per_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_count: Option<i64>,
}

impl RadioBlock {
    /// The all-`absent` block the Python builder emits when no radio status is
    /// available (manager absent, RTL not plugged in, ground-station profile).
    /// Mirrors the `if not wfb_status:` branch of `build_radio_block`.
    pub fn absent() -> Self {
        RadioBlock {
            state: Some("absent".to_string()),
            iface: None,
            driver: None,
            channel: None,
            freq_mhz: None,
            bandwidth_mhz: None,
            tx_power_dbm: None,
            tx_power_max_dbm: None,
            topology: None,
            rssi_dbm: None,
            snr_db: None,
            noise_dbm: None,
            bitrate_kbps: None,
            fec_recovered: None,
            fec_lost: None,
            packets_lost: None,
            loss_percent: None,
            mcs_index: None,
            rx_silent_seconds: None,
            paired: false,
            paired_with_device_id: None,
            paired_at: None,
            public_key_fingerprint: None,
            auto_pair_enabled: None,
            tx_video_stalled: None,
            tx_video_stall_kills: None,
            tx_video_recvq_bytes: None,
            acquire_state: None,
            channel_locked: None,
            rf_unverified: None,
            reacquire_kills: None,
            valid_rx_packets_per_s: None,
            adapter_chipset: None,
            // Null, not false: with no radio view no adapter scan ever ran, so
            // there is no injection verdict to report.
            adapter_injection_ok: None,
            // Null, not false: with no radio view no adapter USB speed was
            // enumerated and no PHY was read, so a `false` would claim a measured
            // healthy-USB-link / unmuted-PHY state nothing examined. These three
            // serialize as `null`, matching the Python absent block.
            adapter_usb_speed_mbps: None,
            adapter_usb_degraded: None,
            phy_muted: None,
            // The churn counters are sidecar-only: `skip_serializing_if` OMITS them
            // from the absent block (they mean nothing without a live radio view),
            // exactly as the Python absent branch does.
            tx_zombie_kills: None,
            tx_bytes_per_s: None,
            restart_count: None,
        }
    }
}

/// The OPTIONAL nested `crsf` block: the CRSF/ExpressLRS RC control lane's
/// state, projected from the lane service's stats sidecar
/// (`/run/ados/crsf-stats.json`). Exactly the pinned sidecar field set,
/// snake_case like the `radio` block (the receiver's generic snake→camel remap
/// owns the casing). The whole block is ABSENT from the payload when the lane
/// service is not running or its sidecar is stale — never a fabricated
/// all-null block for a node without the lane. Inside a present block each
/// field keeps its nested `null` when unmeasured (the Python projection keeps
/// `None` values, matching the radio-block convention), so there is no
/// `skip_serializing_if` on the fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CrsfBlock {
    /// The sidecar schema version, forwarded so the receiver can gate on it.
    pub v: Option<i64>,
    /// `unconfigured|ready|link_ok|degraded|rf_unverified|disabled`.
    pub state: Option<String>,
    pub rssi_dbm: Option<i64>,
    pub lq_uplink: Option<i64>,
    pub lq_downlink: Option<i64>,
    pub snr_db: Option<i64>,
    pub band: Option<String>,
    pub packet_rate_hz: Option<i64>,
    /// Conducted TX power in MILLIWATTS, mapped from the module's power-table
    /// index by the lane. Matches the sidecar + LAN route key `tx_power_mw`; a
    /// `tx_power_dbm` here silently dropped the value (serde ignored the real
    /// key), so a cloud-reached node showed no TX power.
    pub tx_power_mw: Option<i64>,
    pub tx_frames_per_s: Option<f64>,
    pub rx_frames_per_s: Option<f64>,
    /// The transmit-proof verdict: `Some(false)` = a received-side proof
    /// exists, `Some(true)` = transmitting provably unheard, `None` = no
    /// verdict yet — never a fabricated false.
    pub rf_unverified: Option<bool>,
    pub mode: Option<String>,
    pub channel_source: Option<String>,
    pub relay_role: Option<String>,
    /// The MAVLink-over-ELRS command-down gate, projected from the lane sidecar's
    /// `fc_command_down_gated`. `Some(true)` a MAVLink-over-ELRS source is
    /// telemetry-only (command path gated closed), `Some(false)` the command path
    /// is open, `None` no such source exists (nothing to gate) — never a
    /// fabricated false. Lets a consumer reading ONLY the crsf lane see the ELRS
    /// MAVLink command path's gate without the top-level FC status surface.
    pub fc_command_down_gated: Option<bool>,
}

impl CrsfBlock {
    /// An all-`None` block: every field reads null. Used as the deserialize
    /// fallback shape and by tests; the payload itself omits the block
    /// entirely (rather than sending this) when there is no fresh sidecar.
    pub fn absent() -> Self {
        CrsfBlock {
            v: None,
            state: None,
            rssi_dbm: None,
            lq_uplink: None,
            lq_downlink: None,
            snr_db: None,
            band: None,
            packet_rate_hz: None,
            tx_power_mw: None,
            tx_frames_per_s: None,
            rx_frames_per_s: None,
            rf_unverified: None,
            mode: None,
            channel_source: None,
            relay_role: None,
            fc_command_down_gated: None,
        }
    }
}
