//! The default detector model fetch and the vision config block.

use std::path::Path;

use ados_term::exec;

use crate::steps::venv_agent::venv_python;

/// The path the detector block is written to (the same config the engine reads).
const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The inline Python that PICKS + downloads a detector model for the board. It
/// loads the live config, asks the model manager for the recommended detection
/// model (the registry's `recommended` model, else the first detection model) and
/// its best variant for this board, downloads it, and prints what the (Rust)
/// caller needs to write the `vision.detector` block and the vision backend.
/// Model selection + download is the permanent-Python AI layer; the config write
/// is Rust.
///
/// The pick runs with the `auto` backend and the board string (argv[1]): a
/// `.rknn` variant built for the board's RKNN platform wins, and a board whose
/// platform has no `.rknn` file in the registry takes the ONNX variant on the
/// CPU instead of coming up with no detector.
///
/// Prints `FETCHED\t<runtime>\t<id>\t<path>\t<head>\t<WxH>\t<label,label,...>`,
/// or `DEFERRED` when the registry has no detection model this board can run,
/// or `ERROR ...` on stderr on a real fault.
const FETCH_DETECTOR_PY: &str = r#"
import asyncio, sys
from ados.core.config import load_config
from ados.services.vision.model_manager import ModelManager

async def main():
    board = sys.argv[1] if len(sys.argv) > 1 else ""
    cfg = load_config()
    vision = cfg.vision.model_copy(update={"backend": "auto"})
    mgr = ModelManager(vision, board=board)
    # Populate the registry so the recommended pick can see it (best-effort: an
    # offline board falls back to any locally-cached registry).
    await mgr.fetch_registry()
    model_id = mgr.recommended_model("detection") or ""
    if not model_id:
        print("DEFERRED")
        return
    variant = mgr.select_best_variant(model_id) or {}
    runtime = str(variant.get("runtime") or "")
    if runtime not in ("rknn", "onnx"):
        raise RuntimeError(f"{model_id} resolved to a {runtime or 'runtime-less'} variant")
    path = await mgr.download_model(model_id)
    labels = ",".join(str(c) for c in variant.get("classes") or [])
    fields = [runtime, model_id, str(path), str(variant.get("head") or ""),
              str(variant.get("input_size") or ""), labels]
    # Tab-separated fields; labels are comma-joined (class names carry no
    # commas). The installer parses this to write the vision.detector block.
    print("FETCHED\t" + "\t".join(fields))

try:
    asyncio.run(main())
except Exception as exc:  # surface a clean reason to the installer log
    print("ERROR " + repr(exc), file=sys.stderr)
    sys.exit(2)
"#;

/// The detector the fetch script resolved and downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FetchedDetector {
    /// The variant's runtime, which becomes `vision.backend`: `rknn` | `onnx`.
    pub runtime: String,
    pub model_id: String,
    pub model_path: String,
    /// Output-head layout (`yolo5` | `yolo8` | `yolox`); empty when the registry names none.
    pub head: String,
    /// Input width and height, when the registry names them.
    pub input: Option<(u32, u32)>,
    pub labels: Vec<String>,
}

/// What the fetch script reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum FetchOutcome {
    Fetched(FetchedDetector),
    /// The registry has no detection model this board can run.
    Deferred,
}

/// Parse the fetch script's stdout (its last non-empty line). Pure. Trailing
/// tabs are fields (an empty label list), so only spaces and CRs are trimmed.
pub(super) fn parse_fetch_output(stdout: &str) -> Result<FetchOutcome, String> {
    let line = stdout
        .lines()
        .map(|l| l.trim_matches([' ', '\r']))
        .rfind(|l| !l.is_empty())
        .unwrap_or("");
    if line == "DEFERRED" {
        return Ok(FetchOutcome::Deferred);
    }
    let Some(rest) = line.strip_prefix("FETCHED\t") else {
        return Err(format!("unexpected detector fetch output: {line:?}"));
    };
    let f: Vec<&str> = rest.split('\t').map(str::trim).collect();
    let [runtime, model_id, model_path, head, input, labels] = f[..] else {
        return Err(format!(
            "detector fetch returned {} fields: {line:?}",
            f.len()
        ));
    };
    if !matches!(runtime, "rknn" | "onnx") || model_id.is_empty() || model_path.is_empty() {
        return Err(format!(
            "detector fetch returned an incomplete result: {line:?}"
        ));
    }
    let input = input.split_once('x').and_then(|(w, h)| {
        let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
        (w > 0 && h > 0).then_some((w, h))
    });
    Ok(FetchOutcome::Fetched(FetchedDetector {
        runtime: runtime.to_string(),
        model_id: model_id.to_string(),
        model_path: model_path.to_string(),
        head: head.to_string(),
        input,
        labels: labels
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    }))
}

/// Pick + download a detector model for `board` and WRITE the `vision.detector`
/// block so the engine comes up already detecting (best-effort). Model selection
/// and download are the Python AI layer; parsing the result and writing the
/// config block is Rust. Returns the model runtime of the new detector (`rknn`
/// or `onnx`) when this call wrote the block, and `None` when it wrote nothing: a
/// registry with no detection model is NOT a failure (the runtime + config still
/// provision and the detector resolves once one is available, logged, never a
/// silent half-arm), and an operator-configured detector is never clobbered, so
/// its backend is left as it is. A real fetch error IS surfaced so the install
/// degrades honestly.
pub(super) fn fetch_detector_model(board: &str) -> Result<Option<String>, String> {
    let res = exec::run(&venv_python(), &["-c", FETCH_DETECTOR_PY, board]);
    if !res.spawned {
        return Err(
            "the agent venv Python could not be spawned to fetch the detector model".to_string(),
        );
    }
    if !res.success() {
        return Err(format!(
            "fetching the detector model failed: {}",
            res.stderr.trim()
        ));
    }
    match parse_fetch_output(&res.stdout)? {
        FetchOutcome::Deferred => {
            tracing::info!(
                "no detection model in the registry runs on this board; the inference runtime + \
                 vision config are provisioned and the detector resolves once a model is available"
            );
            Ok(None)
        }
        FetchOutcome::Fetched(det) => match write_detector_block(Path::new(CONFIG_YAML), &det) {
            Ok(true) => {
                tracing::info!(
                    model = %det.model_id,
                    path = %det.model_path,
                    runtime = %det.runtime,
                    classes = det.labels.len(),
                    "detector model fetched and configured"
                );
                Ok(Some(det.runtime))
            }
            Ok(false) => {
                tracing::info!(
                    model = %det.model_id,
                    "detector model fetched; a detector is already configured, leaving it"
                );
                Ok(None)
            }
            Err(e) => Err(format!("writing the vision.detector block failed: {e}")),
        },
    }
}

/// Write the `vision.detector` block (model_id, model_path, head, input size,
/// class_labels) into the agent config, merged surgically so every other key
/// survives (NOT a model_dump-with-defaults rewrite). Returns `Ok(true)` when
/// written, `Ok(false)` when a detector is ALREADY configured (an operator's
/// choice is never clobbered). Mirrors the native `PUT /api/vision/detector`
/// writer's YAML merge. A `.rknn` file carries no self-description, so the head
/// and input size the registry names are written too; absent ones are left to
/// the engine's defaults.
pub(super) fn write_detector_block(
    config_path: &Path,
    det: &FetchedDetector,
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

    let key = |k: &str| Yaml::String(k.to_string());
    detector.insert(key("model_id"), Yaml::String(det.model_id.clone()));
    detector.insert(key("model_path"), Yaml::String(det.model_path.clone()));
    detector.insert(key("enabled"), Yaml::Bool(true));
    if !det.head.is_empty() {
        detector.insert(key("head"), Yaml::String(det.head.clone()));
    }
    if let Some((w, h)) = det.input {
        detector.insert(key("input_width"), Yaml::Number(w.into()));
        detector.insert(key("input_height"), Yaml::Number(h.into()));
    }
    detector.insert(
        key("class_labels"),
        Yaml::Sequence(det.labels.iter().map(|s| Yaml::String(s.clone())).collect()),
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
/// config dict, deep-merges a `vision` override into it, and atomically
/// rewrites the file — so every other field is preserved (NOT a
/// model_dump-with-defaults rewrite that would inject every default). argv[1]
/// is the backend the newly written detector runs on; when it is empty (no
/// detector written now) an existing `vision.backend` is kept and an absent one
/// becomes argv[2], the board's default backend (`rknn` when that is absent
/// too). Prints `OK <backend>` with the backend the file now names.
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

current = raw.get("vision") if isinstance(raw.get("vision"), dict) else {}
default = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else "rknn"
backend = sys.argv[1] if len(sys.argv) > 1 and sys.argv[1] else current.get("backend") or default
merged = deep_merge(raw, {"vision": {"enabled": True, "backend": backend}})

body = yaml.safe_dump(merged, sort_keys=False, default_flow_style=False)
os.makedirs(os.path.dirname(CONFIG), exist_ok=True)
tmp = CONFIG + ".tmp"
with open(tmp, "w") as fh:
    fh.write(body)
os.replace(tmp, CONFIG)
print("OK " + str(backend))
"#;

/// Turn on the vision config (`vision.enabled = true`, and `vision.backend` set
/// to `backend` when a detector was just written, else kept, else
/// `default_backend`; see [`ENABLE_VISION_PY`]) via a real config load → merge
/// → save through the inline writer. Returns the backend the config now names,
/// or an actionable error string on any failure.
pub(super) fn enable_vision_config(
    backend: Option<&str>,
    default_backend: &str,
) -> Result<String, String> {
    let res = exec::run(
        &venv_python(),
        &[
            "-c",
            ENABLE_VISION_PY,
            backend.unwrap_or(""),
            default_backend,
        ],
    );
    if !res.spawned {
        return Err(
            "the agent venv Python could not be spawned to write the vision config".to_string(),
        );
    }
    match res.stdout.trim().strip_prefix("OK ") {
        Some(written) if res.success() => Ok(written.trim().to_string()),
        _ => Err(format!(
            "writing the vision config failed: {}",
            res.stderr.trim()
        )),
    }
}
