//! Ground-station WFB radio-config write route.
//!
//! The ground-station profile exposes its stored radio knobs at
//! `PUT /api/v1/ground-station/wfb`. The read view lives in
//! [`crate::routes::gs_status::get_wfb`]; this module serves the one write the
//! front can reproduce faithfully.
//!
//! ## What ports here, and what does not
//!
//! Only `PUT .../wfb` ports. Its whole effect is a config-file persist of three
//! fields under `video.wfb` (`channel`, `bitrate_profile`, `fec`), exactly the
//! same surgical `video.wfb` field merge the sibling
//! [`crate::routes::wfb_write`] tx-power route already performs. There is no
//! in-process manager to drive and no live service to race: the radio + ground
//! services read their channel/profile/fec from the same config file on their
//! own cadence, so the front merging those keys is wire-equivalent to the
//! FastAPI route persisting them.
//!
//! The sibling `POST/DELETE .../wfb/pair` routes do NOT port. Each drives the
//! in-process Python `PairManager`, which writes the 64-byte `rx.key`/`tx.key`
//! file, persists the pair-state config, drops the setup-complete sentinel, and
//! restarts the wfb systemd unit — multi-step orchestration with no native
//! command-socket op to forward to (the radio command sockets serve only
//! `hop`/`status` and `set_*` knobs; the groundlink receive plane only reads the
//! key, it has no apply/unpair seam). Those stay on the residual surface and the
//! proxy forwards them unchanged.
//!
//! ## Persist faithfulness vs the FastAPI route
//!
//! The FastAPI handler mutates its in-memory Pydantic config model and persists
//! the whole materialized model (`save_config()` writes `config.model_dump()`).
//! The front holds no Pydantic model, so it surgically merges the three fields
//! into the on-disk YAML instead — the same approach the tx-power route takes
//! for `video.wfb.tx_power_dbm`. The on-disk shape differs (sparse merge vs a
//! default-filled model dump) but both are valid configs that resolve to the
//! same `video.wfb` values, so the read-back is identical. Crucially the route's
//! RESPONSE body never echoes the file: it echoes the three live fields plus the
//! `persisted` flag, both reproducible from the request + the merge result.
//!
//! ## Response shape (matched to the FastAPI route)
//!
//! The FastAPI handler reads `_read_wfb_view(app)` off the *mutated* in-memory
//! config and adds `persisted`. The view reads `video.wfb.{channel,
//! bitrate_profile, fec}` with the Python field defaults (`0` / `"default"` /
//! `"8/12"`). Because the mutation only sets a field when the request carries a
//! non-null value, the response value for a field is: the request value when
//! present, else the on-disk value, else the default. The front reproduces that
//! exactly by reading the merged on-disk config back. On a persist success the
//! body is `{channel, bitrate_profile, fec, persisted: true}`; on a persist
//! failure it is the same three fields with `persisted: false` plus a
//! `persist_error` string. A non-root front (which cannot write the 0600
//! root-owned config) lands on the same `persisted: false` + `persist_error`
//! path, matching the FastAPI handler's `save_config()` returning `False`.
//!
//! ## The profile gate
//!
//! Like every ground-station route, this first gates on the resolved profile
//! being a ground station and returns the FastAPI
//! `404 {"detail":{"error":{"code":"E_PROFILE_MISMATCH"}}}` on a drone, the same
//! body the read module serves.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Profile gate (mirrors the read module + the Python `_require_ground_profile`).
// ---------------------------------------------------------------------------

/// The FastAPI `_require_ground_profile` 404 body: a `detail` carrying the
/// `E_PROFILE_MISMATCH` error object. A drone-profile caller hits every
/// ground-station route with this exact shape.
fn profile_mismatch() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})),
    )
        .into_response()
}

/// True when the resolved profile is a ground station. Resolves through the
/// shared profile module (config `agent.profile` + the on-disk sentinels), the
/// same source of truth the node advertises on the wire, mirroring the Python
/// `is_ground_station`.
fn is_ground_station() -> bool {
    let cfg = crate::config::PairingConfig::load();
    let (profile, _role) = crate::profile::current_profile_and_role(&cfg.agent.profile);
    profile == "ground-station"
}

// ---------------------------------------------------------------------------
// Path seam: the agent config file.
// ---------------------------------------------------------------------------

/// The agent config path (`ADOS_CONFIG`, default `/etc/ados/config.yaml`), the
/// same resolution the sibling read/write routes use.
fn config_yaml_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string()),
    )
}

// ---------------------------------------------------------------------------
// PUT /api/v1/ground-station/wfb — update the stored radio config.
// ---------------------------------------------------------------------------

/// The `PUT .../wfb` request body. Mirrors the FastAPI `WfbUpdate`: three
/// optional fields, each applied only when present (a null / omitted field
/// leaves the stored value untouched).
#[derive(Debug, Default, Deserialize)]
pub struct WfbUpdate {
    #[serde(default)]
    pub channel: Option<i64>,
    #[serde(default)]
    pub bitrate_profile: Option<String>,
    #[serde(default)]
    pub fec: Option<String>,
}

/// `PUT .../wfb` → `{channel, bitrate_profile, fec, persisted[, persist_error]}`.
///
/// Gates on the ground-station profile (404 on a drone), surgically merges the
/// supplied fields into `video.wfb` of the on-disk config (preserving every
/// other key), then returns the radio view (the merged values projected through
/// the Python defaults) plus the `persisted` flag. A persist failure (an I/O
/// fault or a non-root front that cannot write the 0600 config) yields
/// `persisted: false` + a `persist_error` string, matching the FastAPI handler's
/// `save_config()` returning `False`.
pub async fn put_ground_station_wfb(
    State(_state): State<AppState>,
    Json(update): Json<WfbUpdate>,
) -> Response {
    if !is_ground_station() {
        return profile_mismatch();
    }
    put_wfb_at(&config_yaml_path(), &update)
}

/// The write logic against an explicit config path. The public handler resolves
/// the path from the env/etc dir; this takes it directly so a test can point it
/// at a temp file without mutating process-global env.
fn put_wfb_at(config_path: &Path, update: &WfbUpdate) -> Response {
    // Merge the supplied fields into `video.wfb`. The result carries the merged
    // values so the response view reads them without a second disk round-trip,
    // and on a persist failure the same values still answer (matching the
    // FastAPI handler, which mutated its in-memory model before the persist
    // attempt, so the view reflects the request regardless of `persisted`).
    let (channel, bitrate_profile, fec, persist_error) = merge_wfb_fields(config_path, update);

    let mut view = Map::new();
    view.insert("channel".to_string(), json!(channel));
    view.insert("bitrate_profile".to_string(), json!(bitrate_profile));
    view.insert("fec".to_string(), json!(fec));
    view.insert("persisted".to_string(), json!(persist_error.is_none()));
    if let Some(err) = persist_error {
        view.insert("persist_error".to_string(), json!(err));
    }
    Json(Value::Object(view)).into_response()
}

/// Merge the supplied `video.wfb` fields into the on-disk config and return the
/// three resolved view values (`channel`, `bitrate_profile`, `fec`) plus an
/// optional persist-error string.
///
/// Each view value resolves the way the FastAPI `_read_wfb_view` does over the
/// post-mutation model: the request value when supplied, else the existing
/// on-disk value, else the Python default (`channel: 0`, `bitrate_profile:
/// "default"`, `fec: "8/12"`). The atomic merge preserves every other config
/// key. `persist_error` is `None` on a clean write and `Some(message)` on any
/// read/parse/write fault (e.g. the EPERM a non-root front gets on the 0600
/// config), mirroring the FastAPI `save_config()` exception path that flags
/// `persisted: false` with the exception text.
fn merge_wfb_fields(
    config_path: &Path,
    update: &WfbUpdate,
) -> (i64, String, String, Option<String>) {
    use serde_norway::Value as Yaml;

    // Load the existing config (an absent / non-mapping file starts from an
    // empty mapping, matching the Python `data: dict = {}` seed).
    let mut data: Yaml = match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(serde_norway::Mapping::new()),
        },
        Err(_) => Yaml::Mapping(serde_norway::Mapping::new()),
    };

    // The pre-existing values, so a field the request omits keeps its on-disk
    // value (and falls through to the Python default only when truly absent).
    let existing = existing_wfb(&data);

    // Navigate/create `video.wfb` and set each supplied field, preserving every
    // other key and the mapping's insertion order (Python `sort_keys=False`).
    // If the document is shaped so a section cannot be a mapping, the merge is
    // abandoned and the persist is reported failed — the view still answers from
    // the resolved values.
    let mut persist_error: Option<String> = None;
    {
        match wfb_section_mut(&mut data) {
            Some(wfb_map) => {
                if let Some(ch) = update.channel {
                    wfb_map.insert(Yaml::String("channel".to_string()), Yaml::Number(ch.into()));
                }
                if let Some(bp) = &update.bitrate_profile {
                    wfb_map.insert(
                        Yaml::String("bitrate_profile".to_string()),
                        Yaml::String(bp.clone()),
                    );
                }
                if let Some(f) = &update.fec {
                    wfb_map.insert(Yaml::String("fec".to_string()), Yaml::String(f.clone()));
                }
            }
            None => {
                persist_error = Some("config root is not a mapping".to_string());
            }
        }
    }

    // Write the merged document atomically. Any serialize / write fault becomes a
    // persist error; a non-root front gets the OS EPERM string here.
    if persist_error.is_none() {
        if let Err(e) = write_atomic(config_path, &data) {
            persist_error = Some(e);
        }
    }

    // The resolved view values: request → existing → Python default.
    let channel = update.channel.or(existing.channel).unwrap_or(0);
    let bitrate_profile = update
        .bitrate_profile
        .clone()
        .or(existing.bitrate_profile)
        .unwrap_or_else(|| "default".to_string());
    let fec = update
        .fec
        .clone()
        .or(existing.fec)
        .unwrap_or_else(|| "8/12".to_string());

    (channel, bitrate_profile, fec, persist_error)
}

/// The three `video.wfb` view fields already on disk, each `None` when absent /
/// non-typed (so the merge falls through to the Python default only when there
/// is no stored value). Mirrors the `getattr(wfb_cfg, "<field>", <default>)`
/// reads the FastAPI view performs.
#[derive(Default)]
struct ExistingWfb {
    channel: Option<i64>,
    bitrate_profile: Option<String>,
    fec: Option<String>,
}

/// Read the existing `video.wfb` view fields from a parsed config value.
fn existing_wfb(data: &serde_norway::Value) -> ExistingWfb {
    let wfb = data.get("video").and_then(|v| v.get("wfb"));
    let wfb = match wfb {
        Some(w) => w,
        None => return ExistingWfb::default(),
    };
    ExistingWfb {
        channel: wfb.get("channel").and_then(norway_to_i64),
        bitrate_profile: wfb
            .get("bitrate_profile")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        fec: wfb.get("fec").and_then(|v| v.as_str()).map(str::to_string),
    }
}

/// Navigate/create `video.wfb` as a mutable mapping, returning `None` when a
/// node along the path exists but is not a mapping AND cannot be replaced
/// without clobbering the document root (only the root being a non-mapping fails;
/// a non-mapping `video` / `wfb` is replaced with an empty mapping, matching the
/// tx-power persist's create-on-conflict behavior).
fn wfb_section_mut(data: &mut serde_norway::Value) -> Option<&mut serde_norway::Mapping> {
    use serde_norway::Value as Yaml;
    let root = data.as_mapping_mut()?;
    let video = root
        .entry(Yaml::String("video".to_string()))
        .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
    if !video.is_mapping() {
        *video = Yaml::Mapping(serde_norway::Mapping::new());
    }
    let video_map = video.as_mapping_mut()?;
    let wfb = video_map
        .entry(Yaml::String("wfb".to_string()))
        .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
    if !wfb.is_mapping() {
        *wfb = Yaml::Mapping(serde_norway::Mapping::new());
    }
    wfb.as_mapping_mut()
}

/// Coerce a serde_norway scalar to `i64`, accepting an integer or a float.
/// `None` for a non-number. Mirrors the Python `int(...)` over a numeric config
/// value.
fn norway_to_i64(v: &serde_norway::Value) -> Option<i64> {
    match v {
        serde_norway::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// Serialize `data` to YAML and write it to `path` atomically (ensure the parent
/// dir, write a `.tmp` sibling, rename over the target). Returns the error string
/// on any serialize / I/O fault so the route can flag the persist failure.
/// Mirrors the tmp-write + `os.replace` idiom the config persist uses.
fn write_atomic(path: &Path, data: &serde_norway::Value) -> Result<(), String> {
    let body = serde_norway::to_string(data).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = {
        let mut ext = path
            .extension()
            .map(|e| e.to_os_string())
            .unwrap_or_default();
        ext.push(".tmp");
        path.with_extension(ext)
    };
    std::fs::write(&tmp, body.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── profile gate ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn profile_mismatch_golden_body() {
        // The body the handler returns on a drone, pinned as the golden fixture
        // for the conformance harness's off-a-drone diff. (The success body
        // depends on a live ground-station profile, so it is bench-validated.)
        let resp = profile_mismatch();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {"code": "E_PROFILE_MISMATCH"}}})
        );
    }

    // ── the merge: applies supplied fields, keeps the rest ────────────────────

    #[tokio::test]
    async fn put_merges_supplied_fields_and_persists() {
        // Seed a config with an existing wfb section + an unrelated key to prove
        // the merge applies the supplied fields, keeps the unset one, and
        // preserves the rest of the file.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: gs-1\nvideo:\n  wfb:\n    channel: 149\n    bitrate_profile: default\n    fec: 8/12\n",
        )
        .unwrap();

        let update = WfbUpdate {
            channel: Some(161),
            bitrate_profile: Some("high".to_string()),
            fec: None,
        };
        let resp = put_wfb_at(&cfg, &update);
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // channel + profile take the request; fec keeps the on-disk value.
        assert_eq!(
            body,
            json!({
                "channel": 161,
                "bitrate_profile": "high",
                "fec": "8/12",
                "persisted": true,
            })
        );

        // The on-disk merge applied the supplied fields and kept the rest.
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(wfb.get("channel").and_then(norway_to_i64), Some(161));
        assert_eq!(
            wfb.get("bitrate_profile").and_then(|v| v.as_str()),
            Some("high")
        );
        assert_eq!(wfb.get("fec").and_then(|v| v.as_str()), Some("8/12"));
        // The unrelated agent.name survived.
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("gs-1")
        );
    }

    #[tokio::test]
    async fn put_with_no_existing_section_uses_python_defaults_for_unset_fields() {
        // An absent file → only the supplied field is written; the unset fields
        // resolve to the Python defaults in the response view.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let update = WfbUpdate {
            channel: Some(48),
            bitrate_profile: None,
            fec: None,
        };
        let resp = put_wfb_at(&cfg, &update);
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "channel": 48,
                "bitrate_profile": "default",
                "fec": "8/12",
                "persisted": true,
            })
        );
        // The file now holds the video.wfb section with just the supplied field.
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(wfb.get("channel").and_then(norway_to_i64), Some(48));
        // bitrate_profile / fec were not supplied, so they are not written.
        assert!(wfb.get("bitrate_profile").is_none());
        assert!(wfb.get("fec").is_none());
    }

    #[tokio::test]
    async fn put_with_an_empty_body_echoes_existing_or_defaults() {
        // An empty body (all fields null) is a no-op write that echoes the
        // existing on-disk values (here: defaults, since the file is empty).
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let resp = put_wfb_at(&cfg, &WfbUpdate::default());
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "channel": 0,
                "bitrate_profile": "default",
                "fec": "8/12",
                "persisted": true,
            })
        );
    }

    // ── persist failure → persisted:false + persist_error ─────────────────────

    #[tokio::test]
    async fn a_write_fault_flags_persisted_false_with_persist_error() {
        // Point the config at a path whose parent cannot be created (a file
        // standing where a directory would need to be) so write_atomic's
        // create_dir_all fails → persisted:false + a persist_error string. The
        // view still answers with the resolved fields.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let cfg = blocker.join("config.yaml"); // parent "blocker" is a file

        let update = WfbUpdate {
            channel: Some(157),
            bitrate_profile: None,
            fec: None,
        };
        let resp = put_wfb_at(&cfg, &update);
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // The three view fields resolve (request → existing → default); persisted
        // is false and a persist_error string is present.
        assert_eq!(body["channel"], json!(157));
        assert_eq!(body["bitrate_profile"], json!("default"));
        assert_eq!(body["fec"], json!("8/12"));
        assert_eq!(body["persisted"], json!(false));
        assert!(body["persist_error"].as_str().is_some());
    }

    // ── existing_wfb projection ───────────────────────────────────────────────

    #[test]
    fn existing_wfb_reads_typed_fields_and_skips_missing() {
        let data: serde_norway::Value =
            serde_norway::from_str("video:\n  wfb:\n    channel: 153\n    bitrate_profile: low\n")
                .unwrap();
        let e = existing_wfb(&data);
        assert_eq!(e.channel, Some(153));
        assert_eq!(e.bitrate_profile.as_deref(), Some("low"));
        assert_eq!(e.fec, None);
        // No video section at all → all None.
        let empty: serde_norway::Value = serde_norway::from_str("agent:\n  name: x\n").unwrap();
        let e2 = existing_wfb(&empty);
        assert!(e2.channel.is_none() && e2.bitrate_profile.is_none() && e2.fec.is_none());
    }
}
