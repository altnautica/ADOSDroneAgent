//! Stable-MAC pinning write routes: pin a MAC (POST) + unpin (DELETE).
//!
//! An onboard adapter with no efuse MAC randomizes its address each driver load,
//! churning the DHCP lease (and the box's IP) every boot. The agent pins a stable,
//! locally-administered MAC for such a chipset via a next-boot
//! `systemd-networkd` `.link` drop-in. These two routes let an operator confirm a
//! learner candidate, set an explicit override, or unpin:
//!
//! - **`POST /api/v1/network/mac/pin`** — pin a stable MAC on an adapter. The body
//!   is `{"iface", "mac"?, "apply_now"?}`. The MAC is resolved from `mac` or, when
//!   absent, the adapter's learner-proposed value in the on-disk state file. The
//!   resolved MAC is stored as a `network.mac_pin.overrides[iface]` entry and the
//!   config is persisted; the supervisor reconciler writes the actual `.link` on
//!   its next reconcile and on the next boot. With `apply_now` (and the config
//!   `apply_live_allowed` gate) it also re-tags the LIVE interface — refused on the
//!   management interface so it cannot drop the caller's own connection. Returns
//!   `{status, iface, mac, persisted, appliedLive, note}`.
//! - **`DELETE /api/v1/network/mac/{iface}`** — unpin: clear the override entry and
//!   remove the `.link`. Returns `{status, iface, removedOverride, removedLinkFile,
//!   note}`.
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

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Shared helpers: config path, state-file path, networkd dir.
// ---------------------------------------------------------------------------

/// The on-disk state file with the per-adapter verdicts + the learner's proposed
/// MAC values (`/etc/ados/mac-pins.state`), the same file the installer step +
/// supervisor reconciler write and the heartbeat reads. Overridable via
/// `ADOS_MAC_PINS_STATE` for tests. Mirrors the Python `read_mac_pins_state`
/// reading `ADOS_ETC_DIR / "mac-pins.state"`.
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
    mutate_overrides(config_path, |overrides| {
        overrides.insert(
            serde_norway::Value::String(iface.to_string()),
            serde_norway::Value::String(mac.to_string()),
        );
    })
}

/// Remove `overrides[iface]` from the `network.mac_pin.overrides` block of the
/// on-disk config, returning `(removed, persist_result)`. `removed` reflects
/// whether the key was present (matching the Python `overrides.pop(iface, None) is
/// not None`); the config is only re-persisted when a key was removed, and a
/// persist fault on that path is reported in `persist_result` but not surfaced as
/// an error (the Python swallows it with `except: pass`).
fn config_remove_override(config_path: &Path, iface: &str) -> (bool, Result<(), String>) {
    // Read first to learn whether the key is present without forcing a write when
    // it is not (the Python only re-persists when it removed something).
    let present = read_overrides(config_path)
        .map(|m| m.contains_key(iface))
        .unwrap_or(false);
    if !present {
        return (false, Ok(()));
    }
    let result = mutate_overrides(config_path, |overrides| {
        overrides.remove(iface);
    });
    (true, result)
}

/// Read the `network.mac_pin.overrides` mapping from the config, or `None` when
/// the file / section is absent or unparseable.
fn read_overrides(config_path: &Path) -> Option<serde_norway::Mapping> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let doc: serde_norway::Value = serde_norway::from_str(&text).ok()?;
    doc.get("network")
        .and_then(|n| n.get("mac_pin"))
        .and_then(|m| m.get("overrides"))
        .and_then(serde_norway::Value::as_mapping)
        .cloned()
}

/// Load the full config as a YAML value, apply `f` to the
/// `network.mac_pin.overrides` mapping (creating the `network` / `mac_pin` /
/// `overrides` nodes as needed), and write it back atomically. Shared by the
/// set + remove paths so both preserve the rest of the file identically. The
/// same tmp-write + rename idiom the WFB tx-power persist uses.
fn mutate_overrides<F>(config_path: &Path, f: F) -> Result<(), String>
where
    F: FnOnce(&mut serde_norway::Mapping),
{
    use serde_norway::{Mapping, Value as Yaml};

    // An absent / non-mapping file starts from an empty mapping (the Python
    // `data = {}` seed when the config is fresh).
    let mut data: Yaml = match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(Mapping::new()),
        },
        Err(_) => Yaml::Mapping(Mapping::new()),
    };

    {
        let root = data
            .as_mapping_mut()
            .ok_or_else(|| "config root is not a mapping".to_string())?;
        let network = root
            .entry(Yaml::String("network".to_string()))
            .or_insert_with(|| Yaml::Mapping(Mapping::new()));
        if !network.is_mapping() {
            *network = Yaml::Mapping(Mapping::new());
        }
        let network_map = network
            .as_mapping_mut()
            .ok_or_else(|| "network section is not a mapping".to_string())?;
        let mac_pin = network_map
            .entry(Yaml::String("mac_pin".to_string()))
            .or_insert_with(|| Yaml::Mapping(Mapping::new()));
        if !mac_pin.is_mapping() {
            *mac_pin = Yaml::Mapping(Mapping::new());
        }
        let mac_pin_map = mac_pin
            .as_mapping_mut()
            .ok_or_else(|| "mac_pin section is not a mapping".to_string())?;
        let overrides = mac_pin_map
            .entry(Yaml::String("overrides".to_string()))
            .or_insert_with(|| Yaml::Mapping(Mapping::new()));
        if !overrides.is_mapping() {
            *overrides = Yaml::Mapping(Mapping::new());
        }
        let overrides_map = overrides
            .as_mapping_mut()
            .ok_or_else(|| "overrides section is not a mapping".to_string())?;
        f(overrides_map);
    }

    let body = serde_norway::to_string(&data).map_err(|e| e.to_string())?;
    write_atomic(config_path, body.as_bytes())
}

/// Write `bytes` to `path` atomically: ensure the parent dir, write a `.tmp`
/// sibling, then rename over the target. Mirrors the Python tmp-write +
/// `os.replace` idiom. Returns `Err(message)` on any I/O fault.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Live re-tag + .link removal (the platform-touching legs).
// ---------------------------------------------------------------------------

/// The interface carrying the default route (the management path), used to refuse
/// a live re-tag that would drop the operator's own connection. Mirrors the
/// Python `_default_route_iface()` (`ip route get 1.1.1.1` → the token after
/// `dev`). `None` on any spawn / parse fault, which the caller treats as
/// "uncertain" and refuses the live re-tag for safety.
#[cfg(target_os = "linux")]
fn default_route_iface() -> Option<String> {
    let out = std::process::Command::new("ip")
        .args(["route", "get", "1.1.1.1"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = text.split_whitespace().collect();
    let dev_idx = parts.iter().position(|&p| p == "dev")?;
    parts.get(dev_idx + 1).map(|s| s.to_string())
}

#[cfg(not(target_os = "linux"))]
fn default_route_iface() -> Option<String> {
    // The dev host has no `ip route get`; treat the management interface as
    // undeterminable so the live re-tag is refused for safety (the same posture
    // the Python takes when detection fails).
    None
}

/// Re-tag the live interface to `mac` now (down → set address → up), reusing the
/// shared `ados-macpin` engine so the bytes are identical to the installer /
/// reconciler path. Returns `Ok(())` on success, `Err(message)` on any failure
/// (mapped to the FastAPI "live re-tag failed" note). Linux-only; a non-Linux
/// build never reaches this (the management-iface gate refuses first when the
/// route cannot be determined).
#[cfg(target_os = "linux")]
fn apply_live(iface: &str, mac: &str) -> Result<(), String> {
    let parsed = ados_macpin::MacAddr::parse(mac).ok_or_else(|| format!("malformed MAC: {mac}"))?;
    ados_macpin::engine::apply_live(iface, &parsed).map_err(|e| e.to_string())
}

#[cfg(not(target_os = "linux"))]
fn apply_live(_iface: &str, _mac: &str) -> Result<(), String> {
    Err("live re-tag unavailable on this platform".to_string())
}

/// Remove the pin `.link` for `iface` from `dir`, returning whether a file was
/// removed. Reuses the shared `ados-macpin` engine on Linux (which also reloads
/// udev); on a non-Linux dev host it removes the file directly so the
/// `removedLinkFile` flag is still exercised by tests.
#[cfg(target_os = "linux")]
fn remove_link_file(dir: &Path, iface: &str) -> bool {
    ados_macpin::engine::remove_pin_link(dir, iface).unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn remove_link_file(dir: &Path, iface: &str) -> bool {
    let path = dir.join(ados_macpin::engine::link_file_name(iface));
    if path.exists() {
        std::fs::remove_file(&path).is_ok()
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/network/mac/pin
// ---------------------------------------------------------------------------

/// The `POST /api/v1/network/mac/pin` request body. Mirrors the Python
/// `MacPinRequest`: a required interface, an optional explicit MAC, and an
/// optional live-apply flag (default false).
#[derive(Debug, Deserialize)]
pub struct MacPinRequest {
    pub iface: String,
    #[serde(default)]
    pub mac: Option<String>,
    #[serde(default)]
    pub apply_now: bool,
}

/// The outcome of the optional `apply_now` live re-tag: whether it was applied +
/// the human note the body carries. Split out so the gate decisions are tested
/// without the `ip` IO.
struct ApplyOutcome {
    applied_live: bool,
    note: String,
}

/// The default `note` when `apply_now` is false (or unset): the pin lands on the
/// next reconcile.
const NOTE_PINNED_NEXT_BOOT: &str =
    "pinned for next boot; the agent writes the .link on its next reconcile";

/// `POST /api/v1/network/mac/pin` → pin a stable MAC on an adapter.
///
/// Resolves the MAC (body or state-file candidate), validates + normalises it,
/// merges the override into the config, and (optionally + gated) re-tags the live
/// interface. Degrades to the documented `400`/`500` error-object bodies on each
/// guard; never panics on a seam fault.
pub async fn post_mac_pin(
    State(state): State<AppState>,
    Json(req): Json<MacPinRequest>,
) -> Response {
    post_mac_pin_at(&state.pairing_paths.config, &state_file_path(), req).await
}

/// The pin logic against explicit config + state-file paths. The public handler
/// resolves both from the app state / env; this takes them directly so a test can
/// point them at temp paths without mutating process-global env.
async fn post_mac_pin_at(config_path: &Path, state_path: &Path, req: MacPinRequest) -> Response {
    // 1. Resolve the MAC: the body value (trimmed) wins; an empty body MAC falls
    //    back to the state file's learner-proposed value for the interface.
    let mut mac = req.mac.as_deref().unwrap_or("").trim().to_string();
    if mac.is_empty() {
        if let Some(proposed) = proposed_mac_for(state_path, &req.iface) {
            mac = proposed;
        }
    }
    if mac.is_empty() {
        return error_object(
            StatusCode::BAD_REQUEST,
            "E_NO_MAC",
            "provide a MAC, or pin a candidate that already has a proposed value",
        );
    }

    // 2. Validate against the 6-octet colon/dash form, then normalise to lowercase
    //    colon form.
    if !mac_is_valid(&mac) {
        return error_object(
            StatusCode::BAD_REQUEST,
            "E_BAD_MAC",
            format!("malformed MAC: {mac}"),
        );
    }
    let mac = normalise_mac(&mac);

    // 3. Merge the override into the config + persist. A fault is the E_PERSIST 500.
    if let Err(e) = config_set_override(config_path, &req.iface, &mac) {
        return error_object(StatusCode::INTERNAL_SERVER_ERROR, "E_PERSIST", e);
    }
    let persisted = true;

    // 4. The optional live re-tag, gated three ways (config flag, mgmt-iface
    //    determinable, the named iface is NOT the mgmt iface).
    let outcome = if req.apply_now {
        apply_now_outcome(config_path, &req.iface, &mac)
    } else {
        ApplyOutcome {
            applied_live: false,
            note: NOTE_PINNED_NEXT_BOOT.to_string(),
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "iface": req.iface,
            "mac": mac,
            "persisted": persisted,
            "appliedLive": outcome.applied_live,
            "note": outcome.note,
        })),
    )
        .into_response()
}

/// The `apply_now` decision, mirroring the Python branch exactly: refuse when the
/// config flag is off, when the management interface cannot be determined, or when
/// the named interface IS the management interface — each with the FastAPI note;
/// otherwise re-tag the live interface and report success (or the failure note).
fn apply_now_outcome(config_path: &Path, iface: &str, mac: &str) -> ApplyOutcome {
    if !apply_live_allowed(config_path) {
        return ApplyOutcome {
            applied_live: false,
            note: "live re-tag not permitted (set network.mac_pin.apply_live_allowed=true); pinned for next boot"
                .to_string(),
        };
    }
    // Resolve the management interface ONCE; an undeterminable route is "uncertain"
    // and refuses the live re-tag for safety (never falls through to it).
    let mgmt_iface = match default_route_iface() {
        Some(m) => m,
        None => {
            return ApplyOutcome {
                applied_live: false,
                note: "could not determine the management interface; refusing the live re-tag for safety; pinned for next boot"
                    .to_string(),
            };
        }
    };
    if iface == mgmt_iface {
        return ApplyOutcome {
            applied_live: false,
            note: format!(
                "refusing to re-tag {iface} live: it carries the management route; pinned for next boot"
            ),
        };
    }
    match apply_live(iface, mac) {
        Ok(()) => ApplyOutcome {
            applied_live: true,
            note: "applied to the live interface now".to_string(),
        },
        Err(e) => ApplyOutcome {
            applied_live: false,
            note: format!("live re-tag failed ({e}); pinned for next boot"),
        },
    }
}

// ---------------------------------------------------------------------------
// DELETE /api/v1/network/mac/{iface}
// ---------------------------------------------------------------------------

/// The static `note` the unpin route carries, byte-identical to the Python.
const NOTE_UNPIN: &str =
    "a known no-efuse adapter is re-pinned automatically unless network.mac_pin.enabled is false";

/// `DELETE /api/v1/network/mac/{iface}` → unpin: clear the override + remove the
/// `.link`. Always a `200` with `{status, iface, removedOverride, removedLinkFile,
/// note}` — an absent override / absent `.link` are reported as `false`, never an
/// error. Mirrors the Python `delete_mac_pin`.
pub async fn delete_mac_pin(
    State(state): State<AppState>,
    AxumPath(iface): AxumPath<String>,
) -> Response {
    delete_mac_pin_at(&state.pairing_paths.config, &networkd_dir(), &iface)
}

/// The unpin logic against explicit config + networkd-dir paths. The public
/// handler resolves both from the app state / env; this takes them directly so a
/// test can point them at temp paths.
fn delete_mac_pin_at(config_path: &Path, networkd_dir: &Path, iface: &str) -> Response {
    // Pop the override (re-persist only when it was present; swallow a persist
    // fault, matching the Python `except: pass`).
    let (removed_override, _persist) = config_remove_override(config_path, iface);
    // Remove the `.link` (a file existed → true).
    let removed_link = remove_link_file(networkd_dir, iface);

    Json(json!({
        "status": "ok",
        "iface": iface,
        "removedOverride": removed_override,
        "removedLinkFile": removed_link,
        "note": NOTE_UNPIN,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    // ── MAC validation + normalisation ────────────────────────────────────────

    #[test]
    fn mac_validation_matches_the_python_regex() {
        // Valid: six two-hex octets, colon- or dash-separated (and mixed, as the
        // Python per-separator regex allows).
        assert!(mac_is_valid("02:c6:75:83:1a:3e"));
        assert!(mac_is_valid("02-C6-75-83-1A-3E"));
        assert!(mac_is_valid("AA:bb:CC:dd:EE:ff"));
        assert!(mac_is_valid("02-c6:75-83:1a-3e"));
        // Invalid: too few/many octets, a non-hex digit, a bad separator, wrong
        // octet width.
        assert!(!mac_is_valid("02:c6:75:83:1a"));
        assert!(!mac_is_valid("02:c6:75:83:1a:3e:99"));
        assert!(!mac_is_valid("0g:c6:75:83:1a:3e"));
        assert!(!mac_is_valid("02_c6_75_83_1a_3e"));
        assert!(!mac_is_valid("2:c6:75:83:1a:3e"));
        assert!(!mac_is_valid(""));
        assert!(!mac_is_valid("not-a-mac"));
    }

    #[test]
    fn normalise_lowercases_and_swaps_dashes_for_colons() {
        assert_eq!(normalise_mac("02-C6-75-83-1A-3E"), "02:c6:75:83:1a:3e");
        assert_eq!(normalise_mac("AA:BB:CC:DD:EE:FF"), "aa:bb:cc:dd:ee:ff");
    }

    // ── state-file candidate resolution ───────────────────────────────────────

    #[test]
    fn proposed_mac_reads_the_state_file_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("mac-pins.state");
        std::fs::write(
            &state,
            serde_json::to_string(&json!({
                "version": 1,
                "adapters": [
                    {"name": "wlan0", "pinned_mac": "02:c6:75:83:1a:3e"},
                    {"name": "wlan1"},
                ],
            }))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            proposed_mac_for(&state, "wlan0"),
            Some("02:c6:75:83:1a:3e".to_string())
        );
        // An adapter with no pinned_mac, an unknown name, and an absent file all
        // resolve to None.
        assert_eq!(proposed_mac_for(&state, "wlan1"), None);
        assert_eq!(proposed_mac_for(&state, "eth0"), None);
        assert_eq!(
            proposed_mac_for(&dir.path().join("absent.state"), "wlan0"),
            None
        );
    }

    // ── config merge: set + remove preserve the rest of the file ──────────────

    #[test]
    fn set_override_creates_the_section_and_preserves_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();
        config_set_override(&cfg, "wlan0", "02:c6:75:83:1a:3e").unwrap();

        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let ov = parsed
            .get("network")
            .and_then(|n| n.get("mac_pin"))
            .and_then(|m| m.get("overrides"))
            .and_then(|o| o.get("wlan0"))
            .and_then(serde_norway::Value::as_str);
        assert_eq!(ov, Some("02:c6:75:83:1a:3e"));
        // The unrelated agent.name survived.
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("my-drone")
        );
    }

    #[test]
    fn set_override_on_an_absent_file_seeds_an_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        config_set_override(&cfg, "wlan0", "aa:bb:cc:dd:ee:ff").unwrap();
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("network")
                .and_then(|n| n.get("mac_pin"))
                .and_then(|m| m.get("overrides"))
                .and_then(|o| o.get("wlan0"))
                .and_then(serde_norway::Value::as_str),
            Some("aa:bb:cc:dd:ee:ff")
        );
    }

    #[test]
    fn remove_override_pops_a_present_key_and_reports_true() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "agent:\n  name: my-drone\nnetwork:\n  mac_pin:\n    overrides:\n      wlan0: 02:c6:75:83:1a:3e\n",
        )
        .unwrap();
        let (removed, persist) = config_remove_override(&cfg, "wlan0");
        assert!(removed);
        assert!(persist.is_ok());
        // The key is gone; agent.name survived.
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        let ov_present = parsed
            .get("network")
            .and_then(|n| n.get("mac_pin"))
            .and_then(|m| m.get("overrides"))
            .and_then(|o| o.get("wlan0"))
            .is_some();
        assert!(!ov_present);
        assert_eq!(
            parsed
                .get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("my-drone")
        );
    }

    #[test]
    fn remove_override_of_an_absent_key_reports_false_and_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();
        let before = std::fs::read_to_string(&cfg).unwrap();
        let (removed, persist) = config_remove_override(&cfg, "wlan0");
        assert!(!removed);
        assert!(persist.is_ok());
        // The file is unchanged (the Python only re-persists when it popped a key).
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
    }

    // ── apply_live_allowed flag ───────────────────────────────────────────────

    #[test]
    fn apply_live_allowed_defaults_false_and_reads_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // Absent file / absent section default to false.
        assert!(!apply_live_allowed(&cfg));
        std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();
        assert!(!apply_live_allowed(&cfg));
        // The explicit true flag reads true.
        std::fs::write(&cfg, "network:\n  mac_pin:\n    apply_live_allowed: true\n").unwrap();
        assert!(apply_live_allowed(&cfg));
    }

    // ── POST handler: the guard order + the success body ──────────────────────

    #[tokio::test]
    async fn no_mac_and_no_candidate_is_a_400_e_no_mac() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let state = dir.path().join("mac-pins.state");
        let resp = post_mac_pin_at(
            &cfg,
            &state,
            MacPinRequest {
                iface: "wlan0".to_string(),
                mac: None,
                apply_now: false,
            },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {"code": "E_NO_MAC", "message": "provide a MAC, or pin a candidate that already has a proposed value"}}})
        );
    }

    #[tokio::test]
    async fn a_malformed_mac_is_a_400_e_bad_mac() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let state = dir.path().join("mac-pins.state");
        let resp = post_mac_pin_at(
            &cfg,
            &state,
            MacPinRequest {
                iface: "wlan0".to_string(),
                mac: Some("not-a-mac".to_string()),
                apply_now: false,
            },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({"detail": {"error": {"code": "E_BAD_MAC", "message": "malformed MAC: not-a-mac"}}})
        );
    }

    #[tokio::test]
    async fn an_explicit_mac_pins_and_normalises_with_the_next_boot_note() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let state = dir.path().join("mac-pins.state");
        let resp = post_mac_pin_at(
            &cfg,
            &state,
            MacPinRequest {
                iface: "wlan0".to_string(),
                mac: Some("02-C6-75-83-1A-3E".to_string()),
                apply_now: false,
            },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body,
            json!({
                "status": "ok",
                "iface": "wlan0",
                "mac": "02:c6:75:83:1a:3e",
                "persisted": true,
                "appliedLive": false,
                "note": "pinned for next boot; the agent writes the .link on its next reconcile",
            })
        );
        // The normalised MAC landed in the config override.
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("network")
                .and_then(|n| n.get("mac_pin"))
                .and_then(|m| m.get("overrides"))
                .and_then(|o| o.get("wlan0"))
                .and_then(serde_norway::Value::as_str),
            Some("02:c6:75:83:1a:3e")
        );
    }

    #[tokio::test]
    async fn an_empty_body_mac_falls_back_to_the_state_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let state = dir.path().join("mac-pins.state");
        std::fs::write(
            &state,
            serde_json::to_string(&json!({
                "version": 1,
                "adapters": [{"name": "wlan0", "pinned_mac": "AA:BB:CC:DD:EE:FF"}],
            }))
            .unwrap(),
        )
        .unwrap();
        let resp = post_mac_pin_at(
            &cfg,
            &state,
            MacPinRequest {
                iface: "wlan0".to_string(),
                mac: Some("   ".to_string()),
                apply_now: false,
            },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // The candidate is normalised on resolve.
        assert_eq!(body["mac"], json!("aa:bb:cc:dd:ee:ff"));
        assert_eq!(body["appliedLive"], json!(false));
    }

    #[tokio::test]
    async fn apply_now_without_the_config_flag_refuses_with_the_not_permitted_note() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let state = dir.path().join("mac-pins.state");
        let resp = post_mac_pin_at(
            &cfg,
            &state,
            MacPinRequest {
                iface: "wlan9".to_string(),
                mac: Some("02:c6:75:83:1a:3e".to_string()),
                apply_now: true,
            },
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["appliedLive"], json!(false));
        assert_eq!(
            body["note"],
            json!("live re-tag not permitted (set network.mac_pin.apply_live_allowed=true); pinned for next boot")
        );
    }

    // ── apply_now_outcome gate decisions (no ip IO on these paths) ─────────────

    #[test]
    fn apply_now_outcome_refuses_when_the_flag_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // No flag → refused, not permitted.
        let out = apply_now_outcome(&cfg, "wlan0", "02:c6:75:83:1a:3e");
        assert!(!out.applied_live);
        assert!(out.note.contains("not permitted"));
    }

    #[test]
    fn apply_now_outcome_refuses_when_the_mgmt_iface_is_undeterminable() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(&cfg, "network:\n  mac_pin:\n    apply_live_allowed: true\n").unwrap();
        // On the non-Linux dev host default_route_iface() is None, so the gate
        // refuses for safety (the "could not determine" note). On Linux this asserts
        // the same refusal whenever `ip route get` yields no dev (e.g. no default
        // route on the CI box), which the conformance harness covers in sandbox.
        let out = apply_now_outcome(&cfg, "wlan0", "02:c6:75:83:1a:3e");
        if default_route_iface().is_none() {
            assert!(!out.applied_live);
            assert!(out
                .note
                .contains("could not determine the management interface"));
        }
    }

    // ── DELETE handler ────────────────────────────────────────────────────────

    #[test]
    fn delete_removes_a_present_override_and_link_and_reports_both_true() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let netd = dir.path().join("networkd");
        std::fs::create_dir_all(&netd).unwrap();
        std::fs::write(
            &cfg,
            "network:\n  mac_pin:\n    overrides:\n      wlan0: 02:c6:75:83:1a:3e\n",
        )
        .unwrap();
        // Seed a .link file at the engine's canonical name so the remove reports true.
        let link = netd.join(ados_macpin::engine::link_file_name("wlan0"));
        std::fs::write(
            &link,
            "[Match]\nOriginalName=wlan0\n[Link]\nMACAddress=02:c6:75:83:1a:3e\n",
        )
        .unwrap();

        let resp = delete_mac_pin_at(&cfg, &netd, "wlan0");
        let body = futures_block_on(body_json(resp));
        assert_eq!(body["status"], json!("ok"));
        assert_eq!(body["iface"], json!("wlan0"));
        assert_eq!(body["removedOverride"], json!(true));
        assert_eq!(body["removedLinkFile"], json!(true));
        assert_eq!(
            body["note"],
            json!("a known no-efuse adapter is re-pinned automatically unless network.mac_pin.enabled is false")
        );
        // The .link file is gone.
        assert!(!link.exists());
    }

    #[test]
    fn delete_of_an_absent_override_and_link_reports_both_false() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let netd = dir.path().join("networkd");
        std::fs::create_dir_all(&netd).unwrap();
        std::fs::write(&cfg, "agent:\n  name: my-drone\n").unwrap();

        let resp = delete_mac_pin_at(&cfg, &netd, "wlan0");
        let body = futures_block_on(body_json(resp));
        assert_eq!(body["removedOverride"], json!(false));
        assert_eq!(body["removedLinkFile"], json!(false));
    }

    // ── shared body helpers ───────────────────────────────────────────────────

    /// Read a response body as JSON.
    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Drive a future to completion on a fresh single-thread runtime so the
    /// synchronous DELETE-path tests can read the response body.
    fn futures_block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }
}
