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
//!   auto-pair loop consumes: the request resets the loop's failover signal to
//!   local and restarts its attempt count, which a config flag that was already
//!   `true` would never do.
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
/// auto-pair loop consumes, which resets its failover signal to local and
/// restarts its attempt count (`retry_requested: true`).
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
fn record_force_rearm(
    proof_path: &Path,
    role: &str,
    fingerprint: &str,
    armed: bool,
) -> std::io::Result<()> {
    let mut proof = ados_protocol::pair_proof::load_for(proof_path, role, fingerprint).proof;
    proof.force_rearm = armed;
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

    // The forced re-arm records its one-shot BEFORE the arm flag is persisted, so
    // a failure on either write leaves the rig as it was: a recording failure
    // writes nothing, and a flag failure withdraws the one-shot it just recorded.
    // The re-arm never touches the key. It is only meaningful on a paired rig; a
    // forced request on an unpaired one is just an ordinary arm, which the retry
    // request below already covers. A paired status always carries the
    // fingerprint it was proven with.
    let rearm_fp = if forced {
        let Some(fp) = status.fingerprint.as_str() else {
            return not_applied(
                "rearm_record_failed",
                "the paired key has no readable fingerprint".to_string(),
            );
        };
        if let Err(e) = record_force_rearm(proof_path, role, fp, true) {
            tracing::warn!(error = %e, "force_rearm_record_failed");
            return not_applied("rearm_record_failed", e.to_string());
        }
        Some(fp)
    } else {
        None
    };

    if let Err(e) = persist_auto_pair_flag(config_path, enabled) {
        tracing::warn!(error = %e, "auto_pair_flag_persist_failed");
        if let Some(fp) = rearm_fp {
            if let Err(e) = record_force_rearm(proof_path, role, fp, false) {
                tracing::warn!(error = %e, "force_rearm_withdraw_failed");
            }
        }
        return not_applied("config_write_failed", e.to_string());
    }

    if rearm_fp.is_some() {
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
mod tests;
