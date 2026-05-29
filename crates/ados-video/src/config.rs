//! Video camera configuration, read from the `video.camera:` block of
//! `/etc/ados/config.yaml`. Field names and defaults mirror the Python
//! `CameraConfig` model (`core/config/video.py`). Only the fields the encoder
//! command builder reads are modelled here.

use serde::Deserialize;

fn default_source() -> String {
    "csi".to_string()
}
fn default_codec() -> String {
    "h264".to_string()
}
fn default_width() -> u32 {
    1280
}
fn default_height() -> u32 {
    720
}
fn default_fps() -> u32 {
    30
}
fn default_bitrate_kbps() -> u32 {
    4000
}
fn default_codec_preference() -> String {
    "auto".to_string()
}

/// Camera capture/encode settings. Mirrors the Python `CameraConfig`.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraConfig {
    /// "csi" | "usb" | "ip" device hint, or a device path / RTSP URL.
    #[serde(default = "default_source")]
    pub source: String,
    /// Wire codec: "h264" (default), "h265", "hevc", or "mjpeg".
    #[serde(default = "default_codec")]
    pub codec: String,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_bitrate_kbps")]
    pub bitrate_kbps: u32,
    /// Operator wire-codec preference: "h264" | "h265" | "auto".
    #[serde(default = "default_codec_preference")]
    pub codec_preference: String,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            source: default_source(),
            codec: default_codec(),
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            bitrate_kbps: default_bitrate_kbps(),
            codec_preference: default_codec_preference(),
        }
    }
}

impl CameraConfig {
    /// Load from the `video.camera:` block in the agent config file. Returns
    /// the defaults when the file is missing or unparseable so config loading
    /// never blocks the pipeline.
    pub fn load_from(path: &std::path::Path) -> Self {
        #[derive(Debug, Default, Deserialize)]
        struct RawConfig {
            #[serde(default)]
            video: VideoSection,
        }
        #[derive(Debug, Default, Deserialize)]
        struct VideoSection {
            #[serde(default)]
            camera: CameraConfig,
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            return CameraConfig::default();
        };
        let raw: RawConfig = serde_norway::from_str(&text).unwrap_or_default();
        raw.video.camera
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let c = CameraConfig::default();
        assert_eq!(c.source, "csi");
        assert_eq!(c.codec, "h264");
        assert_eq!(c.width, 1280);
        assert_eq!(c.height, 720);
        assert_eq!(c.fps, 30);
        assert_eq!(c.bitrate_kbps, 4000);
        assert_eq!(c.codec_preference, "auto");
    }

    #[test]
    fn partial_yaml_fills_defaults() {
        let yaml = "video:\n  camera:\n    width: 1920\n    height: 1080\n    fps: 60\n";
        let raw: serde_norway::Value = serde_norway::from_str(yaml).unwrap();
        // Round-trip through the same nested shape the loader uses.
        let cfg: CameraConfig = {
            #[derive(serde::Deserialize, Default)]
            struct R {
                #[serde(default)]
                video: V,
            }
            #[derive(serde::Deserialize, Default)]
            struct V {
                #[serde(default)]
                camera: CameraConfig,
            }
            let r: R = serde_norway::from_value(raw).unwrap();
            r.video.camera
        };
        assert_eq!(cfg.width, 1920);
        assert_eq!(cfg.height, 1080);
        assert_eq!(cfg.fps, 60);
        // Untouched fields fall back to defaults.
        assert_eq!(cfg.codec, "h264");
        assert_eq!(cfg.bitrate_kbps, 4000);
    }
}
