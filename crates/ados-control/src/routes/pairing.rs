//! Pairing routes: the LAN node-identity probe + the local pairing handshake.
//!
//! These are the parity-critical routes: the GCS Add-a-Node flow probes
//! `/api/pairing/info`, then POSTs `/api/pairing/claim`, and stores the returned
//! key. The native surface must answer these byte-identically to the FastAPI
//! surface, down to every field name and the null-as-null shape, or pairing
//! breaks silently.
//!
//! - **`GET /api/pairing/info`** — the node-identity probe. Emits all 19 fields
//!   even when null (no field is ever omitted), reading device identity +
//!   profile off `/etc/ados/config.yaml`, the cloud-pair state off
//!   `pairing.json`, the radio-pair signal off the `/etc/ados/wfb` key files, the
//!   bind session off the `/run/ados/bind-state.json` sentinel, and the FC triple
//!   off the live state snapshot. Every read is fault-tolerant; the route is
//!   guaranteed 200, never 500.
//! - **`GET /api/pairing/code`** — the bare code while unpaired; 409 when paired.
//! - **`POST /api/pairing/claim`** — claim the agent for a user. Writes
//!   `pairing.json` (mirroring `PairingManager.claim` exactly) and returns the
//!   key; 409 when already paired.
//! - **`POST /api/pairing/unpair`** — clear pairing + mint a fresh code; 409 when
//!   not paired. Gated by the auth middleware (it is not in the public set).
//!
//! The mDNS hostname is computed as `ados-{device_id[:6].lower()}.local`, the
//! same format the FastAPI route falls back to. When a live discovery service is
//! running, the FastAPI route may override `mdns_host` with the live mDNS
//! hostname and update the mDNS TXT records on claim/unpair; the native surface
//! has no in-process discovery reader, so it uses the computed format and does
//! NOT touch mDNS. See the module note on `mdns_host` for that deliberate gap.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::config::PairingConfig;
use crate::pairing_store::{self, PairingDoc};
use crate::profile::current_profile_and_role;
use crate::routes::detail;
use crate::state::{AppState, PairingPaths};

/// `GET /api/pairing/info` → the 19-field node-identity probe.
///
/// Doubles as the Mission Control "probe" endpoint when an operator pastes a
/// hostname into Add-a-Node. Every field is emitted even when null (the GCS keys
/// off exact field presence), so `bind_state` and `radio` serialize as JSON
/// `null`, never omitted. Each underlying read is guarded so a partially
/// configured agent answers 200 with a usable shape rather than 500.
pub async fn get_pairing_info(State(state): State<AppState>) -> Json<Value> {
    let paths = &state.pairing_paths;

    // Device identity + profile, read live off the config (mirroring the FastAPI
    // route's read of the live runtime config).
    let cfg = PairingConfig::load_from(&paths.config);
    let device_id = cfg.agent.device_id.clone();
    // The FastAPI route falls back to "ADOS Agent" when the config name is empty
    // (`name or "ADOS Agent"`); the config default is "my-drone", so a configured
    // agent carries a real name here.
    let name = if cfg.agent.name.is_empty() {
        "ADOS Agent".to_string()
    } else {
        cfg.agent.name.clone()
    };
    let (profile, role) = current_profile_and_role(&cfg.agent.profile);
    let radio_peer_device_id = cfg.radio_peer_device_id();

    let short_id = short_device_id(&device_id);
    let mdns_host = format!("ados-{short_id}.local");

    // Cloud-pair state off pairing.json.
    let doc = PairingDoc::load(&paths.pairing_json);

    // Radio-pair signal: the presence of a wfb key file. Owned by the wfb service
    // (a separate process), so read directly off disk, the same as the FastAPI
    // route's `key_exists()` call.
    let radio_paired = wfb_key_exists(paths);

    // The folded bind-session snapshot from the cross-process sentinel.
    let bind_state = read_bind_state(paths);

    // FC presence from the live state snapshot's runtime extras.
    let (fc_connected, fc_port, fc_baud) = fc_from_snapshot(state.state.snapshot().as_ref());

    Json(json!({
        "device_id": device_id,
        "name": name,
        "version": state.agent_version,
        // Board is HAL-detected at runtime in the Python (the FastAPI route reads
        // `app.board_name`); the native surface has no in-process HAL-detect port,
        // so it reads ADOS_BOARD_NAME, which the systemd unit injects from the
        // Python detect_board(). Defaults to "unknown" when the env is unset (this
        // surface is pre-cutover/inert, with no unit injecting it yet).
        "board": crate::state::board_name(),
        "paired": doc.is_paired(),
        "radio_paired": radio_paired,
        "radio_peer_device_id": radio_peer_device_id,
        "pairing_code": doc.info_pairing_code(),
        "owner_id": doc.info_owner_id(),
        "paired_at": doc.info_paired_at(),
        "mdns_host": mdns_host,
        "profile": profile,
        "role": role,
        // Native-vs-packaged badge. The native runtime_mode resolver is not ported
        // here; the route reads ADOS_RUNTIME_MODE, which the systemd unit injects
        // from the Python compute_runtime_mode(profile). Defaults to "packaged"
        // when the env is unset, which is the correct value for any agent that has
        // not cut over (and this surface is itself pre-cutover/inert).
        "runtime_mode": crate::state::runtime_mode(),
        "bind_state": bind_state,
        // Reserved for a future in-process radio reader; null today, exactly as
        // the FastAPI route emits (the GCS falls back to radio_paired).
        "radio": Value::Null,
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "fc_baud": fc_baud,
    }))
}

/// `GET /api/pairing/code` → `{"code": <code>}` while unpaired; 409
/// `{"detail":"Already paired"}` while paired.
///
/// The FastAPI route generates a code on demand (`get_or_create_code`); the
/// native read surface returns the persisted code when one is present, and mints
/// then persists one when absent so a fresh agent still answers a usable code
/// (the same effect `get_or_create_code` has). Paired agents 409.
pub async fn get_pairing_code(State(state): State<AppState>) -> Response {
    let paths = &state.pairing_paths;
    let doc = PairingDoc::load(&paths.pairing_json);
    if doc.is_paired() {
        return detail(StatusCode::CONFLICT, "Already paired");
    }
    let code = match doc.pairing_code.clone() {
        Some(code) if !code.is_empty() => code,
        // No code on file yet: mint + persist one, matching the
        // `get_or_create_code` generate-and-save branch.
        _ => match pairing_store::write_new_code(&paths.pairing_json, now_unix_seconds()) {
            Ok(code) => code,
            Err(e) => {
                tracing::warn!(error = %e, "pairing code persist failed");
                // Fall back to an in-memory code so the route still answers; the
                // FastAPI route persists, but a 200 with a usable code beats a
                // 500 on this probe-adjacent route. A getrandom failure here fails
                // closed to a 500 rather than a predictable code.
                match pairing_store::generate_code() {
                    Ok(code) => code,
                    Err(gen_err) => {
                        tracing::error!(error = %gen_err, "pairing code mint failed");
                        return detail(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "Failed to mint pairing code",
                        );
                    }
                }
            }
        },
    };
    (StatusCode::OK, Json(json!({ "code": code }))).into_response()
}

/// The `POST /api/pairing/claim` request body. Mirrors the FastAPI
/// `ClaimRequest`: a single `user_id` string.
#[derive(serde::Deserialize)]
pub struct ClaimRequest {
    pub user_id: String,
}

/// `POST /api/pairing/claim` → `{api_key, device_id, name, mdns_host}` (all
/// strings); 409 `{"detail":"Already paired. Unpair first."}` when already
/// paired.
///
/// Writes `pairing.json` mirroring `PairingManager.claim` exactly (atomic, 0600,
/// the four Python keys, code + pending key dropped) and returns the key. No
/// auth required, only works while unpaired — being on the LAN is the gate
/// (`is_public` lists this path), the same posture the FastAPI claim takes.
pub async fn claim_pairing(
    State(state): State<AppState>,
    Json(req): Json<ClaimRequest>,
) -> Response {
    let paths = &state.pairing_paths;
    let doc = PairingDoc::load(&paths.pairing_json);
    if doc.is_paired() {
        return detail(StatusCode::CONFLICT, "Already paired. Unpair first.");
    }

    let outcome = match pairing_store::claim(&paths.pairing_json, &req.user_id, now_unix_seconds())
    {
        Ok(o) => o,
        // Fail closed: a getrandom failure while minting a fresh key 500s rather
        // than emitting a predictable key (distinct message from the persist
        // failure so the logs tell an entropy fault from a disk fault).
        Err(pairing_store::ClaimError::KeyGen(e)) => {
            tracing::error!(error = %e, "pairing claim key mint failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to mint pairing key",
            );
        }
        Err(pairing_store::ClaimError::Persist(e)) => {
            tracing::error!(error = %e, "pairing claim persist failed");
            return detail(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to persist pairing: {e}"),
            );
        }
    };

    let cfg = PairingConfig::load_from(&paths.config);
    let device_id = cfg.agent.device_id.clone();
    // The FastAPI claim emits `app.config.agent.name` RAW (no "ADOS Agent"
    // fallback — that fallback is the /info route's, not the claim's), and builds
    // mdns from `device_id[:6].lower()` with NO "unknown" fallback (again unlike
    // /info). Mirror that exactly so the claim response is byte-identical.
    let name = cfg.agent.name.clone();
    let short_id: String = device_id.chars().take(6).collect::<String>().to_lowercase();
    let mdns_host = format!("ados-{short_id}.local");

    // mDNS TXT update is deferred: the FastAPI claim updates the discovery
    // service's TXT records here, but the native surface has no in-process
    // discovery reader. The load-bearing pairing.json write is done; the TXT
    // refresh is a deferred follow-up (see the module note).

    (
        StatusCode::OK,
        Json(json!({
            "api_key": outcome.api_key,
            "device_id": device_id,
            "name": name,
            "mdns_host": mdns_host,
        })),
    )
        .into_response()
}

/// `POST /api/pairing/unpair` → `{"status":"unpaired","new_code":<code>}`; 409
/// `{"detail":"Not paired"}` when not paired.
///
/// Clears `pairing.json` (mirroring `PairingManager.unpair` → empty object) and
/// mints a fresh pairing code. Requires a valid API key, enforced by the auth
/// middleware (this path is NOT in the public set), matching the FastAPI route.
pub async fn unpair(State(state): State<AppState>) -> Response {
    let paths = &state.pairing_paths;
    let doc = PairingDoc::load(&paths.pairing_json);
    if !doc.is_paired() {
        return detail(StatusCode::CONFLICT, "Not paired");
    }

    if let Err(e) = pairing_store::unpair(&paths.pairing_json) {
        tracing::error!(error = %e, "pairing unpair persist failed");
        return detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to clear pairing: {e}"),
        );
    }

    // Mint + persist the fresh code, mirroring the FastAPI route's
    // `get_or_create_code()` after the unpair. A persist failure still returns a
    // usable in-memory code rather than a 500.
    let new_code = match pairing_store::write_new_code(&paths.pairing_json, now_unix_seconds()) {
        Ok(code) => code,
        Err(e) => {
            tracing::warn!(error = %e, "new pairing code persist failed after unpair");
            // A getrandom failure fails closed to a 500 rather than a predictable
            // code; the pairing.json is already cleared, so a fresh probe mints a
            // code once entropy is back.
            match pairing_store::generate_code() {
                Ok(code) => code,
                Err(gen_err) => {
                    tracing::error!(error = %gen_err, "new pairing code mint failed after unpair");
                    return detail(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to mint pairing code",
                    );
                }
            }
        }
    };

    // mDNS TXT update deferred (same gap as claim).

    (
        StatusCode::OK,
        Json(json!({ "status": "unpaired", "new_code": new_code })),
    )
        .into_response()
}

// --- helpers ---

/// The truncated, lowercased device-id used in the mDNS hostname. Mirrors the
/// FastAPI `device_id[:6].lower() or "unknown"`: an empty device id yields
/// `"unknown"`.
fn short_device_id(device_id: &str) -> String {
    let short: String = device_id.chars().take(6).collect::<String>().to_lowercase();
    if short.is_empty() {
        "unknown".to_string()
    } else {
        short
    }
}

/// Whether a role-appropriate WFB key file is present, the `radio_paired` signal.
/// Mirrors `key_mgr.key_exists()` with no explicit role: either `tx.key` or
/// `rx.key` counts as paired (the bind protocol writes one side per rig).
fn wfb_key_exists(paths: &PairingPaths) -> bool {
    paths.wfb_key_dir.join("tx.key").is_file() || paths.wfb_key_dir.join("rx.key").is_file()
}

/// Fold the WFB bind-session snapshot from the cross-process sentinel. Absent
/// file (no bind has run) or a sentinel with no `state` → `null`. Each field is
/// read by key with a missing field tolerated, mirroring the FastAPI
/// `.get()`-guarded fold. Only the six fields the FastAPI route folds are
/// emitted, each as JSON null when absent in the sentinel.
fn read_bind_state(paths: &PairingPaths) -> Value {
    let Ok(text) = std::fs::read_to_string(&paths.bind_state) else {
        return Value::Null;
    };
    let Ok(sess) = serde_json::from_str::<Value>(&text) else {
        return Value::Null;
    };
    let Some(obj) = sess.as_object() else {
        return Value::Null;
    };
    // The FastAPI route only folds when `sess.get("state")` is truthy.
    let state_truthy = obj
        .get("state")
        .map(|v| !v.is_null() && v != &json!("") && v != &json!(false))
        .unwrap_or(false);
    if !state_truthy {
        return Value::Null;
    }
    json!({
        "state": obj.get("state").cloned().unwrap_or(Value::Null),
        "phase": obj.get("phase").cloned().unwrap_or(Value::Null),
        // `bool(sess.get("active", False))` → a missing/falsey active is false.
        "active": obj.get("active").and_then(Value::as_bool).unwrap_or(false),
        "error": obj.get("error").cloned().unwrap_or(Value::Null),
        "finished_at": obj.get("finished_at").cloned().unwrap_or(Value::Null),
        "fingerprint": obj.get("fingerprint").cloned().unwrap_or(Value::Null),
    })
}

/// Read the FC connection triple out of the live state snapshot's runtime extras.
/// Returns `(fc_connected, fc_port, fc_baud)` as JSON values. Mirrors the FastAPI
/// route's `fc_status()`: a connected FC reports a string port + int baud, an
/// absent / disconnected one reports `false` + JSON `null` + JSON `null` (the
/// pairing-info defaults are `None`, unlike the status route's `""`/`0`).
fn fc_from_snapshot(snapshot: Option<&Value>) -> (Value, Value, Value) {
    let obj = snapshot.and_then(Value::as_object);
    let connected = obj
        .and_then(|m| m.get("fc_connected"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // The FastAPI route reports `str(fc.port) if fc.port else None` and
    // `int(fc.baud) if fc.baud else None`: a missing, null, or falsey value → null.
    let port = obj
        .and_then(|m| m.get("fc_port"))
        .filter(|v| v.is_string() && !v.as_str().unwrap_or("").is_empty())
        .cloned()
        .unwrap_or(Value::Null);
    let baud = obj
        .and_then(|m| m.get("fc_baud"))
        .filter(|v| v.as_i64().map(|n| n != 0).unwrap_or(false))
        .cloned()
        .unwrap_or(Value::Null);
    (json!(connected), port, baud)
}

/// Wall-clock unix seconds (fractional), matching the Python `time.time()` the
/// claim/unpair/code writers stamp.
fn now_unix_seconds() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_device_id_truncates_lowercases_and_defaults() {
        assert_eq!(short_device_id("ABCDEF1234567890"), "abcdef");
        assert_eq!(short_device_id("Ab12"), "ab12");
        assert_eq!(short_device_id(""), "unknown");
    }

    #[test]
    fn fc_triple_is_disconnected_with_nulls_when_the_snapshot_is_absent() {
        let (c, p, b) = fc_from_snapshot(None);
        assert_eq!(c, json!(false));
        // Pairing-info uses null (not the status route's "" / 0).
        assert_eq!(p, Value::Null);
        assert_eq!(b, Value::Null);
    }

    #[test]
    fn fc_triple_reads_a_connected_snapshot() {
        let snap = json!({
            "fc_connected": true,
            "fc_port": "/dev/ttyACM0",
            "fc_baud": 115200,
        });
        let (c, p, b) = fc_from_snapshot(Some(&snap));
        assert_eq!(c, json!(true));
        assert_eq!(p, json!("/dev/ttyACM0"));
        assert_eq!(b, json!(115200));
    }

    #[test]
    fn fc_triple_treats_empty_port_and_zero_baud_as_null() {
        let snap = json!({
            "fc_connected": false,
            "fc_port": "",
            "fc_baud": 0,
        });
        let (c, p, b) = fc_from_snapshot(Some(&snap));
        assert_eq!(c, json!(false));
        assert_eq!(p, Value::Null);
        assert_eq!(b, Value::Null);
    }

    #[test]
    fn bind_state_is_null_for_an_absent_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        assert_eq!(read_bind_state(&paths), Value::Null);
    }

    #[test]
    fn bind_state_is_null_when_the_sentinel_has_no_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bind-state.json"), r#"{"phase":"x"}"#).unwrap();
        let paths = test_paths(dir.path());
        assert_eq!(read_bind_state(&paths), Value::Null);
    }

    #[test]
    fn bind_state_folds_the_six_fields_when_state_is_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bind-state.json"),
            r#"{"state":"binding","phase":"key_transfer","active":true,"error":null,"finished_at":123.0,"fingerprint":"ab"}"#,
        )
        .unwrap();
        let paths = test_paths(dir.path());
        let bs = read_bind_state(&paths);
        let obj = bs.as_object().expect("bind_state object");
        let keys: std::collections::BTreeSet<_> = obj.keys().cloned().collect();
        let want: std::collections::BTreeSet<_> = [
            "state",
            "phase",
            "active",
            "error",
            "finished_at",
            "fingerprint",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(keys, want, "bind_state folds exactly the six FastAPI keys");
        assert_eq!(bs["state"], json!("binding"));
        assert_eq!(bs["active"], json!(true));
        assert_eq!(bs["error"], Value::Null);
    }

    #[test]
    fn bind_state_missing_active_folds_to_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bind-state.json"), r#"{"state":"done"}"#).unwrap();
        let paths = test_paths(dir.path());
        let bs = read_bind_state(&paths);
        assert_eq!(bs["active"], json!(false));
        assert_eq!(bs["phase"], Value::Null);
    }

    #[test]
    fn wfb_key_presence_is_the_radio_paired_signal() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let key_dir = &paths.wfb_key_dir;
        std::fs::create_dir_all(key_dir).unwrap();
        assert!(!wfb_key_exists(&paths), "no key → not radio-paired");
        std::fs::write(key_dir.join("tx.key"), b"x").unwrap();
        assert!(wfb_key_exists(&paths), "tx.key → radio-paired");
        std::fs::remove_file(key_dir.join("tx.key")).unwrap();
        std::fs::write(key_dir.join("rx.key"), b"x").unwrap();
        assert!(wfb_key_exists(&paths), "rx.key → radio-paired");
    }

    fn test_paths(dir: &std::path::Path) -> PairingPaths {
        PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
        }
    }
}
