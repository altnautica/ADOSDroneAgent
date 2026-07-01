//! WFB service configuration, read from the `video.wfb:` block of
//! `/etc/ados/config.yaml`. Field names and defaults mirror the Python
//! `WfbConfig` dataclass (__main__.py:71, wfb.py:10-105).

use serde::Deserialize;

fn default_channel() -> u8 {
    149
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
fn default_aux_tx_port() -> u16 {
    5602
}
fn default_aux_rx_port() -> u16 {
    5603
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

/// Operator-facing regulatory posture, read from the `network.regulatory:` block
/// of `/etc/ados/config.yaml`. The DEFAULT is unrestricted: a fresh box with no
/// block radiates on the operator's configured channel at hardware-bounded power
/// without requiring a verified operating region. The operator opts INTO a region
/// to re-enable the strict regulatory gate (channel/domain enforcement for that
/// jurisdiction). This is a separate, higher-level switch over the underlying
/// `video.wfb.{reg_gate_strict,reg_domain,dfs_allowed}` knobs — the mode gates
/// them, it does not change their raw defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegulatoryMode {
    /// Radio brings up + TX-enables on the home channel with no verified region.
    /// The operator is responsible for legal RF operation in their location.
    #[default]
    Unrestricted,
    /// An operating region is pinned; the strict regulatory gate is enforced for
    /// that jurisdiction (today's fail-closed behaviour for the pinned region).
    Region,
}

impl RegulatoryMode {
    /// Parse the wire string. Anything other than the explicit `region` token
    /// (case-insensitive) reads as unrestricted, so a fresh / malformed value is
    /// permissive rather than fail-closed (the policy default).
    pub fn from_wire(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "region" => RegulatoryMode::Region,
            _ => RegulatoryMode::Unrestricted,
        }
    }

    /// The wire string persisted in config + surfaced on the heartbeat.
    pub fn as_wire(self) -> &'static str {
        match self {
            RegulatoryMode::Unrestricted => "unrestricted",
            RegulatoryMode::Region => "region",
        }
    }

    /// True when the strict regulatory gate / region enforcement is OFF.
    pub fn is_unrestricted(self) -> bool {
        matches!(self, RegulatoryMode::Unrestricted)
    }
}

/// The operating-region knob the three operator surfaces write. Defaults to
/// unrestricted with no pinned region. `region` is an ISO 3166-1 alpha-2 country
/// code (uppercase) when `mode == Region`, else `None`. `ack_operator` / `ack_at`
/// record who chose the posture and when (audit), and never affect behaviour.
#[derive(Debug, Clone, Default)]
pub struct RegulatoryConfig {
    pub mode: RegulatoryMode,
    pub region: Option<String>,
    pub ack_operator: Option<String>,
    pub ack_at: Option<String>,
}

impl RegulatoryConfig {
    /// Load the `network.regulatory:` block from the agent config file. An absent
    /// file or block reads as the unrestricted default (the fresh-box posture).
    pub fn load_from(path: &std::path::Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            network: NetworkSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct NetworkSection {
            #[serde(default)]
            regulatory: Option<RawRegulatory>,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return RegulatoryConfig::default();
        };
        let raw: RawConfig = ados_config::yaml_or_default(&text, "radio");
        raw.network
            .regulatory
            .map(RawRegulatory::resolve)
            .unwrap_or_default()
    }

    /// The wanted regulatory domain this posture resolves to, given the configured
    /// `reg_domain` fallback (the `video.wfb.reg_domain` default, "US"). Region →
    /// the pinned region code; Unrestricted → the configured fallback (so the
    /// onboard-WiFi global reconciler still has a sane domain to keep, never `00`).
    pub fn wanted_domain<'a>(&'a self, unrestricted_fallback: &'a str) -> &'a str {
        match self.mode {
            RegulatoryMode::Region => self.region.as_deref().unwrap_or(unrestricted_fallback),
            RegulatoryMode::Unrestricted => unrestricted_fallback,
        }
    }
}

/// The raw `network.regulatory` shape as it appears on disk. Resolved into a
/// [`RegulatoryConfig`] so the mode string and region casing are normalised in
/// one place. A `region` mode with no region code degrades to unrestricted (there
/// is no jurisdiction to enforce), which is the safe-permissive direction.
#[derive(Debug, Default, Deserialize)]
struct RawRegulatory {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    ack_operator: Option<String>,
    #[serde(default)]
    ack_at: Option<String>,
}

impl RawRegulatory {
    fn resolve(self) -> RegulatoryConfig {
        let region = self
            .region
            .map(|r| r.trim().to_ascii_uppercase())
            .filter(|r| !r.is_empty());
        let mode = match self.mode.as_deref().map(RegulatoryMode::from_wire) {
            Some(RegulatoryMode::Region) if region.is_some() => RegulatoryMode::Region,
            // region mode with no code, or an absent/unrestricted mode → permissive
            _ => RegulatoryMode::Unrestricted,
        };
        RegulatoryConfig {
            mode,
            region: if mode == RegulatoryMode::Region {
                region
            } else {
                None
            },
            ack_operator: self.ack_operator,
            ack_at: self.ack_at,
        }
    }
}

/// Operator-facing radio link presets, each mapping to the trio of wfb-ng
/// tunables `(mcs_index, fec_k, fec_n)`. Tuned for RTL8812EU radios on a 20 MHz
/// channel. Values are byte-identical to the Python `_LINK_PRESETS` table.
///
/// - `conservative` MCS 1, FEC 8/12 (50% redundancy). The default; robust under
///   low SNR / host-vbus power budgets / a noisy bench.
/// - `balanced` MCS 3, FEC 8/12 (50% redundancy). Headroom for outdoor links
///   where SNR is reliably above ~10 dB.
/// - `aggressive` MCS 5, FEC 8/10 (25% redundancy). Excellent SNR + close-in.
///
/// Returns `None` for an unknown preset name (caller keeps the current config).
pub fn link_preset_trio(preset: &str) -> Option<(u8, u8, u8)> {
    match preset {
        "conservative" => Some((1, 8, 12)),
        "balanced" => Some((3, 8, 12)),
        "aggressive" => Some((5, 8, 10)),
        _ => None,
    }
}

/// Which radio backend drives the WFB link. `kernel` is the
/// Linux monitor-mode + `wfb_tx`/`wfb_rx` backend (the SBC default); `userspace`
/// selects the cross-platform devourer USB backend (built only under the
/// `userspace-usb` feature; future); `auto` (the default) resolves to `kernel` on
/// Linux. Read from `video.wfb.backend`. Parsing is permissive: an unknown value
/// reads as `auto` rather than failing the config load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendChoice {
    /// Force the kernel monitor-mode backend.
    Kernel,
    /// Force the userspace USB (devourer) backend.
    Userspace,
    /// Resolve automatically (kernel on Linux). The default.
    #[default]
    Auto,
}

impl BackendChoice {
    /// Parse the wire string. Anything other than `kernel` / `userspace`
    /// (case-insensitive) reads as `auto`, so a fresh / malformed value is
    /// permissive rather than fail-closed.
    pub fn from_wire(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "kernel" => BackendChoice::Kernel,
            "userspace" => BackendChoice::Userspace,
            _ => BackendChoice::Auto,
        }
    }

    /// The wire string persisted in config + surfaced on the sidecar.
    pub fn as_wire(self) -> &'static str {
        match self {
            BackendChoice::Kernel => "kernel",
            BackendChoice::Userspace => "userspace",
            BackendChoice::Auto => "auto",
        }
    }
}

impl<'de> Deserialize<'de> for BackendChoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(BackendChoice::from_wire(&s))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WfbConfig {
    #[serde(default = "default_channel")]
    pub channel: u8,
    #[serde(default)]
    pub interface: String,
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
    /// safe-by-default invariant). Off by default. When false this is a hard
    /// dead-switch: an `open` request on the radio aux command socket is REFUSED
    /// (no process spawned), so a deployment can forbid the aux pair outright. An
    /// `open` succeeds only when this flag is true AND a caller explicitly opens
    /// the stream.
    #[serde(default)]
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
            wfb_link_preset: default_link_preset(),
            reg_domain: default_reg_domain(),
            reg_gate_strict: default_reg_gate_strict(),
            dfs_allowed: false,
            rendezvous_channel: None,
            auto_channel_enabled: false,
            auto_pair_enabled: true,
            paired_with_device_id: None,
            paired_at: None,
            aux_enable: false,
            aux_tx_port: default_aux_tx_port(),
            aux_rx_port: default_aux_rx_port(),
            aux_fec_k: default_aux_fec_k(),
            aux_fec_n: default_aux_fec_n(),
            aux_mcs_index: None,
            backend: BackendChoice::default(),
        }
    }
}

/// The UDP ports the data, stats, and control planes own on the shared adapter:
/// data ingress 5600, data stats 5601, control tx 5803, control rx 5810. The
/// auxiliary pair must never reuse one of these (it would inject into, or steal
/// frames from, a primary plane), and the aux tx/rx ports must differ from each
/// other. Operators can set the aux ports, so this is enforced at load time.
pub const RESERVED_PLANE_PORTS: [u16; 4] = [5600, 5601, 5803, 5810];

/// Why an aux port configuration is invalid, for a loud load-time log. `None`
/// from [`aux_port_collision`] means the aux ports are safe.
#[derive(Debug, PartialEq, Eq)]
pub enum AuxPortCollision {
    /// The aux tx ingress port reuses a reserved data/control plane port.
    TxReservesPlanePort(u16),
    /// The aux rx re-emit port reuses a reserved data/control plane port.
    RxReservesPlanePort(u16),
    /// The aux tx and rx ports are the same (receive would feed transmit).
    TxRxSame(u16),
}

/// Validate the auxiliary UDP ports against the reserved data/control/stats
/// ports and against each other. Pure (no I/O) so it is unit-testable; returns
/// the first collision found, or `None` when the aux ports are clear.
pub fn aux_port_collision(aux_tx_port: u16, aux_rx_port: u16) -> Option<AuxPortCollision> {
    if aux_tx_port == aux_rx_port {
        return Some(AuxPortCollision::TxRxSame(aux_tx_port));
    }
    if RESERVED_PLANE_PORTS.contains(&aux_tx_port) {
        return Some(AuxPortCollision::TxReservesPlanePort(aux_tx_port));
    }
    if RESERVED_PLANE_PORTS.contains(&aux_rx_port) {
        return Some(AuxPortCollision::RxReservesPlanePort(aux_rx_port));
    }
    None
}

impl WfbConfig {
    /// Load from the `video.wfb:` block in the agent config file.
    pub fn load_from(path: &std::path::Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            video: VideoSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct VideoSection {
            #[serde(default)]
            wfb: WfbConfig,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return WfbConfig::default();
        };
        let raw: RawConfig = ados_config::yaml_or_default(&text, "radio");
        let mut cfg = raw.video.wfb;
        cfg.guard_aux_ports();
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

/// True when the agent profile resolves to `ground_station` — the WFB TX service
/// must idle there (the GS runs `ados-wfb-rx`, not this) so it doesn't clobber
/// the GS's own `wfb-stats.json`. Reads `agent.profile` from the config file,
/// falling back to `profile.conf`. Defensive: the systemd unit is already
/// profile-gated by the supervisor.
pub fn profile_is_ground_station(
    config_path: &std::path::Path,
    profile_conf: &std::path::Path,
) -> bool {
    #[derive(Debug, Default, Deserialize)]
    struct Raw {
        #[serde(default)]
        agent: AgentSection,
    }
    #[derive(Debug, Default, Deserialize)]
    struct AgentSection {
        #[serde(default)]
        profile: Option<String>,
    }
    let cfg_profile = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_norway::from_str::<Raw>(&t).ok())
        .and_then(|r| r.agent.profile);
    match cfg_profile.as_deref() {
        Some("ground_station") | Some("ground-station") => return true,
        Some("drone") => return false,
        _ => {} // empty/auto/missing → consult profile.conf
    }
    if let Ok(text) = std::fs::read_to_string(profile_conf) {
        for line in text.lines() {
            let s = line.trim();
            if let Some(v) = s
                .strip_prefix("profile:")
                .or_else(|| s.strip_prefix("profile="))
            {
                let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
                return matches!(v, "ground_station" | "ground-station");
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_yields_python_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = WfbConfig::load_from(&dir.path().join("nope.yaml"));
        assert_eq!(c.channel, 149);
        assert_eq!(c.band, "u-nii-3");
        assert!(c.auto_hop_enabled);
        assert_eq!(c.hop_period_seconds, 60);
        assert!((c.hop_loss_threshold_percent - 10.0).abs() < 0.01);
        assert!((c.hop_rssi_threshold_dbm - (-75.0)).abs() < 0.01);
        assert_eq!(c.fec_k, 8);
        assert_eq!(c.fec_n, 12);
        // An absent `backend` key resolves to the auto default (kernel on Linux).
        assert_eq!(c.backend, BackendChoice::Auto);
    }

    #[test]
    fn backend_choice_reads_and_defaults_permissively() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file → the auto default.
        let c = WfbConfig::load_from(&dir.path().join("nope.yaml"));
        assert_eq!(c.backend, BackendChoice::Auto);
        // Explicit kernel / userspace parse.
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "video:\n  wfb:\n    backend: kernel\n").unwrap();
        assert_eq!(WfbConfig::load_from(&cfg).backend, BackendChoice::Kernel);
        std::fs::write(&cfg, "video:\n  wfb:\n    backend: userspace\n").unwrap();
        assert_eq!(WfbConfig::load_from(&cfg).backend, BackendChoice::Userspace);
        // An unknown value is permissive → auto, not a config-load failure.
        std::fs::write(&cfg, "video:\n  wfb:\n    backend: bogus\n").unwrap();
        assert_eq!(WfbConfig::load_from(&cfg).backend, BackendChoice::Auto);
        // Case-insensitive.
        assert_eq!(BackendChoice::from_wire("KERNEL"), BackendChoice::Kernel);
        assert_eq!(BackendChoice::from_wire(""), BackendChoice::Auto);
    }

    #[test]
    fn profile_gate_detects_ground_station_and_drone() {
        let dir = tempfile::tempdir().unwrap();
        let none = dir.path().join("nope.yaml");
        let none2 = dir.path().join("nope.conf");
        // Missing everything → not GS (default drone).
        assert!(!profile_is_ground_station(&none, &none2));
        // Explicit GS in config.yaml.
        let gs = dir.path().join("gs.yaml");
        std::fs::write(&gs, "agent:\n  profile: ground_station\n").unwrap();
        assert!(profile_is_ground_station(&gs, &none2));
        // Explicit drone in config.yaml.
        let dr = dir.path().join("dr.yaml");
        std::fs::write(&dr, "agent:\n  profile: drone\n").unwrap();
        assert!(!profile_is_ground_station(&dr, &none2));
        // auto in config.yaml → consult profile.conf (GS).
        let auto = dir.path().join("auto.yaml");
        std::fs::write(&auto, "agent:\n  profile: auto\n").unwrap();
        let pc = dir.path().join("profile.conf");
        std::fs::write(&pc, "profile: ground-station\n").unwrap();
        assert!(profile_is_ground_station(&auto, &pc));
    }

    #[test]
    fn reads_wfb_section() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    channel: 36\n    band: u-nii-1\n    auto_hop_enabled: false\n",
        )
        .unwrap();
        let c = WfbConfig::load_from(&cfg);
        assert_eq!(c.channel, 36);
        assert_eq!(c.band, "u-nii-1");
        assert!(!c.auto_hop_enabled);
        // Unset fields fall back to defaults.
        assert_eq!(c.mcs_index, 1);
        // Pair fields absent → None.
        assert!(c.paired_with_device_id.is_none());
        assert!(c.paired_at.is_none());
    }

    #[test]
    fn reads_pair_block_from_wfb_section() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    paired_with_device_id: ados-58c27faf\n    paired_at: '2026-05-31T10:06:00+00:00'\n    auto_pair_enabled: false\n",
        )
        .unwrap();
        let c = WfbConfig::load_from(&cfg);
        assert_eq!(c.paired_with_device_id.as_deref(), Some("ados-58c27faf"));
        assert_eq!(c.paired_at.as_deref(), Some("2026-05-31T10:06:00+00:00"));
        assert!(!c.auto_pair_enabled);
    }

    #[test]
    fn link_preset_trio_maps_each_named_preset() {
        // The trio must be byte-identical to the Python _LINK_PRESETS table.
        assert_eq!(link_preset_trio("conservative"), Some((1, 8, 12)));
        assert_eq!(link_preset_trio("balanced"), Some((3, 8, 12)));
        assert_eq!(link_preset_trio("aggressive"), Some((5, 8, 10)));
        // An unknown name yields None so the caller keeps the current config.
        assert_eq!(link_preset_trio("turbo"), None);
        assert_eq!(link_preset_trio(""), None);
    }

    #[test]
    fn apply_preset_conservative_is_noop() {
        // conservative respects whatever was explicitly configured, even when
        // those values differ from the conservative trio.
        let mut c = WfbConfig {
            wfb_link_preset: "conservative".to_string(),
            mcs_index: 4,
            fec_k: 6,
            fec_n: 9,
            ..WfbConfig::default()
        };
        c.apply_link_preset();
        assert_eq!(c.mcs_index, 4);
        assert_eq!(c.fec_k, 6);
        assert_eq!(c.fec_n, 9);
    }

    #[test]
    fn apply_preset_balanced_forces_trio() {
        let mut c = WfbConfig {
            wfb_link_preset: "balanced".to_string(),
            mcs_index: 1,
            fec_k: 8,
            fec_n: 12,
            ..WfbConfig::default()
        };
        c.apply_link_preset();
        assert_eq!(c.mcs_index, 3);
        assert_eq!(c.fec_k, 8);
        assert_eq!(c.fec_n, 12);
    }

    #[test]
    fn apply_preset_aggressive_forces_trio() {
        let mut c = WfbConfig {
            wfb_link_preset: "aggressive".to_string(),
            ..WfbConfig::default()
        };
        c.apply_link_preset();
        assert_eq!(c.mcs_index, 5);
        assert_eq!(c.fec_k, 8);
        assert_eq!(c.fec_n, 10);
    }

    #[test]
    fn apply_preset_unknown_keeps_current_values() {
        let mut c = WfbConfig {
            wfb_link_preset: "turbo".to_string(),
            mcs_index: 2,
            fec_k: 7,
            fec_n: 11,
            ..WfbConfig::default()
        };
        c.apply_link_preset();
        // Unknown preset → no override; the configured values stand.
        assert_eq!(c.mcs_index, 2);
        assert_eq!(c.fec_k, 7);
        assert_eq!(c.fec_n, 11);
    }

    #[test]
    fn link_preset_defaults_to_conservative() {
        let c = WfbConfig::default();
        assert_eq!(c.wfb_link_preset, "conservative");
        // The adaptive FEC controller is armed by default.
        assert!(c.adaptive_bitrate_enabled);
    }

    #[test]
    fn reg_gate_raw_flag_default_is_retained_for_region_pin() {
        // The raw `reg_gate_strict` flag default stays `true` so a pinned region
        // is byte-identical to the legacy fail-closed behaviour. The operating
        // mode (RegulatoryConfig) is what gates whether that flag is in force, not
        // a change to this default.
        let c = WfbConfig::default();
        assert!(c.reg_gate_strict);
        // DFS home is off unless explicitly opted in.
        assert!(!c.dfs_allowed);
        // No rendezvous pin → rendezvous == home channel.
        assert!(c.rendezvous_channel.is_none());
        assert_eq!(c.rendezvous_channel(), c.channel);
    }

    #[test]
    fn regulatory_default_is_unrestricted_with_no_region() {
        // The fresh-box operating posture: a default RegulatoryConfig is
        // unrestricted with no pinned region. This is what makes the EFFECTIVE
        // gate permissive even though the raw `reg_gate_strict` flag stays true.
        let r = RegulatoryConfig::default();
        assert_eq!(r.mode, RegulatoryMode::Unrestricted);
        assert!(r.mode.is_unrestricted());
        assert!(r.region.is_none());
        assert_eq!(r.mode.as_wire(), "unrestricted");
    }

    #[test]
    fn regulatory_missing_file_or_block_reads_unrestricted() {
        let dir = tempfile::tempdir().unwrap();
        // No file at all.
        let r = RegulatoryConfig::load_from(&dir.path().join("nope.yaml"));
        assert_eq!(r.mode, RegulatoryMode::Unrestricted);
        assert!(r.region.is_none());
        // A config with no network.regulatory block.
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "video:\n  wfb:\n    channel: 149\n").unwrap();
        let r = RegulatoryConfig::load_from(&cfg);
        assert_eq!(r.mode, RegulatoryMode::Unrestricted);
        assert!(r.region.is_none());
    }

    #[test]
    fn regulatory_region_mode_reads_and_uppercases_region() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "network:\n  regulatory:\n    mode: region\n    region: in\n    ack_operator: ada\n    ack_at: '2026-06-03T00:00:00+05:30'\n",
        )
        .unwrap();
        let r = RegulatoryConfig::load_from(&cfg);
        assert_eq!(r.mode, RegulatoryMode::Region);
        assert_eq!(r.region.as_deref(), Some("IN"));
        assert_eq!(r.ack_operator.as_deref(), Some("ada"));
        assert_eq!(r.ack_at.as_deref(), Some("2026-06-03T00:00:00+05:30"));
    }

    #[test]
    fn regulatory_region_mode_without_code_degrades_to_unrestricted() {
        // A `region` mode with no region code has no jurisdiction to enforce, so
        // it degrades to unrestricted (the safe-permissive direction), never
        // fail-closed.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "network:\n  regulatory:\n    mode: region\n").unwrap();
        let r = RegulatoryConfig::load_from(&cfg);
        assert_eq!(r.mode, RegulatoryMode::Unrestricted);
        assert!(r.region.is_none());
    }

    #[test]
    fn regulatory_explicit_unrestricted_clears_any_region() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "network:\n  regulatory:\n    mode: unrestricted\n    region: US\n",
        )
        .unwrap();
        let r = RegulatoryConfig::load_from(&cfg);
        assert_eq!(r.mode, RegulatoryMode::Unrestricted);
        // The region is irrelevant under unrestricted; it is dropped.
        assert!(r.region.is_none());
    }

    #[test]
    fn regulatory_wanted_domain_resolution() {
        // Region → the pinned region code.
        let region = RegulatoryConfig {
            mode: RegulatoryMode::Region,
            region: Some("DE".to_string()),
            ..RegulatoryConfig::default()
        };
        assert_eq!(region.wanted_domain("US"), "DE");
        // Unrestricted → the configured fallback (so the global reconciler keeps a
        // sane domain, never the world default).
        let unrestricted = RegulatoryConfig::default();
        assert_eq!(unrestricted.wanted_domain("US"), "US");
        assert_eq!(unrestricted.wanted_domain("GB"), "GB");
    }

    #[test]
    fn regulatory_mode_from_wire_is_permissive() {
        assert_eq!(RegulatoryMode::from_wire("region"), RegulatoryMode::Region);
        assert_eq!(RegulatoryMode::from_wire("REGION"), RegulatoryMode::Region);
        assert_eq!(
            RegulatoryMode::from_wire("unrestricted"),
            RegulatoryMode::Unrestricted
        );
        // Anything unrecognised reads as unrestricted (permissive default).
        assert_eq!(RegulatoryMode::from_wire(""), RegulatoryMode::Unrestricted);
        assert_eq!(
            RegulatoryMode::from_wire("garbage"),
            RegulatoryMode::Unrestricted
        );
    }

    #[test]
    fn reg_gate_keys_read_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    channel: 149\n    reg_gate_strict: false\n    dfs_allowed: true\n    rendezvous_channel: 153\n",
        )
        .unwrap();
        let c = WfbConfig::load_from(&cfg);
        assert!(!c.reg_gate_strict);
        assert!(c.dfs_allowed);
        assert_eq!(c.rendezvous_channel, Some(153));
        // The pin overrides the home channel for rendezvous.
        assert_eq!(c.rendezvous_channel(), 153);
    }

    #[test]
    fn reg_gate_keys_absent_fall_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // A config that sets only the home channel: the new keys take defaults.
        std::fs::write(&cfg, "video:\n  wfb:\n    channel: 149\n").unwrap();
        let c = WfbConfig::load_from(&cfg);
        assert!(c.reg_gate_strict);
        assert!(!c.dfs_allowed);
        assert_eq!(c.rendezvous_channel(), 149);
    }

    #[test]
    fn aux_stream_defaults_are_safe_off_and_clear_of_other_planes() {
        // Safe-by-default: the aux allow flag is off, and even when later turned
        // on nothing starts at boot (the process layer enforces that). The aux
        // ports must never collide with the data (5600) or control (5803/5810)
        // ports.
        let c = WfbConfig::default();
        assert!(!c.aux_enable);
        assert_eq!(c.aux_tx_port, 5602);
        assert_eq!(c.aux_rx_port, 5603);
        assert_eq!(c.aux_fec_k, 1);
        assert_eq!(c.aux_fec_n, 2);
        // No explicit aux MCS → the aux pair rides the data-plane rate.
        assert!(c.aux_mcs_index.is_none());
        assert_eq!(c.aux_mcs(), c.mcs_index);
        // The aux ports are distinct from the data/control/stats ports.
        for reserved in [5600u16, 5601, 5803, 5810] {
            assert_ne!(c.aux_tx_port, reserved);
            assert_ne!(c.aux_rx_port, reserved);
        }
    }

    #[test]
    fn aux_port_collision_detects_reserved_and_self_collisions() {
        // The safe default ports are clear.
        assert_eq!(aux_port_collision(5602, 5603), None);
        // An aux tx reusing the data-plane ingress (5600) collides.
        assert_eq!(
            aux_port_collision(5600, 5603),
            Some(AuxPortCollision::TxReservesPlanePort(5600))
        );
        // An aux rx reusing the control rx (5810) collides.
        assert_eq!(
            aux_port_collision(5602, 5810),
            Some(AuxPortCollision::RxReservesPlanePort(5810))
        );
        // Stats (5601) and control tx (5803) are reserved too.
        assert_eq!(
            aux_port_collision(5601, 5603),
            Some(AuxPortCollision::TxReservesPlanePort(5601))
        );
        assert_eq!(
            aux_port_collision(5602, 5803),
            Some(AuxPortCollision::RxReservesPlanePort(5803))
        );
        // Aux tx == aux rx (receive would feed transmit).
        assert_eq!(
            aux_port_collision(5612, 5612),
            Some(AuxPortCollision::TxRxSame(5612))
        );
    }

    #[test]
    fn loading_a_colliding_aux_port_disables_the_aux_stream() {
        // An operator that sets aux_tx_port onto the data plane (5600) must not
        // silently collide: the load-time guard disables the aux stream.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    aux_enable: true\n    aux_tx_port: 5600\n    aux_rx_port: 5603\n",
        )
        .unwrap();
        let c = WfbConfig::load_from(&cfg);
        // The guard fired: the aux stream is disabled despite aux_enable: true.
        assert!(!c.aux_enable);

        // A clear configuration keeps the aux stream enabled.
        let ok = dir.path().join("ok.yaml");
        std::fs::write(
            &ok,
            "video:\n  wfb:\n    aux_enable: true\n    aux_tx_port: 5612\n    aux_rx_port: 5613\n",
        )
        .unwrap();
        let c2 = WfbConfig::load_from(&ok);
        assert!(c2.aux_enable);
        assert_eq!(c2.aux_tx_port, 5612);
        assert_eq!(c2.aux_rx_port, 5613);
    }

    #[test]
    fn aux_stream_keys_read_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    aux_enable: true\n    aux_tx_port: 5612\n    aux_rx_port: 5613\n    aux_fec_k: 4\n    aux_fec_n: 8\n    aux_mcs_index: 3\n",
        )
        .unwrap();
        let c = WfbConfig::load_from(&cfg);
        assert!(c.aux_enable);
        assert_eq!(c.aux_tx_port, 5612);
        assert_eq!(c.aux_rx_port, 5613);
        assert_eq!(c.aux_fec_k, 4);
        assert_eq!(c.aux_fec_n, 8);
        // The explicit aux MCS overrides the data-plane rate.
        assert_eq!(c.aux_mcs_index, Some(3));
        assert_eq!(c.aux_mcs(), 3);
    }
}
