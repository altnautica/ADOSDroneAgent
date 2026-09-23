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
//!   off the live state snapshot. Every optional read is fault-tolerant. The one
//!   exception is `pairing.json` itself: an unreadable or malformed file is a
//!   `503`, never the unpaired default, because "unpaired" is the state in which
//!   anyone on the LAN may claim a fresh key.
//! - **`GET /api/pairing/code`** — the bare code while unpaired; 409 when paired.
//! - **`POST /api/pairing/claim`** — claim the agent for a user. Writes
//!   `pairing.json` (mirroring `PairingManager.claim` exactly) and returns the
//!   key; 409 when already paired; 503 when the pairing file cannot be read.
//! - **`POST /api/pairing/unpair`** — clear pairing + mint a fresh code; 409 when
//!   not paired. Gated by the auth middleware (it is not in the public set). An
//!   unreadable pairing file is cleared too: the gate already restricts it to the
//!   on-box operator, and this is how that operator recovers the node.
//!
//! The pairing code is withheld from a remote caller (anything relayed through a
//! proxy or tunnel, or a public-WAN host): `info` reports it as null and `code`
//! refuses. It is a claim credential for the device's own networks only.
//!
//! `mdns_host` is the RESOLVABLE reach name — the system hostname avahi
//! publishes (`<hostname>.local`, or the name verbatim when it already carries
//! a domain), resolved through [`ados_protocol::reach::mdns_hostname`], the same
//! rule `DiscoveryService.mdns_hostname` applies on the Python side. It is
//! deliberately NOT a constructed `ados-<6hex>.local`: nothing publishes an
//! A-record for that name, so a GCS that stores it as a node's canonical reach
//! stores a name that resolves nowhere. A host with no usable hostname has no
//! mDNS reach at all and the field is emitted as `""` rather than as a name this
//! node cannot prove. The `_ados._tcp` advert this daemon publishes at boot
//! (`crate::mdns`) uses the identical name as its SRV target, so the browse
//! record and the probe response name one host.

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use ados_protocol::pairing_posture::CallerClass;

use crate::config::PairingConfig;
use crate::pairing_store::{self, PairingDoc};
use crate::profile::current_profile_and_role_at;
use crate::routes::detail;
use crate::state::{AppState, PairingPaths};

/// The `503` for a pairing file that exists but cannot be read or parsed. The
/// node refuses to act as unpaired (which would open the claim) and names the
/// recovery.
fn pairing_unreadable(reason: &str) -> Response {
    tracing::error!(reason, "pairing_state_unreadable");
    detail(
        StatusCode::SERVICE_UNAVAILABLE,
        "The pairing state on this device is unreadable. Unpair it on the device itself to recover.",
    )
}

/// Whether the caller may see the pairing code: anyone on the device's own
/// networks, never a remote caller. A request with no caller class (never
/// produced by the edges) is treated as remote.
fn may_see_code(caller: Option<Extension<CallerClass>>) -> bool {
    !matches!(
        caller.map_or(CallerClass::Remote, |Extension(c)| c),
        CallerClass::Remote
    )
}

/// `GET /api/pairing/info` → the 19-field node-identity probe.
///
/// Doubles as the Mission Control "probe" endpoint when an operator pastes a
/// hostname into Add-a-Node. Every field is emitted even when null (the GCS keys
/// off exact field presence), so `bind_state` and `radio` serialize as JSON
/// `null`, never omitted. Each underlying read is guarded so a partially
/// configured agent answers 200 with a usable shape rather than 500.
pub async fn get_pairing_info(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
) -> Response {
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
    let (profile, role) =
        current_profile_and_role_at(&cfg.agent.profile, &paths.profile_conf, &paths.mesh_role);
    let radio_peer_device_id = cfg.radio_peer_device_id();

    // The name this host actually answers to. Empty when the host has no
    // usable hostname: the GCS falls back to the IPv4 it just reached us on,
    // which is a proven reach, where a constructed name is not.
    let mdns_host = ados_protocol::reach::mdns_hostname().unwrap_or_default();

    // Cloud-pair state off pairing.json. Absent is unpaired; unreadable is not.
    let doc = match PairingDoc::read(&paths.pairing_json) {
        Ok(doc) => doc,
        Err(reason) => return pairing_unreadable(&reason),
    };

    // Radio-pair signal: the same predicate `GET /api/wfb/pair` answers from —
    // this role's own key file, exactly 64 bytes, with a readable fingerprint.
    let radio_paired = crate::routes::wfb::paired_key_fingerprint(
        &paths.wfb_key_dir,
        crate::routes::wfb::bind_role_for(&profile),
    )
    .is_some();

    // The folded bind-session snapshot from the cross-process sentinel.
    let bind_state = read_bind_state(paths);

    // FC presence from the live state snapshot's runtime extras.
    let (fc_connected, fc_port, fc_baud) = fc_from_snapshot(state.state.snapshot().as_ref());

    Json(json!({
        "device_id": device_id,
        "name": name,
        "version": state.agent_version(),
        // Board is HAL-detected at runtime in the Python (the FastAPI route reads
        // `app.board_name`); the native surface has no in-process HAL-detect port,
        // so it reads the `name` field live off the board sidecar
        // (`/run/ados/board.json`) the detector persists — the same on-disk source
        // the status route's board block reads. Defaults to "unknown" when the
        // sidecar is absent (a fresh boot before the first status write).
        "board": crate::state::board_name(&state.board_path),
        "paired": doc.is_paired(),
        "radio_paired": radio_paired,
        "radio_peer_device_id": radio_peer_device_id,
        "pairing_code": if may_see_code(caller) { doc.info_pairing_code() } else { None },
        "owner_id": doc.info_owner_id(),
        "paired_at": doc.info_paired_at(),
        "mdns_host": mdns_host,
        "profile": profile,
        "role": role,
        // Native-vs-packaged badge. The native surface has no in-process port of
        // the Python compute_runtime_mode(profile), so the Python API writes the
        // computed value to the `runtime-mode` sidecar at startup and this reads it
        // live (then the ADOS_RUNTIME_MODE env, then "packaged"). Defaults to
        // "packaged" when neither is present, the correct value for any agent that
        // has not cut over.
        "runtime_mode": crate::state::runtime_mode(),
        "bind_state": bind_state,
        // Reserved for a future in-process radio reader; null today, exactly as
        // the FastAPI route emits (the GCS falls back to radio_paired).
        "radio": Value::Null,
        "fc_connected": fc_connected,
        "fc_port": fc_port,
        "fc_baud": fc_baud,
    }))
    .into_response()
}

/// `GET /api/pairing/code` → `{"code": <code>}` while unpaired; 409
/// `{"detail":"Already paired"}` while paired.
///
/// The FastAPI route generates a code on demand (`get_or_create_code`); the
/// native read surface returns the persisted code when one is present, and mints
/// then persists one when absent so a fresh agent still answers a usable code
/// (the same effect `get_or_create_code` has). Paired agents 409.
pub async fn get_pairing_code(
    State(state): State<AppState>,
    caller: Option<Extension<CallerClass>>,
) -> Response {
    if !may_see_code(caller) {
        return detail(
            StatusCode::FORBIDDEN,
            "The pairing code is only served on the device's own networks.",
        );
    }
    let paths = &state.pairing_paths;
    let doc = match PairingDoc::read(&paths.pairing_json) {
        Ok(doc) => doc,
        Err(reason) => return pairing_unreadable(&reason),
    };
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
/// Writes `pairing.json` (mirroring `PairingManager.claim` exactly: atomic,
/// 0600, the four Python keys, code + pending key dropped) and returns the key.
/// No credential required and only works while unpaired. Being on the device's
/// own networks is the gate: the edge refuses a remote caller (a public-WAN
/// host, or anything relayed through a proxy or tunnel) before this handler
/// runs — see `auth::unpaired_decision`.
pub async fn claim_pairing(
    State(state): State<AppState>,
    Json(req): Json<ClaimRequest>,
) -> Response {
    let paths = &state.pairing_paths;
    let doc = match PairingDoc::read(&paths.pairing_json) {
        Ok(doc) => doc,
        Err(reason) => return pairing_unreadable(&reason),
    };
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
        Err(pairing_store::ClaimError::Unreadable(reason)) => {
            return pairing_unreadable(&reason);
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
    // fallback — that fallback is the /info route's, not the claim's).
    let name = cfg.agent.name.clone();
    // The GCS persists this as the node's canonical reach and six consumers
    // prefer it over the IPv4 they just proved. So it must be the name this
    // host answers to, identical to the one `/api/pairing/info` reported and
    // the one the `_ados._tcp` advert targets.
    let mdns_host = ados_protocol::reach::mdns_hostname().unwrap_or_default();

    // The `_ados._tcp` advert's `paired` TXT is refreshed by the same daemon
    // that publishes it (`crate::mdns`), which re-reads pairing.json on a fixed
    // cadence rather than being poked from here — a claim that lands while the
    // advert thread is mid-publish must not be able to wedge the write path
    // the operator is waiting on.

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
    // An unreadable file is cleared as well: only the on-box operator reaches
    // this while it is unreadable (no key can match), and it is their recovery.
    if let Ok(doc) = PairingDoc::read(&paths.pairing_json) {
        if !doc.is_paired() {
            return detail(StatusCode::CONFLICT, "Not paired");
        }
    }

    if let Err(e) = pairing_store::unpair(&paths.pairing_json, &paths.relay_secret) {
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
    // Best-effort schema-drift signal (never reject): warn when the sentinel was
    // written by an agent with a different schema version, then read anyway. The
    // writer const lives in the supervisor crate, so compare against the shared
    // registry.
    let got = obj.get("version").and_then(Value::as_u64).unwrap_or(0) as u16;
    if let Some(ours) = ados_protocol::contracts::sidecar_version("bind-state") {
        ados_protocol::sidecar::check_sidecar_version("bind-state", got, ours);
    }
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

    /// `radio_paired` answers from the shared predicate: this role's own key,
    /// exactly 64 bytes. A truncated key, or a stale key of the other role, is
    /// not a radio pairing.
    #[test]
    fn radio_paired_is_the_roles_own_complete_key() {
        use crate::routes::wfb::paired_key_fingerprint;
        let dir = tempfile::tempdir().unwrap();
        let key_dir = dir.path().join("wfb");
        std::fs::create_dir_all(&key_dir).unwrap();
        assert!(
            paired_key_fingerprint(&key_dir, "drone").is_none(),
            "no key"
        );
        std::fs::write(key_dir.join("tx.key"), b"x").unwrap();
        assert!(
            paired_key_fingerprint(&key_dir, "drone").is_none(),
            "a truncated tx.key is not a pairing"
        );
        std::fs::write(key_dir.join("rx.key"), [7u8; 64]).unwrap();
        assert!(
            paired_key_fingerprint(&key_dir, "drone").is_none(),
            "a drone holding only a ground-station rx.key is not paired"
        );
        assert!(paired_key_fingerprint(&key_dir, "gs").is_some());
        std::fs::write(key_dir.join("tx.key"), [7u8; 64]).unwrap();
        assert!(paired_key_fingerprint(&key_dir, "drone").is_some());
    }

    fn test_paths(dir: &std::path::Path) -> PairingPaths {
        PairingPaths {
            config: dir.join("config.yaml"),
            pairing_json: dir.join("pairing.json"),
            wfb_key_dir: dir.join("wfb"),
            bind_state: dir.join("bind-state.json"),
            profile_conf: dir.join("profile.conf"),
            mesh_role: dir.join("mesh-role"),
            relay_secret: dir.join("relay-peer-secret"),
        }
    }
}
