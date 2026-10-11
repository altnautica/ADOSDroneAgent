//! Stable-MAC pinning write routes: pin a MAC (POST) + unpin (DELETE).
//!
//! An onboard adapter with no efuse MAC randomizes its address each driver load,
//! churning the DHCP lease (and the box's IP) every boot. The agent pins a stable,
//! locally-administered MAC for such a chipset via a next-boot
//! `systemd-networkd` `.link` drop-in. These two routes let an operator confirm a
//! learner candidate, set an explicit override, or unpin:
//!
//! - **`POST /api/v1/network/mac/pin`** — pin a stable MAC on an adapter. The body
//!   is `{"iface", "mac"?, "apply_now"?}`. The interface name is resolved to the
//!   adapter's stable key (`vvvv:pppp@<usb_path>`) through the state file; an
//!   interface the state file does not know is `400 E_UNKNOWN_ADAPTER`, because a
//!   pin keyed on a name would follow whichever adapter holds that name. The MAC
//!   is resolved from `mac` or, when absent, the adapter's learner-proposed value
//!   in the on-disk state file. The resolved MAC is stored as a
//!   `network.mac_pin.overrides[<adapter key>]` entry and the config is
//!   persisted; the supervisor reconciler writes the actual `.link` on its next
//!   reconcile and on the next boot. With `apply_now` (and the config
//!   `apply_live_allowed` gate) it also re-tags the LIVE interface — refused on the
//!   management interface so it cannot drop the caller's own connection. Returns
//!   `{status, iface, adapterKey, mac, persisted, appliedLive, note}`.
//! - **`DELETE /api/v1/network/mac/{iface}`** — unpin: resolve the adapter key,
//!   clear its override entry and remove its `.link`. Returns `{status, iface,
//!   adapterKey, removedOverride, removedLinkFile, note}`.
//!
//! ## Why these port cleanly to the native front
//!
//! Unlike the radio/Wi-Fi writes, the MAC-pin write path has no in-process manager
//! and no command-socket seam: the FastAPI route writes a config override + (on
//! delete) a `.link` file directly, and the always-on supervisor reconciler is what
//! later turns an override into a `.link`. So the front does the identical two
//! things the FastAPI route does — an atomic YAML merge of the override into
//! `network.mac_pin.overrides` (the same merge the WFB tx-power write uses for its
//! key) and a `.link` removal — with no daemon round-trip. The `.link` removal +
//! the optional live re-tag reuse the shared `ados-macpin` engine, the same code
//! the installer step + the supervisor reconciler drive, so the bytes written /
//! removed are identical to what the rest of the system expects.
//!
//! ## Guard order + envelopes (matched to the FastAPI routes)
//!
//! Pin (POST):
//! 1. Resolve the MAC from the body, falling back to the state file's
//!    learner-proposed value for the named interface. Neither present → `400
//!    {"detail": {"error": {"code": "E_NO_MAC", ...}}}`.
//! 2. The MAC must match the 6-octet colon/dash form → `400 E_BAD_MAC` with
//!    `"malformed MAC: <mac>"`. The accepted MAC is normalised to lowercase colon
//!    form.
//! 3. The override is merged into the config and persisted. A persist fault is
//!    `500 E_PERSIST` with the I/O error text.
//! 4. The success body carries `persisted` + the `apply_now` outcome
//!    (`appliedLive` + a human `note`). The live re-tag is refused when
//!    `apply_live_allowed` is false, when the management interface cannot be
//!    determined, or when the named interface IS the management interface — each
//!    with the FastAPI note text, byte-identical.
//!
//! Unpin (DELETE):
//! 1. The override entry is popped from the config; `removedOverride` reflects
//!    whether it was present. A persist fault after a successful pop is swallowed
//!    (matching the FastAPI `except: pass`).
//! 2. The `.link` is removed; `removedLinkFile` reflects whether a file existed.
//!    A `.link` that exists but cannot be removed adds `link_error`.
//! 3. The static `note` reminds the operator a known no-efuse chipset is re-pinned
//!    automatically unless `network.mac_pin.enabled` is false.

use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use ados_macpin::engine::{NETWORKD_DIR, STATE_PATH};

use crate::config_store::{section_path, update_config};
use crate::state::AppState;

mod pin;

pub use pin::post_mac_pin;
pub use pin::MacPinRequest;
#[cfg(test)]
use pin::*;

// ---------------------------------------------------------------------------
// Shared helpers: config path, state-file path, networkd dir.
// ---------------------------------------------------------------------------

/// The on-disk state file with the per-adapter verdicts + the learner's proposed
/// MAC values (`/etc/ados/mac-pins.state`), the same file the installer step +
/// supervisor reconciler write and the heartbeat reads. Overridable via
/// `ADOS_MAC_PINS_STATE` for tests.
fn state_file_path() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_MAC_PINS_STATE").unwrap_or_else(|_| STATE_PATH.to_string()))
}

/// The `systemd-networkd` drop-in directory the pin `.link` lives in
/// (`/etc/systemd/network`). Overridable via `ADOS_NETWORKD_DIR` for tests.
fn networkd_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_NETWORKD_DIR").unwrap_or_else(|_| NETWORKD_DIR.to_string()))
}

/// Build a FastAPI-shaped error-object response: `(status, {"detail": {"error":
/// {"code": <code>, "message": <message>}}})`. Mirrors the Python
/// `HTTPException(status_code=..., detail={"error": {"code": ..., "message":
/// ...}})` shape these routes raise (FastAPI wraps the detail dict under
/// `"detail"`).
fn error_object(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"detail": {"error": {"code": code, "message": message.into()}}})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// MAC validation + normalisation (the Python `_MAC_RE` + `.lower().replace`).
// ---------------------------------------------------------------------------

/// Whether `mac` matches the Python `_MAC_RE`
/// (`^([0-9a-fA-F]{2}[:-]){5}[0-9a-fA-F]{2}$`): exactly six two-hex-digit octets
/// separated by a single `:` or `-` each (the separators may be mixed, as the
/// Python regex allows per-separator). Hand-rolled so the crate carries no regex
/// dependency.
fn mac_is_valid(mac: &str) -> bool {
    // Six octets ⇒ five separators ⇒ length is always 6*2 + 5 = 17.
    if mac.len() != 17 {
        return false;
    }
    let bytes = mac.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        // Positions 2, 5, 8, 11, 14 are the separators; the rest are hex digits.
        if i % 3 == 2 {
            if b != b':' && b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// Normalise an accepted MAC to lowercase colon form, mirroring the Python
/// `mac.lower().replace("-", ":")`.
fn normalise_mac(mac: &str) -> String {
    mac.to_ascii_lowercase().replace('-', ":")
}

// ---------------------------------------------------------------------------
// State-file read: the learner's proposed MAC for an interface.
// ---------------------------------------------------------------------------

/// The learner-proposed `pinned_mac` for the named interface from the state file,
/// or `None` when the file is absent / malformed / has no adapter with that name
/// and a truthy `pinned_mac`. Mirrors the Python fallback that scans
/// `read_mac_pins_state()["adapters"]` for `a["name"] == iface and a["pinned_mac"]`.
fn proposed_mac_for(state_path: &Path, iface: &str) -> Option<String> {
    let text = std::fs::read_to_string(state_path).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    let adapters = doc.get("adapters")?.as_array()?;
    for a in adapters {
        let obj = match a.as_object() {
            Some(o) => o,
            None => continue,
        };
        if obj.get("name").and_then(Value::as_str) == Some(iface) {
            if let Some(pinned) = obj.get("pinned_mac").and_then(Value::as_str) {
                if !pinned.is_empty() {
                    return Some(pinned.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Config read/merge: network.mac_pin.{overrides, apply_live_allowed}.
// ---------------------------------------------------------------------------

/// The `apply_live_allowed` flag from `network.mac_pin`, defaulting to `false`
/// (the Python field default). A missing section / non-boolean reads false.
fn apply_live_allowed(config_path: &Path) -> bool {
    let text = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let doc: serde_norway::Value = match serde_norway::from_str(&text) {
        Ok(v) => v,
        Err(_) => return false,
    };
    doc.get("network")
        .and_then(|n| n.get("mac_pin"))
        .and_then(|m| m.get("apply_live_allowed"))
        .and_then(serde_norway::Value::as_bool)
        .unwrap_or(false)
}

/// Merge `overrides[iface] = mac` into the `network.mac_pin.overrides` block of
/// the on-disk config, atomically (tmp + rename), preserving every other key and
/// the mapping insertion order (the Python `sort_keys=False`). Returns `Ok(())`
/// on success, `Err(message)` on any read/parse/write fault so the caller can map
/// it to the `E_PERSIST` 500.
fn config_set_override(config_path: &Path, iface: &str, mac: &str) -> Result<(), String> {
    update_config(config_path, |root| {
        section_path(root, &["network", "mac_pin", "overrides"]).insert(
            serde_norway::Value::String(iface.to_string()),
            serde_norway::Value::String(mac.to_string()),
        );
        Ok(())
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Remove `overrides[iface]` from the `network.mac_pin.overrides` block of the
/// on-disk config through the shared config store. `Ok(true)` when the key was
/// present and the removal landed, `Ok(false)` when there was nothing to remove
/// (nothing is written), `Err` when the document could not be read, parsed or
/// written.
fn config_remove_override(config_path: &Path, iface: &str) -> Result<bool, String> {
    update_config(config_path, |root| {
        let removed = root
            .get_mut("network")
            .and_then(|n| n.get_mut("mac_pin"))
            .and_then(|m| m.get_mut("overrides"))
            .and_then(serde_norway::Value::as_mapping_mut)
            .is_some_and(|overrides| overrides.remove(iface).is_some());
        Ok(removed)
    })
    .map(|w| w.value)
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Live re-tag + .link removal (the platform-touching legs).
// ---------------------------------------------------------------------------

/// The interface carrying the default route (the management path), used to refuse
/// a live re-tag that would drop the operator's own connection. Mirrors the
/// Python `_default_route_iface()` (`ip route get 1.1.1.1` → the token after
/// `dev`). `None` on any spawn / timeout / parse fault, which the caller treats
/// as "uncertain" and refuses the live re-tag for safety.
///
/// Deliberately still `ip route get 1.1.1.1` rather than the `/proc/net/route`
/// parse in [`super::reachable_addr::default_route_iface`]: the two can disagree
/// (policy routing, per-destination routes), and this answer decides whether a
/// MAC pin is applied to the live control interface. Swapping the resolution
/// strategy here is a separate decision from making the call non-blocking.
#[cfg(target_os = "linux")]
async fn default_route_iface() -> Option<String> {
    let out = crate::probe::capture(
        "ip",
        &["route", "get", "1.1.1.1"],
        crate::probe::PROBE_TIMEOUT,
    )
    .await;
    parse_route_get_dev(out.text())
}

#[cfg(not(target_os = "linux"))]
async fn default_route_iface() -> Option<String> {
    // The dev host has no `ip route get`; treat the management interface as
    // undeterminable so the live re-tag is refused for safety (the same posture
    // the Python takes when detection fails).
    None
}

/// The token following `dev` in `ip route get` output, or `None`.
///
/// Split out of the spawn so the token walk is unit-testable without a process.
/// Whitespace-splits the whole output rather than per line, which is what the
/// blocking version did — `ip route get` answers on one line, and a multi-line
/// answer's first `dev` is still the chosen egress.
// Only the Linux probe calls this; the non-Linux stub never spawns `ip`, so the
// parser would be dead code there outside the unit tests.
#[cfg(any(target_os = "linux", test))]
fn parse_route_get_dev(text: &str) -> Option<String> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    let dev_idx = parts.iter().position(|&p| p == "dev")?;
    parts.get(dev_idx + 1).map(|s| s.to_string())
}

/// Re-tag the live interface to `mac` now (down → set address → up), reusing the
/// shared `ados-macpin` engine so the bytes are identical to the installer /
/// reconciler path. Returns `Ok(())` on success, `Err(message)` on any failure
/// (mapped to the FastAPI "live re-tag failed" note). Linux-only; a non-Linux
/// build never reaches this (the management-iface gate refuses first when the
/// route cannot be determined).
///
/// Async because the engine spawns `ip link set` under `tokio::process` with a
/// timeout: a blocking spawn here stalls every other in-flight request on a
/// single-core SBC for the whole link down/up.
#[cfg(target_os = "linux")]
async fn apply_live(iface: &str, mac: &str) -> Result<(), String> {
    let parsed = ados_macpin::MacAddr::parse(mac).ok_or_else(|| format!("malformed MAC: {mac}"))?;
    ados_macpin::engine::apply_live(iface, &parsed)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
async fn apply_live(_iface: &str, _mac: &str) -> Result<(), String> {
    Err("live re-tag unavailable on this platform".to_string())
}

/// Remove the pin `.link` for the adapter `key` from `dir`: `Ok(true)` when a
/// file was removed, `Ok(false)` when none existed, `Err(message)` when one
/// exists and could not be removed (a read-only or unwritable networkd dir), so
/// the reply never passes a surviving `.link` off as "there was no file". Reuses
/// the shared `ados-macpin` engine on Linux (which also reloads udev, bounded
/// and off the reactor); on a non-Linux dev host it removes the file directly so
/// the flag and the error are still exercised by tests.
#[cfg(target_os = "linux")]
async fn remove_link_file(dir: &Path, key: &str) -> Result<bool, String> {
    ados_macpin::engine::remove_pin_link(dir, key)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
async fn remove_link_file(dir: &Path, key: &str) -> Result<bool, String> {
    let path = dir.join(ados_macpin::engine::link_file_name(key));
    if path.exists() {
        std::fs::remove_file(&path)
            .map(|()| true)
            .map_err(|e| e.to_string())
    } else {
        Ok(false)
    }
}

/// The adapter key for `iface` from the state file, or the `400
/// E_UNKNOWN_ADAPTER` response when the state file does not know it.
fn adapter_key_or_400(state_path: &Path, iface: &str) -> Result<String, Box<Response>> {
    ados_macpin::engine::adapter_key_for_iface(state_path, iface).ok_or_else(|| {
        Box::new(error_object(
            StatusCode::BAD_REQUEST,
            "E_UNKNOWN_ADAPTER",
            format!(
                "{iface} is not a known USB adapter; pins are keyed by the adapter's port, \
                 so only an adapter the agent has enumerated can be pinned"
            ),
        ))
    })
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/network/mac/{iface}
// ---------------------------------------------------------------------------

/// The static `note` the unpin route carries, byte-identical to the Python.
const NOTE_UNPIN: &str =
    "a known no-efuse adapter is re-pinned automatically unless network.mac_pin.enabled is false";

/// `DELETE /api/v1/network/mac/{iface}` → unpin: clear the override + remove the `.link`. A
/// `200` with `{status, iface, adapterKey, removedOverride, removedLinkFile, note}` — an absent
/// override / absent `.link` are reported as `false`. A config the store refuses adds
/// `persist_error`; a `.link` that exists but could not be removed adds `link_error`. An
/// interface the state file does not know is `400 E_UNKNOWN_ADAPTER`.
pub async fn delete_mac_pin(
    State(state): State<AppState>,
    AxumPath(iface): AxumPath<String>,
) -> Response {
    delete_mac_pin_at(
        &state.pairing_paths.config,
        &state_file_path(),
        &networkd_dir(),
        &iface,
    )
    .await
}

/// The unpin logic against explicit config, state-file and networkd-dir paths.
/// The public handler resolves them from the app state / env; this takes them
/// directly so a test can point them at temp paths.
async fn delete_mac_pin_at(
    config_path: &Path,
    state_path: &Path,
    networkd_dir: &Path,
    iface: &str,
) -> Response {
    let key = match adapter_key_or_400(state_path, iface) {
        Ok(k) => k,
        Err(resp) => return *resp,
    };
    // Pop the override. A config the store refuses (or cannot write) leaves the
    // pin in place, so the body says so rather than reporting a clean unpin.
    let (removed_override, persist_error) = match config_remove_override(config_path, &key) {
        Ok(removed) => (removed, None),
        Err(e) => {
            tracing::error!(error = %e, iface, "mac pin override not removed from the config");
            (false, Some(e))
        }
    };
    // Remove the `.link` (a file existed → true). A `.link` that exists but
    // could not be removed is reported with its error: networkd would re-apply
    // the old MAC at the next boot, so a bare `false` would read as "no file".
    let (removed_link, link_error) = match remove_link_file(networkd_dir, &key).await {
        Ok(removed) => (removed, None),
        Err(e) => {
            tracing::error!(error = %e, iface, "mac pin .link not removed");
            (false, Some(e))
        }
    };

    // The top-level status is the body-level failure convention other consumers
    // (the cloud dispatcher) read: a pin still in force is not an ok unpin.
    let failed = persist_error.is_some() || link_error.is_some();
    let mut body = json!({
        "status": if failed { "error" } else { "ok" },
        "iface": iface,
        "adapterKey": key,
        "removedOverride": removed_override,
        "removedLinkFile": removed_link,
        "note": NOTE_UNPIN,
    });
    if let Some(e) = persist_error {
        body["persist_error"] = json!(e);
    }
    if let Some(e) = link_error {
        body["link_error"] = json!(e);
    }
    if failed {
        body["message"] = json!("the adapter is still pinned for the next boot");
    }
    Json(body).into_response()
}

#[cfg(test)]
mod tests;
