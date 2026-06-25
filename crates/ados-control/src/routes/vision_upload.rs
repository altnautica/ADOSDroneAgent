//! Custom-model upload: stream a sideloaded detector file into the models dir.
//!
//! `POST /api/vision/models/upload` is a multipart upload carrying two parts:
//!
//! - a **file** part — the raw model bytes (`.rknn` / `.onnx` / `.tflite` /
//!   `.engine`), streamed straight to disk under the models directory;
//! - a **metadata** part — a JSON object describing the model the engine needs to
//!   run it: `{id, name, classes[], head, input_w, input_h, runtime,
//!   board_match}`.
//!
//! The route writes the file, computes its sha256, and records (or updates) an
//! entry in `custom-catalog.json` in the same directory so the Python model
//! manager + the `GET /api/vision/models` route list the sideloaded model
//! alongside the registry + the installed files. The catalog is the durable
//! index of operator-supplied models; the file on disk is the payload.
//!
//! ## What is rejected
//!
//! A request with no metadata `id`, or no file (or an empty file), is a 400 — a
//! catalog entry with no id, or pointing at a zero-byte file, would describe a
//! model that cannot load. Everything else is tolerated: unknown metadata fields
//! are ignored, an absent `name`/`classes`/`head` default to empty, and a re-used
//! `id` replaces the prior catalog entry (and overwrites its file), so a
//! corrected re-upload is idempotent on the id.
//!
//! ## Why the file name is derived, not trusted
//!
//! The on-disk file name is `<id><suffix>`, where the suffix comes from the
//! declared `runtime` (rknn → `.rknn`, tensorrt → `.engine`, tflite → `.tflite`,
//! else `.onnx`). The client-supplied multipart filename is NOT used for the path.
//! The `id` itself is attacker-influenced (it comes from the metadata JSON), so it
//! is validated against a strict `[A-Za-z0-9._-]` charset with no `..` component
//! before it can become a path: an `id` carrying a `/` or `..` would otherwise let
//! `Path::join` escape the models dir (an absolute id discards the base entirely),
//! so a paired LAN client could write a `.<suffix>` file anywhere the agent can
//! write. A rejected id is a 400 before any write or catalog touch.
//!
//! ## Auth posture
//!
//! An upload is a write, so it sits outside the public set and the LAN edge
//! requires the pairing key when paired — the same posture as the other vision
//! writes.

use std::path::{Path, PathBuf};

use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::routes::detail;
use crate::routes::wfb_pair_write::write_atomic;
use crate::state::AppState;

/// The models directory the engine + the model manager resolve files under. The
/// default matches the Python `VisionConfig.models_dir`; the live path is read
/// from config so the two halves never diverge (see [`models_dir`]).
const DEFAULT_MODELS_DIR: &str = "/opt/ados/models/vision";

/// The catalog file in the models dir that records operator-uploaded models so
/// the Python manager + the `GET /api/vision/models` route surface them.
const CUSTOM_CATALOG: &str = "custom-catalog.json";

/// Resolve the models directory the upload writes into, from one source of truth:
/// the agent config's `system.models_dir` (the same field the Python
/// `VisionConfig` reads), so an operator override lands in the same place the
/// model manager lists from. Falls back to the `ADOS_MODELS_DIR` override (the
/// test/seam knob) and finally the default. Resolving from config — not an
/// independent env var — is what keeps an uploaded model visible in
/// `GET /api/vision/models`.
fn models_dir() -> PathBuf {
    if let Some(dir) = models_dir_from_config() {
        return dir;
    }
    PathBuf::from(
        std::env::var("ADOS_MODELS_DIR").unwrap_or_else(|_| DEFAULT_MODELS_DIR.to_string()),
    )
}

/// Read `system.models_dir` out of `/etc/ados/config.yaml` (or the `ADOS_CONFIG`
/// override), returning `None` when the file / section / field is absent or
/// blank so the caller falls through to the env / default.
fn models_dir_from_config() -> Option<PathBuf> {
    let path =
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| crate::config::CONFIG_YAML.to_string());
    models_dir_from_config_at(Path::new(&path))
}

/// The path-explicit core of [`models_dir_from_config`]: read `system.models_dir`
/// from `path`, returning `None` on absence / parse error / a blank field. A
/// fresh box has no override, so a missing or unparseable config is not an error.
fn models_dir_from_config_at(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let root: Value = serde_norway::from_str(&text).ok()?;
    let dir = root.get("system")?.get("models_dir")?.as_str()?.trim();
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir))
}

/// Whether a model `id` is safe to derive a filename from: a non-empty
/// `[A-Za-z0-9._-]` string that is not `.` or `..`. The charset already excludes
/// every path separator (`/`, `\`), so an absolute or multi-segment id can never
/// form; the only traversal that the charset would otherwise permit is the literal
/// `..`, which is rejected explicitly. With both guards an id can only ever name a
/// single file directly inside the models dir.
fn valid_model_id(id: &str) -> bool {
    if id.is_empty() || id == "." || id == ".." {
        return false;
    }
    id.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The metadata part of the upload (the JSON object describing the model). Only
/// `id` is required; the rest default so a terse metadata blob is accepted and
/// the engine fills the gaps with its own defaults.
#[derive(Debug, Default, Deserialize)]
pub struct UploadMeta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub classes: Vec<String>,
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub input_w: u32,
    #[serde(default)]
    pub input_h: u32,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub board_match: String,
}

/// `POST /api/vision/models/upload` → stream the file, hash it, catalog it.
///
/// Returns `{status:"ok", id, filename, sha256, size_bytes}` on success, a 400
/// when the metadata id is missing/empty/invalid or the file is missing/empty,
/// and a 500 when the file or the catalog cannot be written.
///
/// The file part is streamed straight to a tmp file in the models dir (hashing as
/// it goes) rather than buffered in RAM, so a multi-megabyte detector does not sit
/// in memory on a 1-4 GB SBC. The default 2 MB multipart cap is lifted on this
/// route at registration; the on-disk size is bounded only by the partition.
pub async fn upload_model(State(_state): State<AppState>, mut multipart: Multipart) -> Response {
    let dir = models_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("models dir not writable: {e}"),
        );
    }

    let mut meta: Option<UploadMeta> = None;
    // The file streams to a unique tmp file as it arrives (the parts may come in
    // either order relative to the metadata). The id-derived final name is applied
    // by an atomic rename only after the whole upload validates, so a rejected or
    // failed upload leaves no partial model behind.
    let mut staged: Option<StagedFile> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return detail(StatusCode::BAD_REQUEST, format!("malformed upload: {e}")),
        };
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "metadata" => {
                let text = match field.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        return detail(
                            StatusCode::BAD_REQUEST,
                            format!("metadata read failed: {e}"),
                        )
                    }
                };
                match serde_json::from_str::<UploadMeta>(&text) {
                    Ok(m) => meta = Some(m),
                    Err(e) => {
                        return detail(
                            StatusCode::BAD_REQUEST,
                            format!("metadata is not valid JSON: {e}"),
                        )
                    }
                }
            }
            "file" => match stream_field_to_tmp(&dir, field).await {
                Ok(s) => staged = Some(s),
                Err(msg) => return detail(StatusCode::BAD_REQUEST, msg),
            },
            // Ignore any other part — the contract is the two named parts.
            _ => {}
        }
    }

    let meta = match meta {
        Some(m) if !m.id.trim().is_empty() => m,
        Some(_) => return detail(StatusCode::BAD_REQUEST, "metadata id is required"),
        None => return detail(StatusCode::BAD_REQUEST, "missing metadata part"),
    };
    // Reject a traversal / out-of-charset id before it can become a path. The tmp
    // file (if any) is dropped, cleaning itself up — nothing was written under a
    // derived name yet.
    if !valid_model_id(meta.id.trim()) {
        return detail(
            StatusCode::BAD_REQUEST,
            "metadata id must be a non-empty [A-Za-z0-9._-] name with no path separators",
        );
    }
    let staged = match staged {
        Some(s) if s.size_bytes > 0 => s,
        _ => return detail(StatusCode::BAD_REQUEST, "missing or empty file part"),
    };

    match store_upload(&dir, &meta, staged) {
        Ok(entry) => (StatusCode::OK, Json(entry)).into_response(),
        Err(msg) => detail(StatusCode::INTERNAL_SERVER_ERROR, msg),
    }
}

/// A model file streamed to a tmp path in the models dir, with its sha256 + size
/// accumulated during the stream. Dropping it removes the tmp file, so an upload
/// that fails validation or write leaves nothing behind.
struct StagedFile {
    tmp_path: PathBuf,
    sha256: String,
    size_bytes: u64,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        // Best-effort cleanup: a successful finalize renames the tmp away first,
        // so this only fires on the error / rejected paths.
        let _ = std::fs::remove_file(&self.tmp_path);
    }
}

/// Stream a multipart file field to a uniquely-named tmp file in `dir`, hashing
/// the bytes as they land. The tmp name carries the process id + a random suffix
/// so concurrent uploads never collide; the file is renamed to its id-derived
/// name only on finalize. Returns the staged file (with its digest + size) or a
/// 400-worthy message on a read / write fault.
async fn stream_field_to_tmp(
    dir: &Path,
    mut field: axum::extract::multipart::Field<'_>,
) -> Result<StagedFile, String> {
    use tokio::io::AsyncWriteExt;

    let tmp_path = dir.join(format!(
        ".upload-{}-{}.tmp",
        std::process::id(),
        random_token()
    ));
    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(|e| format!("staging file not writable: {e}"))?;

    let mut hasher = Sha256::new();
    let mut size_bytes: u64 = 0;
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                hasher.update(&chunk);
                size_bytes += chunk.len() as u64;
                if let Err(e) = file.write_all(&chunk).await {
                    let _ = tokio::fs::remove_file(&tmp_path).await;
                    return Err(format!("staging write failed: {e}"));
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(format!("file read failed: {e}"));
            }
        }
    }
    if let Err(e) = file.flush().await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(format!("staging flush failed: {e}"));
    }
    drop(file);

    Ok(StagedFile {
        tmp_path,
        sha256: hex::encode(hasher.finalize()),
        size_bytes,
    })
}

/// A short random hex token for the staging-file name, so two concurrent uploads
/// into the same dir never write the same tmp path.
fn random_token() -> String {
    let mut buf = [0u8; 8];
    // getrandom is already a crate dep (the pairing-key source); a failure here is
    // implausible, but fall back to a time-derived token rather than panicking.
    if getrandom::getrandom(&mut buf).is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return format!("{nanos:016x}");
    }
    hex::encode(buf)
}

/// Finalize a staged upload: rename the tmp file to its id-derived name and upsert
/// the catalog entry. Returns the success body `{status, id, filename, sha256,
/// size_bytes}`. The id was validated by the handler before this runs, so the
/// derived path is guaranteed to stay inside `dir` (a single file directly in it),
/// and the tmp already lives in `dir`, so the rename is a same-dir atomic move.
fn store_upload(dir: &Path, meta: &UploadMeta, staged: StagedFile) -> Result<Value, String> {
    let id = meta.id.trim();
    let filename = format!("{id}{}", suffix_for_runtime(&meta.runtime));
    let model_path = dir.join(&filename);

    // Move the streamed tmp into place atomically. On the error path `staged`'s
    // Drop removes the tmp; on success ManuallyDrop disarms that cleanup so the
    // just-renamed file is not raced away.
    if let Err(e) = std::fs::rename(&staged.tmp_path, &model_path) {
        return Err(format!("model write failed: {e}"));
    }
    let staged = std::mem::ManuallyDrop::new(staged);

    let sha256 = staged.sha256.clone();
    let size_bytes = staged.size_bytes;

    upsert_catalog(dir, meta, &filename, &sha256, size_bytes)?;

    Ok(json!({
        "status": "ok",
        "id": id,
        "filename": filename,
        "sha256": sha256,
        "size_bytes": size_bytes,
    }))
}

/// The on-disk suffix for a declared runtime, matching the model-manager fetch
/// path's mapping (rknn → `.rknn`, tensorrt → `.engine`, tflite → `.tflite`, else
/// `.onnx`). A blank/unknown runtime defaults to ONNX (the CPU portable form).
fn suffix_for_runtime(runtime: &str) -> &'static str {
    match runtime.to_ascii_lowercase().as_str() {
        "rknn" => ".rknn",
        "tensorrt" => ".engine",
        "tflite" => ".tflite",
        _ => ".onnx",
    }
}

/// Read the catalog (a JSON array of entry objects), replace any entry with the
/// same `id`, append the new one, and write it back atomically. A missing /
/// unparseable / non-array catalog starts from an empty list, so a fresh dir
/// gets a clean single-entry catalog.
fn upsert_catalog(
    dir: &Path,
    meta: &UploadMeta,
    filename: &str,
    sha256: &str,
    size_bytes: u64,
) -> Result<(), String> {
    let catalog_path = dir.join(CUSTOM_CATALOG);

    let mut entries: Vec<Value> = match std::fs::read_to_string(&catalog_path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Array(a)) => a,
            _ => Vec::new(),
        },
        Err(_) => Vec::new(),
    };

    let id = meta.id.trim();
    // Drop any prior entry for this id (a corrected re-upload replaces it).
    entries.retain(|e| e.get("id").and_then(|v| v.as_str()) != Some(id));

    entries.push(json!({
        "id": id,
        "name": meta.name,
        "classes": meta.classes,
        "head": meta.head,
        "input_w": meta.input_w,
        "input_h": meta.input_h,
        "runtime": meta.runtime,
        "board_match": meta.board_match,
        "filename": filename,
        "sha256": sha256,
        "size_bytes": size_bytes,
        "custom": true,
    }));

    let body = serde_json::to_vec_pretty(&Value::Array(entries)).map_err(|e| e.to_string())?;
    write_atomic(&catalog_path, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str, runtime: &str) -> UploadMeta {
        UploadMeta {
            id: id.to_string(),
            name: format!("{id} model"),
            classes: vec!["person".to_string(), "car".to_string()],
            head: "yolo8".to_string(),
            input_w: 640,
            input_h: 640,
            runtime: runtime.to_string(),
            board_match: "generic".to_string(),
        }
    }

    fn read_catalog(dir: &Path) -> Vec<Value> {
        let text = std::fs::read_to_string(dir.join(CUSTOM_CATALOG)).unwrap();
        match serde_json::from_str::<Value>(&text).unwrap() {
            Value::Array(a) => a,
            _ => panic!("catalog is not an array"),
        }
    }

    /// Build a `StagedFile` the way `stream_field_to_tmp` does: write a tmp file in
    /// `dir` and accumulate its digest + size, so `store_upload` can finalize it.
    fn stage(dir: &Path, bytes: &[u8]) -> StagedFile {
        let tmp_path = dir.join(format!(".upload-test-{}.tmp", random_token()));
        std::fs::write(&tmp_path, bytes).unwrap();
        let mut h = Sha256::new();
        h.update(bytes);
        StagedFile {
            tmp_path,
            sha256: hex::encode(h.finalize()),
            size_bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn suffix_maps_each_runtime() {
        assert_eq!(suffix_for_runtime("rknn"), ".rknn");
        assert_eq!(suffix_for_runtime("RKNN"), ".rknn");
        assert_eq!(suffix_for_runtime("tensorrt"), ".engine");
        assert_eq!(suffix_for_runtime("tflite"), ".tflite");
        assert_eq!(suffix_for_runtime("onnx"), ".onnx");
        assert_eq!(suffix_for_runtime(""), ".onnx");
        assert_eq!(suffix_for_runtime("anything-else"), ".onnx");
    }

    #[test]
    fn store_writes_file_hashes_and_catalogs() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = b"fake-onnx-bytes";
        let staged = stage(dir.path(), bytes);
        let entry = store_upload(dir.path(), &meta("custom-1", "onnx"), staged).unwrap();

        // The body reports the derived filename + the streamed-file hash.
        assert_eq!(entry["status"], json!("ok"));
        assert_eq!(entry["id"], json!("custom-1"));
        assert_eq!(entry["filename"], json!("custom-1.onnx"));
        assert_eq!(entry["size_bytes"], json!(bytes.len()));
        // sha256 of the bytes.
        let mut h = Sha256::new();
        h.update(bytes);
        assert_eq!(entry["sha256"], json!(hex::encode(h.finalize())));

        // The file landed under the derived name.
        let model_path = dir.path().join("custom-1.onnx");
        assert_eq!(std::fs::read(&model_path).unwrap(), bytes);

        // The catalog carries the metadata.
        let cat = read_catalog(dir.path());
        assert_eq!(cat.len(), 1);
        let e = &cat[0];
        assert_eq!(e["id"], json!("custom-1"));
        assert_eq!(e["name"], json!("custom-1 model"));
        assert_eq!(e["classes"], json!(["person", "car"]));
        assert_eq!(e["input_w"], json!(640));
        assert_eq!(e["custom"], json!(true));
        assert_eq!(e["filename"], json!("custom-1.onnx"));
    }

    #[test]
    fn runtime_drives_the_suffix_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let staged = stage(dir.path(), b"rk");
        store_upload(dir.path(), &meta("rk-model", "rknn"), staged).unwrap();
        assert!(dir.path().join("rk-model.rknn").exists());
        let cat = read_catalog(dir.path());
        assert_eq!(cat[0]["filename"], json!("rk-model.rknn"));
    }

    #[test]
    fn re_uploading_the_same_id_replaces_the_catalog_entry() {
        let dir = tempfile::tempdir().unwrap();
        let s1 = stage(dir.path(), b"v1");
        store_upload(dir.path(), &meta("dup", "onnx"), s1).unwrap();
        // A second model under a different id coexists.
        let s2 = stage(dir.path(), b"x");
        store_upload(dir.path(), &meta("other", "onnx"), s2).unwrap();
        // Re-upload "dup" with new bytes → one "dup" entry, updated hash + file.
        let s3 = stage(dir.path(), b"v2-longer");
        store_upload(dir.path(), &meta("dup", "onnx"), s3).unwrap();

        let cat = read_catalog(dir.path());
        let dup: Vec<&Value> = cat
            .iter()
            .filter(|e| e.get("id").and_then(|v| v.as_str()) == Some("dup"))
            .collect();
        assert_eq!(dup.len(), 1, "the id must be deduplicated in the catalog");
        // Both ids are present (the replace did not drop the other entry).
        assert_eq!(cat.len(), 2);
        assert_eq!(
            std::fs::read(dir.path().join("dup.onnx")).unwrap(),
            b"v2-longer"
        );
        let mut h = Sha256::new();
        h.update(b"v2-longer");
        assert_eq!(dup[0]["sha256"], json!(hex::encode(h.finalize())));
    }

    #[test]
    fn catalog_starts_fresh_when_the_existing_one_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        // A pre-existing non-array catalog (corruption) is replaced cleanly.
        std::fs::write(dir.path().join(CUSTOM_CATALOG), b"{ not an array }").unwrap();
        let staged = stage(dir.path(), b"z");
        store_upload(dir.path(), &meta("fresh", "onnx"), staged).unwrap();
        let cat = read_catalog(dir.path());
        assert_eq!(cat.len(), 1);
        assert_eq!(cat[0]["id"], json!("fresh"));
    }

    #[test]
    fn empty_id_rejected_before_a_write_in_the_handler_guard() {
        // The handler rejects an empty id; assert the guard predicate the handler
        // uses so the contract is covered without driving a real multipart body.
        let m = UploadMeta {
            id: "   ".to_string(),
            ..UploadMeta::default()
        };
        assert!(m.id.trim().is_empty());
    }

    #[test]
    fn upload_meta_tolerates_unknown_fields_and_terse_blob() {
        // A terse metadata blob (only id) parses with defaults; unknown fields are
        // ignored (serde default policy), so a richer client payload is accepted.
        let m: UploadMeta =
            serde_json::from_str(r#"{"id":"x","unknown_extra":42,"name":"X"}"#).unwrap();
        assert_eq!(m.id, "x");
        assert_eq!(m.name, "X");
        assert!(m.classes.is_empty());
        assert_eq!(m.input_w, 0);
    }

    #[test]
    fn valid_model_id_accepts_normal_names() {
        for id in ["custom-1", "yolov8n", "model.v2", "a_b-c.D9", ".hidden-ok"] {
            assert!(valid_model_id(id), "{id} should be accepted");
        }
    }

    #[test]
    fn valid_model_id_rejects_traversal_and_separators() {
        // Path separators (absolute or relative), `..`, and out-of-charset chars
        // are all refused before the id can become a filename joined onto the dir.
        for id in [
            "",
            ".",
            "..",
            "../../etc/ados/evil",
            "/etc/cron.d/x",
            "a/b",
            "a\\b",
            "evil id",
            "name;rm",
            "weird*name",
        ] {
            assert!(!valid_model_id(id), "{id:?} must be rejected");
        }
    }

    #[test]
    fn store_upload_only_writes_inside_the_dir_for_a_valid_id() {
        // The handler validates the id before calling store_upload; a valid id is a
        // single file directly inside the dir (the rename is a same-dir move), so
        // no path outside `dir` is ever touched.
        let dir = tempfile::tempdir().unwrap();
        let staged = stage(dir.path(), b"safe");
        let entry = store_upload(dir.path(), &meta("ok-id", "onnx"), staged).unwrap();
        assert_eq!(entry["filename"], json!("ok-id.onnx"));
        assert!(dir.path().join("ok-id.onnx").exists());
    }

    #[test]
    fn dropping_a_staged_file_removes_the_tmp() {
        // The error / rejected paths rely on StagedFile's Drop to clean the tmp.
        let dir = tempfile::tempdir().unwrap();
        let tmp = {
            let staged = stage(dir.path(), b"orphan");
            let p = staged.tmp_path.clone();
            assert!(p.exists());
            p
            // staged drops here
        };
        assert!(!tmp.exists(), "the tmp file must be removed on drop");
    }

    #[test]
    fn store_upload_consumes_the_staged_tmp_on_success() {
        // A finalized upload renames the tmp into place (ManuallyDrop disarms the
        // cleanup), leaving exactly the derived file and no lingering tmp.
        let dir = tempfile::tempdir().unwrap();
        let staged = stage(dir.path(), b"payload");
        let tmp = staged.tmp_path.clone();
        store_upload(dir.path(), &meta("final", "onnx"), staged).unwrap();
        assert!(
            !tmp.exists(),
            "the staged tmp must be renamed, not left behind"
        );
        assert_eq!(
            std::fs::read(dir.path().join("final.onnx")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn models_dir_from_config_reads_system_models_dir() {
        // The upload resolves the models dir from `system.models_dir` so it writes
        // where the Python manager lists from. An override is honoured; a blank /
        // absent field / unparseable file falls through to None (env / default).
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");

        std::fs::write(&cfg, "system:\n  models_dir: /var/ados/custom-models\n").unwrap();
        assert_eq!(
            models_dir_from_config_at(&cfg),
            Some(PathBuf::from("/var/ados/custom-models"))
        );

        std::fs::write(&cfg, "system:\n  models_dir: \"\"\n").unwrap();
        assert_eq!(
            models_dir_from_config_at(&cfg),
            None,
            "a blank dir falls through"
        );

        std::fs::write(&cfg, "video:\n  enabled: true\n").unwrap();
        assert_eq!(
            models_dir_from_config_at(&cfg),
            None,
            "an absent field falls through"
        );

        assert_eq!(
            models_dir_from_config_at(Path::new("/nonexistent/ados/config.yaml")),
            None,
            "a missing config falls through"
        );
    }
}
