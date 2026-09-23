//! WFB-ng auto-pair toggle write route.
//!
//! One operator knob the GCS pairing card writes:
//!
//! - **`PUT /api/wfb/pair/auto-pair`** — toggle the persisted auto-bind arm flag
//!   (`video.wfb.auto_pair_enabled`). The body is `{"enabled": <bool>}`.
//!
//! ## What this route does, faithfully to the residual handler
//!
//! The residual `wfb.py` route resolves the bind-protocol role from the agent
//! profile, then calls the pair manager's `set_auto_pair(enabled, role)`. That
//! method is NOT a bare bool write — it first computes the live pair *status*
//! (the same snapshot `GET /api/wfb/pair` returns: `paired`, peer device-id,
//! paired-at, the blake2b-8 key fingerprint, the current auto-pair flag, role),
//! then branches:
//!
//! - **Re-arm on a paired rig** (`enabled` true AND already paired) is refused:
//!   the response is the status snapshot with `auto_pair_enabled: false` and an
//!   added `rearm_blocked: true`, and NOTHING is persisted.
//! - **...unless `force` is set**, the escape hatch this route adds on top of the
//!   residual behaviour. The refusal above left an operator with exactly one way
//!   to re-arm a rig holding a suspect key: `unpair` first, which DELETES the
//!   key. That is the worst available move if the key turns out to have been
//!   fine, because it turns "possibly stale" into "definitely gone". A forced
//!   re-arm instead records a one-shot against the key's own fingerprint in the
//!   pair-proof record (`ados_protocol::pair_proof`), persists the arm flag, and
//!   leaves the key exactly where it is; the supervisor's re-arm latch consumes
//!   the one-shot on its next tick and opens a single bind window.
//! - **Otherwise** it persists the new arm flag — `video.wfb.auto_pair_enabled`
//!   and nothing else — and returns the status snapshot with
//!   `auto_pair_enabled` set to the requested value. The peer and `paired_at`
//!   belong to the bind that wrote them; the toggle never rewrites them from its
//!   own read, which is lossy (a timestamp-shaped `paired_at` reads as null) and
//!   may predate a bind that completes while the request is in flight. Arming an
//!   unpaired rig also
//!   drops the local-retry request (`ados_protocol::pair_proof`) the supervisor's
//!   auto-pair loop consumes: a loop that spent its local attempts and parked on
//!   the cloud relay holds that verdict in memory, and a config flag that was
//!   already `true` would never reach it.
//!
//! The native front holds no in-process pair manager — the supervisor reads the
//! same on-disk YAML this route writes on its own cadence, plus the proof record
//! and the retry request.
//!
//! ## Persist and the reported outcome
//!
//! The persist goes through the shared config store (`crate::config_store`):
//! locked, owner-only, and never over a document it could not read or parse.
//! The route reports what actually happened: every `200` body carries
//! `applied` (whether the change will take effect), and a persist, proof-record
//! or retry-request write that fails is a `500 {"detail": {"error", "message"}}`
//! rather than a success that did nothing.
//!
//! ## Response shape
//!
//! Persist path: `{paired, paired_with_device_id, paired_at, fingerprint,
//! auto_pair_enabled: <requested>, role, applied: true}`, plus `retry_requested:
//! true` when arming an unpaired rig. Re-arm-blocked path: the same keys with
//! `auto_pair_enabled: false`, `rearm_blocked: true` and `applied: false`. Forced
//! path: the persist field set plus `rearm_blocked: false` and `forced: true`.
//! `force` is absent by default, so a client that does not send it still gets
//! the refusal on a paired rig.

use std::path::Path;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config_store::{section_path, update_config, ConfigWriteError};
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Persist: the arm flag, and only the arm flag.
// ---------------------------------------------------------------------------

/// Set `video.wfb.auto_pair_enabled`, inside the config store's lock, touching
/// no other key. The pair record (peer, `paired_at`, the GS mirror) is the
/// bind's; re-writing it from the status snapshot erased a `paired_at` the read
/// projection demotes to null, and wrote back a pre-bind peer over a bind that
/// landed mid-request.
fn persist_auto_pair_flag(config_path: &Path, enabled: bool) -> Result<(), ConfigWriteError> {
    use serde_norway::Value as Yaml;
    update_config(config_path, |root| {
        section_path(root, &["video", "wfb"]).insert(
            Yaml::String("auto_pair_enabled".to_string()),
            Yaml::Bool(enabled),
        );
        Ok(())
    })
    .map(|_| ())
}

/// Whether the supervisor's re-arm latch is switched on
/// (`video.wfb.pair_rearm.enabled`, default on). With it off the latch never
/// reads a forced one-shot, so a force recorded now would sit in the proof
/// record and fire whenever the latch is next enabled.
fn rearm_latch_enabled(config_path: &Path) -> bool {
    crate::config::load_config_object(config_path)
        .pointer("/video/wfb/pair_rearm/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

// ---------------------------------------------------------------------------
// PUT /api/wfb/pair/auto-pair — toggle the arm flag.
// ---------------------------------------------------------------------------

/// The `PUT /api/wfb/pair/auto-pair` request body: the required `enabled` bool
/// plus an optional `force` that overrides the re-arm refusal on a paired rig.
#[derive(Debug, Deserialize)]
pub struct AutoPairToggleRequest {
    pub enabled: bool,
    /// Re-arm a rig that already holds a key, without deleting that key.
    ///
    /// The only way to do this used to be `unpair` first, which DELETES the key —
    /// the worst available move when the key turns out to have been fine, since
    /// it turns "possibly stale" into "definitely gone". This opens one bind
    /// window and leaves the key exactly where it is; if the bind fails, the rig
    /// still has what it had.
    #[serde(default)]
    pub force: bool,
}

/// `PUT /api/wfb/pair/auto-pair` → toggle the auto-bind arm flag.
///
/// Resolves the role from the profile, computes the live pair status, and either
/// refuses a re-arm on a paired rig (returning the status with `rearm_blocked:
/// true`, no persist) or persists the new flag and returns the status with the
/// requested value. Every `200` body carries `applied`: whether the change will
/// actually take effect. A persist the config store refuses, or a retry request
/// the supervisor will never see, is a `500` naming what failed.
///
/// Arming an unpaired rig also drops the local-retry request the supervisor's
/// auto-pair loop consumes, which is what brings a loop that parked on the cloud
/// relay back to binding locally (`retry_requested: true`).
///
/// With `force`, a re-arm on a paired rig is granted instead of refused: the
/// one-shot flag is recorded against the key's own fingerprint in the pair-proof
/// record, and the supervisor's latch consumes it on its next tick.
pub async fn put_auto_pair(
    State(state): State<AppState>,
    Json(req): Json<AutoPairToggleRequest>,
) -> Response {
    let paths = &state.pairing_paths;
    put_auto_pair_at(
        &paths.config,
        &paths.wfb_key_dir,
        Path::new(ados_protocol::pair_proof::PAIR_PROOF_PATH),
        Path::new(ados_protocol::pair_proof::AUTO_PAIR_RETRY_PATH),
        crate::wfb_pair_state::bind_role(paths),
        req.enabled,
        req.force,
    )
}

/// Record the one-shot operator force against the key's OWN fingerprint.
///
/// Keying it to the fingerprint is what keeps the hatch honest: if the key is
/// replaced between the request and the supervisor's next tick, the record no
/// longer matches and the force is discarded with the rest of it, rather than
/// firing at whatever key happens to be there. The rest of the record — the
/// proof, the spent episodes — is loaded and preserved, because forcing one
/// window is not a reason to forget everything else known about the key.
fn record_force_rearm(proof_path: &Path, role: &str, fingerprint: &str) -> std::io::Result<()> {
    let mut proof = ados_protocol::pair_proof::load_for(proof_path, role, fingerprint).proof;
    proof.force_rearm = true;
    ados_protocol::pair_proof::write_pair_proof_to(proof_path, &proof)
}

/// The `500` for a change that did not land: `{"detail": {"error", "message"}}`.
fn not_applied(error: &str, message: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"detail": {"error": error, "message": message}})),
    )
        .into_response()
}

/// The auto-pair toggle logic against explicit config + key-dir + proof-record +
/// retry-request paths and a resolved role. The public handler resolves all of
/// them from the app state / env; this takes them directly so a test can point
/// them at temp paths.
fn put_auto_pair_at(
    config_path: &Path,
    key_dir: &Path,
    proof_path: &Path,
    retry_path: &Path,
    role: &'static str,
    enabled: bool,
    force: bool,
) -> Response {
    let status = crate::wfb_pair_state::status(config_path, key_dir, role);
    // The response reports the REQUESTED flag, not the stored one.
    let mut body = Value::Object(status.to_json());
    body["auto_pair_enabled"] = json!(enabled);

    // Re-arm on a paired rig is refused: the status snapshot with auto_pair_enabled
    // forced false and rearm_blocked added. NOTHING is persisted, so nothing is
    // applied either.
    if enabled && status.paired && !force {
        body["auto_pair_enabled"] = json!(false);
        body["rearm_blocked"] = json!(true);
        body["applied"] = json!(false);
        return Json(body).into_response();
    }

    let forced = enabled && status.paired;
    // A forced re-arm the supervisor's latch will not act on is refused before
    // anything is written: recording it would leave a one-shot that fires at an
    // unplanned moment when the latch is next switched on.
    if forced && !rearm_latch_enabled(config_path) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail": {
                "error": "rearm_latch_disabled",
                "message": "The re-arm latch is switched off (video.wfb.pair_rearm.enabled), so a forced re-arm cannot open a bind window.",
            }})),
        )
            .into_response();
    }

    if let Err(e) = persist_auto_pair_flag(config_path, enabled) {
        tracing::warn!(error = %e, "auto_pair_flag_persist_failed");
        return not_applied("config_write_failed", e.to_string());
    }

    if forced {
        // The forced re-arm: record the one-shot and never touch the key. Only
        // meaningful on a paired rig; a forced request on an unpaired one is just
        // an ordinary arm, which the retry request below already covers. A paired
        // status always carries the fingerprint it was proven with.
        let Some(fp) = status.fingerprint.as_str() else {
            return not_applied(
                "rearm_record_failed",
                "the paired key has no readable fingerprint".to_string(),
            );
        };
        if let Err(e) = record_force_rearm(proof_path, role, fp) {
            tracing::warn!(error = %e, "force_rearm_record_failed");
            return not_applied("rearm_record_failed", e.to_string());
        }
        body["rearm_blocked"] = json!(false);
        body["forced"] = json!(true);
    } else if enabled {
        if let Err(e) = ados_protocol::pair_proof::request_local_retry_at(retry_path) {
            return not_applied("retry_request_failed", e.to_string());
        }
        body["retry_requested"] = json!(true);
    }
    body["applied"] = json!(true);
    Json(body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wfb_pair_state::read_public_fingerprint;

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// A valid 64-byte key file whose fingerprint is computable; returns the
    /// fingerprint the route would report.
    fn write_key(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        let mut bytes = vec![0u8; 64];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        std::fs::write(&path, &bytes).unwrap();
        read_public_fingerprint(&path).unwrap()
    }

    // ── the disable path (enabled=false): always persists ─────────────────────

    #[tokio::test]
    async fn disable_on_an_unpaired_drone_persists_false_and_echoes_status() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        // A drone with no key file, an existing auto-pair flag + an unrelated key.
        std::fs::write(
            &cfg,
            "agent:\n  name: my-drone\nvideo:\n  wfb:\n    channel: 149\n    auto_pair_enabled: true\n",
        )
        .unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", false, false);
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "paired": false,
                "paired_with_device_id": null,
                "paired_at": null,
                "fingerprint": null,
                "auto_pair_enabled": false,
                "role": "drone",
                "applied": true,
            })
        );

        // The flag landed in video.wfb; the unrelated channel + agent.name survived.
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(
            wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(wfb.get("channel").and_then(|v| v.as_i64()), Some(149));
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("my-drone")
        );
    }

    #[tokio::test]
    async fn enable_on_an_unpaired_drone_persists_true() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        // Unpaired (no key) → enable is allowed and persists true.
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, false);
        let body = body_json(resp).await;
        assert_eq!(body["auto_pair_enabled"], json!(true));
        assert!(body.get("rearm_blocked").is_none());
        // Arming an unpaired rig asks the supervisor to retry the local bind, so a
        // loop parked on the cloud relay actually comes back.
        assert_eq!(body["retry_requested"], json!(true));
        assert_eq!(body["applied"], json!(true));
        assert!(retry.exists());
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("video")
                .and_then(|v| v.get("wfb"))
                .and_then(|w| w.get("auto_pair_enabled"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    // ── the re-arm-blocked path (enabled=true on a paired rig): no persist ────

    #[tokio::test]
    async fn rearm_on_a_paired_drone_is_blocked_and_does_not_persist() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        let fp = write_key(&keys, "tx.key");
        // A paired drone (tx.key present) with a peer + a disarmed flag on disk.
        std::fs::write(
            &cfg,
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: false\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&cfg).unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, false);
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "paired": true,
                "paired_with_device_id": "peer-xyz",
                "paired_at": null,
                "fingerprint": fp,
                "auto_pair_enabled": false,
                "rearm_blocked": true,
                "role": "drone",
                "applied": false,
            })
        );
        // The file is unchanged — the refuse path persists nothing.
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
        // And nothing was recorded against the key.
        assert!(!proof.exists());
        assert!(!retry.exists(), "a refused re-arm must not request a retry");
    }

    #[tokio::test]
    async fn an_unparseable_config_is_a_500_and_is_left_untouched() {
        // A duplicate key a hand edit left behind: the write must refuse rather
        // than replace the operator's whole document with three pair fields, and
        // the route must not claim success.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        let original = "agent:\n  profile: drone\nvideo:\n  a: 1\nvideo:\n  b: 2\n";
        std::fs::write(&cfg, original).unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, false);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(resp).await;
        assert_eq!(body["detail"]["error"], json!("config_write_failed"));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
        assert!(!retry.exists());
    }

    #[tokio::test]
    async fn a_retry_request_the_supervisor_cannot_see_is_a_500() {
        // The request path's parent is a file, so the request cannot be written.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(dir.path().join("blocker"), "x").unwrap();
        let retry = dir.path().join("blocker").join("auto-pair-retry.request");
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, false);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(resp).await["detail"]["error"],
            json!("retry_request_failed")
        );
    }

    // ── the forced re-arm: granted, one-shot, and the key is never touched ────

    #[tokio::test]
    async fn a_forced_rearm_on_a_paired_drone_is_granted_and_leaves_the_key_alone() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        let fp = write_key(&keys, "tx.key");
        let key_before = std::fs::read(keys.join("tx.key")).unwrap();
        std::fs::write(
            &cfg,
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: false\n",
        )
        .unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, true);
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["rearm_blocked"], json!(false));
        assert_eq!(body["forced"], json!(true));
        assert_eq!(body["auto_pair_enabled"], json!(true));
        assert_eq!(body["fingerprint"], json!(fp));

        // The one-shot is recorded against the key's OWN fingerprint, so a key
        // replaced before the supervisor's next tick discards it rather than
        // firing at whatever key happens to be there.
        let stored = ados_protocol::pair_proof::read_pair_proof_from(&proof).unwrap();
        assert!(stored.force_rearm);
        assert_eq!(stored.key_fingerprint, fp);
        assert_eq!(stored.role, "drone");

        // The whole point: the key is still there. Unpairing first would have
        // deleted it, which is the worst move if the key turns out to be fine.
        assert_eq!(std::fs::read(keys.join("tx.key")).unwrap(), key_before);

        // And the arm flag is persisted, so the supervisor sees a live request.
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("video")
                .and_then(|v| v.get("wfb"))
                .and_then(|w| w.get("auto_pair_enabled"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn a_forced_rearm_preserves_what_is_already_known_about_the_key() {
        // Forcing one window is not a reason to forget the key's proof or the
        // episodes already spent on it.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        let fp = write_key(&keys, "tx.key");
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

        let mut existing = ados_protocol::pair_proof::PairProof::fresh("drone", &fp);
        existing.mark_proven(1_700_000_000);
        existing.record_rearm(1_700_000_100);
        ados_protocol::pair_proof::write_pair_proof_to(&proof, &existing).unwrap();

        let _ = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, true);
        let stored = ados_protocol::pair_proof::read_pair_proof_from(&proof).unwrap();
        assert!(stored.force_rearm);
        assert_eq!(stored.proven_at, existing.proven_at);
        assert_eq!(stored.rearm_episodes, existing.rearm_episodes);
    }

    #[tokio::test]
    async fn force_on_an_unpaired_rig_is_an_ordinary_arm() {
        // There is nothing to override and no key to key a one-shot to, so the
        // request must not manufacture a record.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

        let body = body_json(put_auto_pair_at(
            &cfg, &keys, &proof, &retry, "drone", true, true,
        ))
        .await;
        assert_eq!(body["auto_pair_enabled"], json!(true));
        assert!(body.get("forced").is_none());
        assert!(body.get("rearm_blocked").is_none());
        assert!(!proof.exists());
        assert!(retry.exists());
    }

    #[tokio::test]
    async fn force_is_ignored_when_disabling() {
        // `enabled: false` is a disarm; force has nothing to grant.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        write_key(&keys, "tx.key");
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

        let body = body_json(put_auto_pair_at(
            &cfg, &keys, &proof, &retry, "drone", false, true,
        ))
        .await;
        assert_eq!(body["auto_pair_enabled"], json!(false));
        assert!(body.get("forced").is_none());
        assert!(!proof.exists(), "a disarm must not record a one-shot");
        assert!(!retry.exists(), "a disarm must not request a retry");
    }

    #[test]
    fn the_force_field_defaults_off_so_an_existing_client_is_unchanged() {
        // The GCS pairing card sends `{"enabled": ...}` and must keep getting the
        // refusal, not a silent re-bind of a working pair.
        let req: AutoPairToggleRequest = serde_json::from_str(r#"{"enabled": true}"#).unwrap();
        assert!(req.enabled);
        assert!(!req.force);
        let req: AutoPairToggleRequest =
            serde_json::from_str(r#"{"enabled": true, "force": true}"#).unwrap();
        assert!(req.force);
    }

    #[tokio::test]
    async fn disable_on_a_paired_drone_is_allowed_and_persists() {
        // enabled=false is never blocked, even when paired.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        write_key(&keys, "tx.key");
        std::fs::write(
            &cfg,
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: true\n",
        )
        .unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", false, false);
        let body = body_json(resp).await;
        assert_eq!(body["auto_pair_enabled"], json!(false));
        assert!(body.get("rearm_blocked").is_none());
        // The peer survives the persist (status read it, persist wrote it back).
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(
            wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            wfb.get("paired_with_device_id").and_then(|v| v.as_str()),
            Some("peer-xyz")
        );
    }

    // ── the toggle writes the arm flag and nothing else ───────────────────────

    /// The bind stores `paired_at` as an unquoted ISO timestamp, which the
    /// status projection reads as null. The toggle must leave the record the
    /// bind wrote exactly as it was, on both profiles, rather than write that
    /// lossy projection back over it.
    #[tokio::test]
    async fn the_toggle_leaves_the_bind_record_untouched() {
        for (profile, role) in [("drone", "drone"), ("ground_station", "gs")] {
            let dir = tempfile::tempdir().unwrap();
            let cfg = dir.path().join("config.yaml");
            let keys = dir.path().join("wfb");
            let proof = dir.path().join("pair-proof.json");
            let retry = dir.path().join("auto-pair-retry.request");
            std::fs::create_dir_all(&keys).unwrap();
            std::fs::write(
                &cfg,
                format!(
                    "agent:\n  profile: {profile}\nvideo:\n  wfb:\n    paired_with_device_id: peer-1\n    paired_at: 2026-05-29T12:34:56+00:00\n    auto_pair_enabled: true\n"
                ),
            )
            .unwrap();
            let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, role, false, false);
            assert_eq!(resp.status(), StatusCode::OK);
            let parsed: serde_norway::Value =
                serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
            let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
            assert_eq!(
                wfb.get("paired_at").and_then(|v| v.as_str()),
                Some("2026-05-29T12:34:56+00:00"),
                "{role}: paired_at survives the toggle"
            );
            assert_eq!(
                wfb.get("paired_with_device_id").and_then(|v| v.as_str()),
                Some("peer-1")
            );
            assert_eq!(
                wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
                Some(false)
            );
            assert!(
                parsed.get("ground_station").is_none(),
                "{role}: no legacy mirror is written by the toggle"
            );
        }
    }

    /// A forced re-arm with the latch switched off is refused and records
    /// nothing, so no one-shot is left to fire when the latch is re-enabled.
    #[tokio::test]
    async fn a_forced_rearm_with_the_latch_off_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        write_key(&keys, "tx.key");
        let original = "agent:\n  profile: drone\nvideo:\n  wfb:\n    auto_pair_enabled: false\n    pair_rearm:\n      enabled: false\n";
        std::fs::write(&cfg, original).unwrap();
        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", true, true);
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            body_json(resp).await["detail"]["error"],
            json!("rearm_latch_disabled")
        );
        assert!(!proof.exists(), "no one-shot recorded");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
    }

    #[tokio::test]
    async fn gs_toggle_persists_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        // A GS with a peer recorded under the canonical spot, unpaired (no rx.key),
        // so the enable persist runs (not blocked).
        std::fs::write(
            &cfg,
            "agent:\n  profile: ground_station\nvideo:\n  wfb:\n    paired_with_device_id: drone-1\n    auto_pair_enabled: false\n",
        )
        .unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "gs", true, false);
        let body = body_json(resp).await;
        assert_eq!(body["auto_pair_enabled"], json!(true));
        assert_eq!(body["role"], json!("gs"));
        assert_eq!(body["paired_with_device_id"], json!("drone-1"));

        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert!(parsed.get("ground_station").is_none());
        assert_eq!(
            parsed
                .get("video")
                .and_then(|v| v.get("wfb"))
                .and_then(|w| w.get("auto_pair_enabled"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn disable_with_no_peer_writes_only_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        let proof = dir.path().join("pair-proof.json");
        let retry = dir.path().join("auto-pair-retry.request");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();
        let resp = put_auto_pair_at(&cfg, &keys, &proof, &retry, "drone", false, false);
        let body = body_json(resp).await;
        assert_eq!(body["paired_with_device_id"], Value::Null);
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let wfb = parsed.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert!(wfb.get("paired_with_device_id").is_none());
        assert!(wfb.get("paired_at").is_none());
        assert_eq!(
            wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
    }
}
