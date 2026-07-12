//! The workstation's offload-serving config, read from the agent config
//! (`perception.serving`).
//!
//! The compute daemon auto-serves perception offload by default (`enabled:
//! auto`). This reads the two operator controls off `/etc/ados/config.yaml`:
//! whether to serve at all (the toggle) and which detector model to serve. The
//! `ADOS_COMPUTE_DETECTOR_MODEL` env still wins over the config (the bench
//! override). A resolution to a path that does not exist simply falls back to the
//! mock at load time (Rule 26 — the node still comes up).

use std::path::Path;

use serde::Deserialize;

/// Where the daemon looks for a bare model id (`vision.models_dir`).
const DEFAULT_MODELS_DIR: &str = "/opt/ados/models/vision";

/// The resolved serving config the daemon acts on.
#[derive(Debug, Clone, PartialEq)]
pub struct ServingConfig {
    /// Whether to accept perception-offload sessions (`perception.serving.enabled`
    /// != "off"). Default true (auto-serve).
    pub serve_offload: bool,
    /// A model-path fallback for the served detector, resolved from
    /// `perception.serving.detector_model` (a path, or a bare id under the models
    /// dir). `None` ⇒ the daemon uses the env / the mock.
    pub detector_model_path: Option<String>,
}

impl Default for ServingConfig {
    fn default() -> Self {
        ServingConfig {
            serve_offload: true,
            detector_model_path: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    perception: PerceptionSlice,
    #[serde(default)]
    vision: VisionSlice,
}

#[derive(Debug, Default, Deserialize)]
struct PerceptionSlice {
    #[serde(default)]
    serving: ServingSlice,
}

#[derive(Debug, Default, Deserialize)]
struct ServingSlice {
    #[serde(default)]
    enabled: Option<String>,
    #[serde(default)]
    detector_model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct VisionSlice {
    #[serde(default)]
    models_dir: Option<String>,
}

/// Resolve `detector_model` to a model path: an explicit path (contains `/` or
/// ends `.onnx`) is used verbatim; a bare id resolves to `<models_dir>/<id>.onnx`.
/// Empty / absent ⇒ `None`. Pure (testable).
fn resolve_detector_model(detector_model: Option<&str>, models_dir: &str) -> Option<String> {
    let m = detector_model.map(str::trim).filter(|s| !s.is_empty())?;
    if m.contains('/') || m.ends_with(".onnx") {
        Some(m.to_string())
    } else {
        Some(format!("{}/{m}.onnx", models_dir.trim_end_matches('/')))
    }
}

/// Interpret a parsed config file into the resolved serving config. Pure
/// (testable): the file I/O is in [`load_serving_config`].
fn from_file(file: &ConfigFile) -> ServingConfig {
    let serve_offload = file
        .perception
        .serving
        .enabled
        .as_deref()
        .map(|e| !e.trim().eq_ignore_ascii_case("off"))
        .unwrap_or(true);
    let models_dir = file
        .vision
        .models_dir
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_MODELS_DIR);
    let detector_model_path = resolve_detector_model(
        file.perception.serving.detector_model.as_deref(),
        models_dir,
    );
    ServingConfig {
        serve_offload,
        detector_model_path,
    }
}

/// The agent config path (`ADOS_CONFIG` override, else the default).
fn config_path() -> String {
    std::env::var("ADOS_CONFIG").unwrap_or_else(|_| "/etc/ados/config.yaml".to_string())
}

/// Load + resolve the serving config from `/etc/ados/config.yaml`. A missing /
/// unreadable file yields the defaults (serve, no model override).
pub fn load_serving_config() -> ServingConfig {
    let file: ConfigFile =
        ados_config::load_yaml_or_default(Path::new(&config_path()), "compute-serving");
    from_file(&file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_serve_with_no_model_override() {
        let cfg = from_file(&ConfigFile::default());
        assert!(cfg.serve_offload);
        assert_eq!(cfg.detector_model_path, None);
    }

    #[test]
    fn enabled_off_disables_serving() {
        let file = ConfigFile {
            perception: PerceptionSlice {
                serving: ServingSlice {
                    enabled: Some("off".into()),
                    detector_model: None,
                },
            },
            vision: VisionSlice::default(),
        };
        assert!(!from_file(&file).serve_offload);
        // "on" / "auto" / anything-else all serve.
        for v in ["on", "auto", "ON", " "] {
            let f = ConfigFile {
                perception: PerceptionSlice {
                    serving: ServingSlice {
                        enabled: Some(v.into()),
                        detector_model: None,
                    },
                },
                vision: VisionSlice::default(),
            };
            assert!(from_file(&f).serve_offload, "value {v:?} should serve");
        }
    }

    #[test]
    fn detector_model_resolves_a_bare_id_and_a_path() {
        // A bare id resolves under the models dir.
        assert_eq!(
            resolve_detector_model(Some("coco-yolov8n"), "/opt/ados/models/vision"),
            Some("/opt/ados/models/vision/coco-yolov8n.onnx".into())
        );
        // An explicit path is used verbatim.
        assert_eq!(
            resolve_detector_model(Some("/tmp/custom.onnx"), "/opt/ados/models/vision"),
            Some("/tmp/custom.onnx".into())
        );
        // A bare `.onnx` filename is treated as a path (verbatim).
        assert_eq!(
            resolve_detector_model(Some("my.onnx"), "/models"),
            Some("my.onnx".into())
        );
        // Empty / absent ⇒ None.
        assert_eq!(resolve_detector_model(Some("  "), "/models"), None);
        assert_eq!(resolve_detector_model(None, "/models"), None);
    }
}
