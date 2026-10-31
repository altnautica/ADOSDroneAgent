//! The default detector model fetch and the vision config block.

use std::path::Path;

use ados_term::exec;

use crate::steps::venv_agent::venv_python;

/// The path the detector block is written to (the same config the engine reads).
const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The inline Python that PICKS + downloads a detector model for the board. It
/// loads the live config, asks the model manager for the recommended detection
/// model (the registry's `recommended` model, else the first detection model),
/// downloads the best variant for this board's NPU TOPS, and prints the resolved
/// id + path + class labels so the (Rust) caller can write the `vision.detector`
/// block. Model selection + download is the permanent-Python AI layer; the config
/// write is Rust. Prints `FETCHED\t<id>\t<path>\t<label,label,...>`, or `DEFERRED`
/// when the registry has no detection model, or `ERROR ...` on a real fault.
const FETCH_DETECTOR_PY: &str = r#"
import asyncio, sys
from ados.core.config import load_config
from ados.services.vision.model_manager import ModelManager

async def main():
    cfg = load_config()
    vision = cfg.vision
    mgr = ModelManager(vision)
    # Populate the registry so the recommended pick can see it (best-effort: an
    # offline board falls back to any locally-cached registry).
    await mgr.fetch_registry()
    model_id = mgr.recommended_detector() or ""
    if not model_id:
        print("DEFERRED")
        return
    path = await mgr.download_model(model_id)
    variant = mgr.select_best_variant(model_id) or {}
    classes = variant.get("classes") or []
    labels = ",".join(str(c) for c in classes)
    # Tab-separated top-level fields; labels are comma-joined (class names carry
    # no commas). The installer parses this to write the vision.detector block.
    print("FETCHED\t" + model_id + "\t" + str(path) + "\t" + labels)

try:
    asyncio.run(main())
except Exception as exc:  # surface a clean reason to the installer log
    print("ERROR " + repr(exc), file=sys.stderr)
    sys.exit(2)
"#;

/// Pick + download a detector model and WRITE the `vision.detector` block so the
/// engine comes up already detecting (best-effort). Model selection + download is
/// the Python AI layer; parsing the result and writing the config block is Rust.
/// A registry with no detection model is NOT a failure — the runtime + config
/// still provision and the detector resolves once one is available (logged, never
/// a silent half-arm). A real fetch error IS surfaced so the install degrades
/// honestly. An operator-configured detector is never clobbered.
pub(super) fn fetch_detector_model() -> Result<(), String> {
    let res = exec::run(&venv_python(), &["-c", FETCH_DETECTOR_PY]);
    if !res.spawned {
        return Err(
            "the agent venv Python could not be spawned to fetch the detector model".to_string(),
        );
    }
    let out = res.stdout.trim();
    if let Some(rest) = out.strip_prefix("FETCHED\t") {
        // <model_id>\t<path>\t<labels-csv>
        let mut it = rest.splitn(3, '\t');
        let model_id = it.next().unwrap_or("").trim();
        let model_path = it.next().unwrap_or("").trim();
        let labels: Vec<String> = it
            .next()
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if model_id.is_empty() || model_path.is_empty() {
            return Err(format!(
                "detector fetch returned an incomplete result: {out}"
            ));
        }
        match write_detector_block(Path::new(CONFIG_YAML), model_id, model_path, &labels) {
            Ok(true) => {
                tracing::info!(
                    model = model_id,
                    path = model_path,
                    classes = labels.len(),
                    "detector model fetched and configured"
                )
            }
            Ok(false) => {
                tracing::info!(
                    model = model_id,
                    "detector model fetched; a detector is already configured, leaving it"
                )
            }
            Err(e) => return Err(format!("writing the vision.detector block failed: {e}")),
        }
        Ok(())
    } else if out == "DEFERRED" {
        tracing::info!(
            "no detection model in the registry; the NPU runtime + vision config are provisioned \
             and the detector resolves once a model is available"
        );
        Ok(())
    } else {
        Err(format!(
            "fetching the detector model failed: {}",
            res.stderr.trim()
        ))
    }
}

/// Write the `vision.detector` block (model_id + model_path + class_labels) into
/// the agent config, merged surgically so every other key survives (NOT a
/// model_dump-with-defaults rewrite). Returns `Ok(true)` when written, `Ok(false)`
/// when a detector is ALREADY configured (an operator's choice is never
/// clobbered). Mirrors the native `PUT /api/vision/detector` writer's YAML merge.
pub(super) fn write_detector_block(
    config_path: &Path,
    model_id: &str,
    model_path: &str,
    class_labels: &[String],
) -> Result<bool, String> {
    use serde_norway::{Mapping, Value as Yaml};

    let mut data: Yaml = match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(Mapping::new()),
        },
        Err(_) => Yaml::Mapping(Mapping::new()),
    };

    let root = data
        .as_mapping_mut()
        .ok_or_else(|| "config root is not a mapping".to_string())?;

    if detector_already_configured(root) {
        return Ok(false);
    }

    // vision:
    let vision = root
        .entry(Yaml::String("vision".to_string()))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    let vision = vision
        .as_mapping_mut()
        .ok_or_else(|| "vision is not a mapping".to_string())?;
    // vision.detector:
    let detector = vision
        .entry(Yaml::String("detector".to_string()))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    let detector = detector
        .as_mapping_mut()
        .ok_or_else(|| "vision.detector is not a mapping".to_string())?;

    detector.insert(
        Yaml::String("model_id".to_string()),
        Yaml::String(model_id.to_string()),
    );
    detector.insert(
        Yaml::String("model_path".to_string()),
        Yaml::String(model_path.to_string()),
    );
    detector.insert(Yaml::String("enabled".to_string()), Yaml::Bool(true));
    detector.insert(
        Yaml::String("class_labels".to_string()),
        Yaml::Sequence(
            class_labels
                .iter()
                .map(|s| Yaml::String(s.clone()))
                .collect(),
        ),
    );

    let body = serde_norway::to_string(&data).map_err(|e| e.to_string())?;
    write_config_atomic(config_path, body.as_bytes())?;
    Ok(true)
}

/// Whether the config already carries a non-empty `vision.detector.model_id` (an
/// operator's or a prior install's choice we must not overwrite).
fn detector_already_configured(root: &serde_norway::Mapping) -> bool {
    root.get("vision")
        .and_then(|v| v.as_mapping())
        .and_then(|v| v.get("detector"))
        .and_then(|d| d.as_mapping())
        .and_then(|d| d.get("model_id"))
        .and_then(|m| m.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Atomically write `body` to `path` (temp file + rename), creating the parent.
fn write_config_atomic(path: &Path, body: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, body).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("placing config at {}: {e}", path.display())
    })
}

/// The inline Python that turns on the vision config. It loads the on-disk
/// config dict, deep-merges a `vision` override (enabled + rknn backend) into
/// it, and atomically rewrites the file — so every other field is preserved
/// (NOT a model_dump-with-defaults rewrite that would inject every default).
const ENABLE_VISION_PY: &str = r#"
import os, sys, yaml

CONFIG = "/etc/ados/config.yaml"

def deep_merge(base, override):
    out = dict(base)
    for k, v in override.items():
        if k in out and isinstance(out[k], dict) and isinstance(v, dict):
            out[k] = deep_merge(out[k], v)
        else:
            out[k] = v
    return out

raw = {}
if os.path.isfile(CONFIG):
    with open(CONFIG) as fh:
        loaded = yaml.safe_load(fh)
    if isinstance(loaded, dict):
        raw = loaded

merged = deep_merge(raw, {"vision": {"enabled": True, "backend": "rknn"}})

body = yaml.safe_dump(merged, sort_keys=False, default_flow_style=False)
os.makedirs(os.path.dirname(CONFIG), exist_ok=True)
tmp = CONFIG + ".tmp"
with open(tmp, "w") as fh:
    fh.write(body)
os.replace(tmp, CONFIG)
print("OK")
"#;

/// Turn on the vision config (`vision.enabled = true`, `vision.backend = rknn`)
/// via a real config load → merge → save through the inline writer. Returns an
/// actionable error string on any failure.
pub(super) fn enable_vision_config() -> Result<(), String> {
    let res = exec::run(&venv_python(), &["-c", ENABLE_VISION_PY]);
    if !res.spawned {
        return Err(
            "the agent venv Python could not be spawned to write the vision config".to_string(),
        );
    }
    if res.success() && res.stdout.trim() == "OK" {
        Ok(())
    } else {
        Err(format!(
            "writing the vision config failed: {}",
            res.stderr.trim()
        ))
    }
}
