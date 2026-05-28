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
    #[serde(default)]
    pub adaptive_bitrate_enabled: bool,
    #[serde(default)]
    pub reg_domain: Option<String>,
    #[serde(default)]
    pub auto_channel_enabled: bool,
    #[serde(default = "default_true")]
    pub auto_pair_enabled: bool,
}

impl Default for WfbConfig {
    fn default() -> Self {
        Self {
            channel: default_channel(),
            interface: String::new(),
            band: default_band(),
            auto_hop_enabled: true,
            hop_period_seconds: default_hop_period(),
            hop_loss_threshold_percent: default_hop_loss_threshold(),
            hop_rssi_threshold_dbm: default_hop_rssi_threshold(),
            mcs_index: default_mcs_index(),
            fec_k: default_fec_k(),
            fec_n: default_fec_n(),
            tx_power_dbm: default_tx_power_dbm(),
            tx_power_max_dbm: default_tx_power_max_dbm(),
            topology: default_topology(),
            adaptive_bitrate_enabled: false,
            reg_domain: None,
            auto_channel_enabled: false,
            auto_pair_enabled: true,
        }
    }
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
        let raw: RawConfig = serde_norway::from_str(&text).unwrap_or_default();
        raw.video.wfb
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
    }
}
