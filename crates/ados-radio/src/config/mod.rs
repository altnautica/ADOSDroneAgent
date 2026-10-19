//! WFB service configuration, read from the `video.wfb:` block of
//! `/etc/ados/config.yaml`. Field names and defaults mirror the Python
//! `WfbConfig` dataclass (__main__.py:71, wfb.py:10-105).

use serde::{Deserialize, Serialize};

mod adapters;
mod fleet;
mod link;
mod profile;
mod regulatory;

pub use adapters::{
    AdapterEntry, AdapterMatch, AdapterRole, RadioAdaptersConfig, RADIO_CONFIG_POINTER,
};
pub use fleet::*;
pub use link::{link_preset_trio, BackendChoice};
pub use profile::{ground_station_from_config_text, profile_is_ground_station, PROFILE_CONF};
pub use regulatory::{RegulatoryConfig, RegulatoryMode};

fn default_channel() -> u8 {
    149
}
/// Default fleet. A single-drone rig never has to think about fleets, so the
/// unconfigured node lands in fleet 1 rather than the reserved 0.
fn default_fleet_id() -> u16 {
    1
}
fn default_band() -> String {
    "u-nii-3".to_string()
}
// Regulatory domain applied (iw reg set) before monitor-mode bring-up.
// Permits the home channel (149 / 5745 MHz, U-NII-3, non-DFS) at usable TX
// power; the kernel's startup domain can otherwise forbid 5745 and cap TX
// to the -100 dBm "not permitted" sentinel. Operators override per region.
fn default_reg_domain() -> Option<String> {
    Some("US".to_string())
}
fn default_hop_period() -> u32 {
    60
}
fn default_hop_loss_threshold() -> f32 {
    10.0
}
fn default_hop_rssi_threshold() -> f32 {
    -75.0
}
fn default_mcs_index() -> u8 {
    1
}
/// Default cap on the adaptive MCS ladder's top rung. Mirrors
/// [`crate::mcs_ladder::DEFAULT_LADDER_MAX_MCS`]; kept as a `serde` default fn so
/// an absent YAML key resolves the same way an absent field does in `Default`.
fn default_adaptive_mcs_max() -> u8 {
    crate::mcs_ladder::DEFAULT_LADDER_MAX_MCS
}
fn default_fec_k() -> u8 {
    8
}
fn default_fec_n() -> u8 {
    12
}
fn default_tx_power_dbm() -> i8 {
    5
}
fn default_tx_power_max_dbm() -> i8 {
    15
}
fn default_topology() -> String {
    "host_vbus".to_string()
}
fn default_true() -> bool {
    true
}
fn default_link_preset() -> String {
    "conservative".to_string()
}
// Fail-closed regulatory gate: when the wanted domain cannot be verified the
// radio refuses to bring up monitor mode / set a channel rather than running on
// a band the active domain forbids (the silent power-cap class). An operator
// with an EEPROM-locked dongle in a lab can set this false to revert to the old
// best-effort behaviour.
fn default_reg_gate_strict() -> bool {
    true
}
// Auxiliary stream radio-ports. The data plane owns radio_id 0 (UDP 5600), the
// control plane radio_id 1 (UDP 5803 tx / 5810 rx). The auxiliary pair takes
// radio_id 2/3 on UDP 5602 (tx ingress) / 5603 (rx re-emit) so it can never
// collide with the data or control planes on the shared adapter.
fn default_aux_enable() -> bool {
    true
}
// Delegated to the shared reader every consumer of the lane resolves through,
// so the radio and the processes that feed it cannot drift apart on a default.
fn default_aux_tx_port() -> u16 {
    ados_protocol::aux_ports::DEFAULT_AUX_TX_PORT
}
fn default_aux_rx_port() -> u16 {
    ados_protocol::aux_ports::DEFAULT_AUX_RX_PORT
}
// The auxiliary channel is a low-rate application pipe between nodes, so it
// defaults to the lightest valid Reed-Solomon ratio (k=1, n=2) — the same trio
// the control plane uses — rather than the heavier video FEC.
fn default_aux_fec_k() -> u8 {
    1
}
fn default_aux_fec_n() -> u8 {
    2
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WfbConfig {
    #[serde(default = "default_channel")]
    pub channel: u8,
    #[serde(default)]
    pub interface: String,
    /// Fleet this node belongs to. Every node that must hear every other node
    /// shares it. Range 1..=65535; 0 is reserved as "unprovisioned" and is
    /// rejected at load time. Combined with [`fleet_slot`](Self::fleet_slot)
    /// through [`link_id`] it fills the wfb-ng 24-bit `link_id` space exactly,
    /// so two fleets can share one channel with no `channel_id` collision.
    #[serde(default = "default_fleet_id")]
    pub fleet_id: u16,
    /// This node's slot within the fleet. [`SLOT_GROUND`] (0) is the ground
    /// station; 1..=[`FLEET_MAX_SLOTS`] are drones. Every drone's slot MUST be
    /// unique within a fleet: two transmitters sharing a `channel_id` thrash
    /// each other's FEC decoder roughly once a second (the wfb-ng `Aggregator`
    /// re-inits FEC on any foreign session packet), which presents as
    /// unexplained link loss. Slots are issued by the ground station's fleet
    /// registry at pair time, never negotiated at runtime.
    #[serde(default)]
    pub fleet_slot: u8,
    #[serde(default = "default_band")]
    pub band: String,
    #[serde(default = "default_true")]
    pub auto_hop_enabled: bool,
    /// Explicit opt-in for the time-based periodic channel hop. Off by default:
    /// the periodic scan locks the radio and drops `wfb_tx` frames, and a
    /// coordinated time-based hop needs the GS to follow the announce — proven on
    /// a two-node rig before it is trusted in the field. The reactive
    /// (link-degradation) hop + the GS-coordinated follow stay enabled regardless;
    /// only the unattended periodic-execution path is gated here.
    #[serde(default)]
    pub periodic_hop_enabled: bool,
    #[serde(default = "default_hop_period")]
    pub hop_period_seconds: u32,
    #[serde(default = "default_hop_loss_threshold")]
    pub hop_loss_threshold_percent: f32,
    #[serde(default = "default_hop_rssi_threshold")]
    pub hop_rssi_threshold_dbm: f32,
    #[serde(default = "default_mcs_index")]
    pub mcs_index: u8,
    #[serde(default = "default_fec_k")]
    pub fec_k: u8,
    #[serde(default = "default_fec_n")]
    pub fec_n: u8,
    #[serde(default = "default_tx_power_dbm")]
    pub tx_power_dbm: i8,
    #[serde(default = "default_tx_power_max_dbm")]
    pub tx_power_max_dbm: i8,
    #[serde(default = "default_topology")]
    pub topology: String,
    /// Closed-loop FEC controller. Default ON: on a link with received-side
    /// stats it steps the Reed-Solomon ratio up under loss / weak RSSI and back
    /// down on a clean window. A drone-only rig with no peer stats holds the
    /// rung (the cold-start guard), so the default is safe before a ground
    /// station is present. Operators pin a manual trio to disable it.
    #[serde(default = "default_true")]
    pub adaptive_bitrate_enabled: bool,
    /// Cap on the top rung the adaptive SNR/MCS ladder may select.
    ///
    /// Defaults to 3, not the hard ceiling of 5. The rung table in
    /// [`crate::mcs_ladder`] is arithmetic from standard 802.11n required-SNR
    /// figures, not a measurement of the vendored `rtl8812eu`; OpenIPC's
    /// `adaptive-link` — the working production reference for this loop — ships a
    /// profile table that tops out at MCS 2 in the field. Anything above 3 is
    /// bench-only until an MCS characterisation sweep
    /// measures which rungs this driver actually applies and holds. Clamped into
    /// `1..=5` on read, so a typo can never radiate an unmeasured rung.
    #[serde(default = "default_adaptive_mcs_max")]
    pub adaptive_mcs_max: u8,
    /// Operator-facing link preset; overrides `mcs_index`/`fec_k`/`fec_n` via
    /// [`WfbConfig::apply_link_preset`]. `conservative` (the default) is a no-op
    /// so a rig with explicitly-tuned values keeps them.
    #[serde(default = "default_link_preset")]
    pub wfb_link_preset: String,
    #[serde(default = "default_reg_domain")]
    pub reg_domain: Option<String>,
    /// Fail-closed regulatory gate. When true (the default) the radio refuses to
    /// bring up monitor mode / set a channel until the wanted domain verifies; on
    /// failure it parks in the `reg_blocked` state with bounded retry. False
    /// reverts to legacy best-effort (an EEPROM-locked lab dongle escape hatch).
    #[serde(default = "default_reg_gate_strict")]
    pub reg_gate_strict: bool,
    /// Permit DFS / radar channels as the rendezvous home. Off by default — a DFS
    /// home needs a channel-availability check the link does not perform, so the
    /// gate refuses a DFS rendezvous channel unless this is set.
    #[serde(default)]
    pub dfs_allowed: bool,
    /// Optional rendezvous channel pin, distinct from the operator's home
    /// `channel`. `None` falls back to `channel`. Lets an operator meet on a
    /// channel other than the home when ever needed; never auto-written.
    #[serde(default)]
    pub rendezvous_channel: Option<u8>,
    #[serde(default)]
    pub auto_channel_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_pair_enabled: bool,
    /// Persisted peer device-id from the last pair (the back-fill target). None
    /// until a pair completes; surfaced in `wfb-stats.json` so the panel shows
    /// pair identity from the drone side without the cloud relay.
    #[serde(default)]
    pub paired_with_device_id: Option<String>,
    /// ISO timestamp of the last pair, persisted under `video.wfb`. None when
    /// never paired.
    #[serde(default)]
    pub paired_at: Option<String>,
    /// Whether the auxiliary application stream is permitted to start. This is
    /// the config-level allow flag, NOT a boot-time spawn: even with this true
    /// the aux pair stays down until something explicitly opens it (the
    /// safe-by-default invariant). ON by default, because the aux lane is how a
    /// linked ground station receives everything that is neither video nor a
    /// tiny control frame, and a node that cannot carry its own capability over
    /// the radio is not much use in the field. Nothing is spawned and no frame
    /// is radiated until a consumer explicitly opens the stream, so permitting
    /// it costs nothing on a deployment that never uses it. Set false for a hard
    /// dead-switch: an `open` on the radio aux command socket is then REFUSED
    /// with no process spawned, so a deployment can still forbid the pair
    /// outright. `guard_aux_ports` also forces this false on a port collision.
    #[serde(default = "default_aux_enable")]
    pub aux_enable: bool,
    /// UDP port the auxiliary tx ingress reads application frames from
    /// (radio_id 2). Defaults clear of the data/control ports.
    #[serde(default = "default_aux_tx_port")]
    pub aux_tx_port: u16,
    /// UDP port the auxiliary rx re-emits decoded application frames onto
    /// (radio_id 3, 127.0.0.1). Defaults clear of the data/control ports.
    #[serde(default = "default_aux_rx_port")]
    pub aux_rx_port: u16,
    /// Auxiliary stream Reed-Solomon data-shard count (k). Defaults to the light
    /// control-plane ratio; the aux channel is a low-rate pipe, not video.
    #[serde(default = "default_aux_fec_k")]
    pub aux_fec_k: u8,
    /// Auxiliary stream Reed-Solomon total-shard count (n). Defaults to the
    /// light control-plane ratio (k=1, n=2).
    #[serde(default = "default_aux_fec_n")]
    pub aux_fec_n: u8,
    /// Optional MCS index for the auxiliary stream. `None` reuses the data-plane
    /// `mcs_index`, so the aux pair rides the same modulation rate by default.
    #[serde(default)]
    pub aux_mcs_index: Option<u8>,
    /// Which radio backend drives the WFB link. `auto` (the
    /// default) resolves to the kernel monitor backend on Linux. Read from
    /// `video.wfb.backend`.
    #[serde(default)]
    pub backend: BackendChoice,
}

impl Default for WfbConfig {
    fn default() -> Self {
        Self {
            channel: default_channel(),
            interface: String::new(),
            fleet_id: default_fleet_id(),
            fleet_slot: SLOT_GROUND,
            band: default_band(),
            auto_hop_enabled: true,
            periodic_hop_enabled: false,
            hop_period_seconds: default_hop_period(),
            hop_loss_threshold_percent: default_hop_loss_threshold(),
            hop_rssi_threshold_dbm: default_hop_rssi_threshold(),
            mcs_index: default_mcs_index(),
            fec_k: default_fec_k(),
            fec_n: default_fec_n(),
            tx_power_dbm: default_tx_power_dbm(),
            tx_power_max_dbm: default_tx_power_max_dbm(),
            topology: default_topology(),
            adaptive_bitrate_enabled: true,
            adaptive_mcs_max: default_adaptive_mcs_max(),
            wfb_link_preset: default_link_preset(),
            reg_domain: default_reg_domain(),
            reg_gate_strict: default_reg_gate_strict(),
            dfs_allowed: false,
            rendezvous_channel: None,
            auto_channel_enabled: false,
            auto_pair_enabled: true,
            paired_with_device_id: None,
            paired_at: None,
            aux_enable: default_aux_enable(),
            aux_tx_port: default_aux_tx_port(),
            aux_rx_port: default_aux_rx_port(),
            aux_fec_k: default_aux_fec_k(),
            aux_fec_n: default_aux_fec_n(),
            aux_mcs_index: None,
            backend: BackendChoice::default(),
        }
    }
}

/// JSON pointer of the `video.wfb` slice in the agent config.
pub const CONFIG_POINTER: &str = "/video/wfb";

impl WfbConfig {
    /// Load from the `video.wfb:` block in the agent config file.
    ///
    /// The slice is read from the effective config (schema defaults overlaid
    /// with the file), so an absent key is the schema default. An absent file
    /// is the defaults; a malformed file or slice falls back to the defaults
    /// with the error published.
    pub fn load_from(path: &std::path::Path) -> Self {
        if !path.exists() {
            return WfbConfig::default();
        }
        let (mut cfg, cfg_err) =
            match ados_config::schema::load_slice::<WfbConfig>(path, CONFIG_POINTER) {
                Ok(cfg) => (cfg, None),
                Err(e) => (WfbConfig::default(), Some(e)),
            };
        // Publish the parse result so a malformed config surfaces on the fleet
        // Health view, not just in the log (per-service status sidecar). This is
        // the radio service's single startup config load; the regulatory reader
        // stays on the quiet-default helper (it also runs during radio bring-up).
        ados_config::write_config_status("radio", cfg_err.as_deref());
        cfg.guard_aux_ports();
        cfg.guard_tx_power();
        cfg
    }

    /// Reject a colliding auxiliary-port configuration at load time: a settable
    /// aux port that reuses a reserved data/control/stats port (or an aux tx that
    /// equals the aux rx) would silently corrupt a primary plane, so the aux
    /// stream is disabled and the collision is logged loudly. Idempotent and safe
    /// to call on every load. A clear configuration is left untouched.
    pub fn guard_aux_ports(&mut self) {
        if let Some(collision) = aux_port_collision(self.aux_tx_port, self.aux_rx_port) {
            tracing::error!(
                aux_tx_port = self.aux_tx_port,
                aux_rx_port = self.aux_rx_port,
                ?collision,
                "aux_port_collision: auxiliary stream disabled (port collides with a data/control plane or itself)"
            );
            self.aux_enable = false;
        }
    }

    /// Hold the boot TX power inside `1..=tx_power_max_dbm`. A hand-edited power
    /// above the ceiling would bring the adapter up past the budget the ceiling
    /// exists to enforce (host-VBUS brownout, regulatory limit), so it is pulled
    /// down to the ceiling and logged. Idempotent.
    pub fn guard_tx_power(&mut self) {
        let max = self.tx_power_max_dbm.max(1);
        let bounded = self.tx_power_dbm.clamp(1, max);
        if bounded != self.tx_power_dbm {
            tracing::warn!(
                tx_power_dbm = self.tx_power_dbm,
                tx_power_max_dbm = self.tx_power_max_dbm,
                applied_dbm = bounded,
                "tx_power_out_of_budget: boot power held inside the configured ceiling"
            );
            self.tx_power_dbm = bounded;
        }
    }

    /// The rendezvous (meeting) channel: the optional `rendezvous_channel` pin
    /// when set, else the operator's home `channel`. Both rigs derive it the same
    /// way from the same config, so a fresh drone and a fresh ground station meet
    /// with zero search.
    pub fn rendezvous_channel(&self) -> u8 {
        self.rendezvous_channel.unwrap_or(self.channel)
    }

    /// The MCS index the auxiliary stream uses: the explicit `aux_mcs_index`
    /// override when set, else the data-plane `mcs_index` so the aux pair rides
    /// the same modulation rate by default.
    pub fn aux_mcs(&self) -> u8 {
        self.aux_mcs_index.unwrap_or(self.mcs_index)
    }

    /// Override `mcs_index`/`fec_k`/`fec_n` from `wfb_link_preset`.
    ///
    /// The default `conservative` leaves the explicit config values alone, which
    /// lets a rig with hand-tuned values keep them untouched. Any other known
    /// preset forces the trio so the operator can widen the link by changing one
    /// field instead of three. An unknown preset is a no-op (the current values
    /// stand). Byte-identical behaviour to the Python `_apply_link_preset`.
    pub fn apply_link_preset(&mut self) {
        if self.wfb_link_preset == "conservative" {
            // Respect the explicit config; do not override.
            return;
        }
        let Some((mcs, fec_k, fec_n)) = link_preset_trio(&self.wfb_link_preset) else {
            tracing::warn!(
                preset = %self.wfb_link_preset,
                note = "unknown link preset; keeping current config values",
                "wfb_link_preset_unknown"
            );
            return;
        };
        self.mcs_index = mcs;
        self.fec_k = fec_k;
        self.fec_n = fec_n;
        tracing::info!(
            preset = %self.wfb_link_preset,
            mcs_index = mcs,
            fec_k,
            fec_n,
            "wfb_link_preset_applied"
        );
    }
}

#[cfg(test)]
mod tests;
