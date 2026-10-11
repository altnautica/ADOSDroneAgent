//! The `video.wfb` + `video.camera` config slices the config route projects.

use super::*;

// ---------------------------------------------------------------------------
// Config seam: the `video.wfb` + `video.camera` slices, with Pydantic defaults.
// ---------------------------------------------------------------------------

/// The `video.wfb` fields the config route projects. Each field carries the Python
/// `WfbConfig` default so an absent field reads the same value the loaded Python
/// config object would (which fills every field from its model defaults).
#[derive(Debug, Clone, Deserialize)]
pub(super) struct WfbConfigSection {
    #[serde(default = "default_channel")]
    pub(super) channel: i64,
    #[serde(default = "default_band")]
    pub(super) band: String,
    #[serde(default = "default_mcs_index")]
    pub(super) mcs_index: i64,
    #[serde(default = "default_fec_k")]
    pub(super) fec_k: i64,
    #[serde(default = "default_fec_n")]
    pub(super) fec_n: i64,
    #[serde(default = "default_tx_power_dbm")]
    pub(super) tx_power_dbm: i64,
    #[serde(default = "default_wfb_link_preset")]
    pub(super) wfb_link_preset: String,
    #[serde(default = "default_true")]
    pub(super) adaptive_bitrate_enabled: bool,
    #[serde(default = "default_true")]
    pub(super) auto_hop_enabled: bool,
    #[serde(default = "default_hop_period_seconds")]
    pub(super) hop_period_seconds: i64,
}

pub(super) fn default_channel() -> i64 {
    149
}
pub(super) fn default_band() -> String {
    "u-nii-3".to_string()
}
pub(super) fn default_mcs_index() -> i64 {
    1
}
pub(super) fn default_fec_k() -> i64 {
    8
}
pub(super) fn default_fec_n() -> i64 {
    12
}
pub(super) fn default_tx_power_dbm() -> i64 {
    5
}
pub(super) fn default_wfb_link_preset() -> String {
    "conservative".to_string()
}
pub(super) fn default_true() -> bool {
    true
}
pub(super) fn default_hop_period_seconds() -> i64 {
    60
}

impl Default for WfbConfigSection {
    fn default() -> Self {
        Self {
            channel: default_channel(),
            band: default_band(),
            mcs_index: default_mcs_index(),
            fec_k: default_fec_k(),
            fec_n: default_fec_n(),
            tx_power_dbm: default_tx_power_dbm(),
            wfb_link_preset: default_wfb_link_preset(),
            adaptive_bitrate_enabled: default_true(),
            auto_hop_enabled: default_true(),
            hop_period_seconds: default_hop_period_seconds(),
        }
    }
}

/// The `video.camera` fields the encoder block projects, with the Python
/// `CameraConfig` defaults.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct CameraConfigSection {
    #[serde(default = "default_bitrate_kbps")]
    pub(super) bitrate_kbps: i64,
    #[serde(default = "default_width")]
    pub(super) width: i64,
    #[serde(default = "default_height")]
    pub(super) height: i64,
    #[serde(default = "default_fps")]
    pub(super) fps: i64,
    #[serde(default = "default_codec")]
    pub(super) codec: String,
}

pub(super) fn default_bitrate_kbps() -> i64 {
    4000
}
pub(super) fn default_width() -> i64 {
    1280
}
pub(super) fn default_height() -> i64 {
    720
}
pub(super) fn default_fps() -> i64 {
    30
}
pub(super) fn default_codec() -> String {
    "h264".to_string()
}

impl Default for CameraConfigSection {
    fn default() -> Self {
        Self {
            bitrate_kbps: default_bitrate_kbps(),
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            codec: default_codec(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct VideoSection {
    #[serde(default)]
    pub(super) wfb: WfbConfigSection,
    #[serde(default)]
    pub(super) camera: CameraConfigSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct VideoConfig {
    #[serde(default)]
    pub(super) video: VideoSection,
}

impl VideoConfig {
    /// Load the `video.wfb` + `video.camera` slices from the config path
    /// (`ADOS_CONFIG`, default `/etc/ados/config.yaml`). A missing or unparseable
    /// file yields the all-defaults slice, so the route still answers a usable body
    /// with every field at its Python default.
    pub(super) fn load() -> Self {
        let path =
            std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string());
        Self::load_from(Path::new(&path))
    }

    pub(crate) fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_norway::from_str(&text).unwrap_or_default(),
            Err(_) => VideoConfig::default(),
        }
    }

    /// The configured data-plane trio `(mcs_index, fec_k, fec_n)` from
    /// `video.wfb`, each at its default when absent.
    pub(crate) fn data_plane(&self) -> (i64, i64, i64) {
        let wfb = &self.video.wfb;
        (wfb.mcs_index, wfb.fec_k, wfb.fec_n)
    }
}
