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
//!   added `rearm_blocked: true`, and NOTHING is persisted. The operator must
//!   `unpair` first.
//! - **Otherwise** it persists the new arm flag, re-writing the canonical pair
//!   state (`video.wfb.{paired_with_device_id, paired_at, auto_pair_enabled}`,
//!   with the legacy `ground_station.*` mirror on the GS profile) from the values
//!   the status read just computed, and returns the status snapshot with
//!   `auto_pair_enabled` set to the requested value.
//!
//! The residual route then mirrors the result onto the in-process Python config
//! object so the auto-pair supervisor sees the change without a reload race. The
//! native front holds no such in-process config object — the supervisor reads the
//! same on-disk YAML this route writes on its own cadence — so that mirror step
//! has no native counterpart and the response is the manager result unchanged.
//!
//! ## Why this ports cleanly to the native front
//!
//! There is no in-process manager and no command-socket seam: the whole effect is
//! reading the role-appropriate key file + the config, then (on the persist path)
//! a surgical YAML merge of three `video.wfb` fields — the same atomic merge the
//! sibling `wfb_write` tx-power route and the `mac_pin` write use. So the front
//! does the identical things the residual route does with no daemon round-trip.
//! The persist requires euid 0 (the config is a 0600 root-owned file); a non-root
//! front cannot write it, exactly like the residual `_save_config_dict` returning
//! `False` — the side effect simply does not land, and the residual route ignores
//! that result, so the RESPONSE is unchanged either way (it echoes the values the
//! status read computed, regardless of whether the persist landed).
//!
//! ## Response shape (matched to the residual route)
//!
//! Persist path: `{paired, paired_with_device_id, paired_at, fingerprint,
//! auto_pair_enabled: <requested>, role}`. Re-arm-blocked path: the same keys with
//! `auto_pair_enabled: false` plus `rearm_blocked: true`. Both carry the exact
//! field set + casing the pair-status read produces.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

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
// Pair-status snapshot: the same `status(role)` the GET /api/wfb/pair read
// computes, the input both branches of set_auto_pair start from.
// ---------------------------------------------------------------------------

/// The exact 64-byte size a complete WFB-ng key file is. Mirrors
/// `WFB_KEY_FILE_BYTES`.
const WFB_KEY_FILE_BYTES: usize = 64;

/// The byte offset of the peer-public half (the second 32 bytes) inside a 64-byte
/// WFB key file. Mirrors `WFB_PUBLIC_HALF_OFFSET`.
const WFB_PUBLIC_HALF_OFFSET: usize = 32;

/// The live pair-status snapshot for a role: the field set `set_auto_pair` reads
/// before it branches, and that the persist path writes back from. Mirrors the
/// dict the residual `PairManager.status(role)` returns.
struct PairStatus {
    paired: bool,
    /// The peer device-id off the config (or the GS legacy mirror), or JSON null.
    peer: Value,
    /// The paired-at string off the config, demoted to null for a YAML-timestamp
    /// (matching the residual `isinstance(str)` guard over the YAML-loaded value),
    /// or null.
    paired_at: Value,
    /// The blake2b-8 key fingerprint, or JSON null when not paired / unreadable.
    fingerprint: Value,
    /// The current arm flag off the config (default true when absent). Computed for
    /// status fidelity (and asserted by the read-parity test), but the handler's
    /// response always reports the REQUESTED value, not this stored one — the
    /// residual `set_auto_pair` returns `{**current, auto_pair_enabled: enabled}`,
    /// overriding the status value — so the handler never reads this field.
    #[allow(dead_code)]
    auto_pair_enabled: bool,
    /// `"drone"` or `"gs"`.
    role: String,
}

/// Resolve the bind-protocol role from the agent's profile, mirroring the
/// residual `_current_role(app)` → `_agent_role_from_profile`. The profile is the
/// hyphen-wire form (`"drone"` / `"ground-station"`); the role is `"drone"` only
/// when the profile is exactly `"drone"`, else `"gs"`.
fn current_role(config_profile: &str) -> String {
    let (profile, _role) = crate::profile::current_profile_and_role(config_profile);
    if profile == "drone" {
        "drone".to_string()
    } else {
        "gs".to_string()
    }
}

/// Compute the live pair-status snapshot the manager reads, mirroring the
/// residual `PairManager.status(role)` byte-for-byte: the role-appropriate key
/// file is the paired signal (present AND exactly 64 bytes AND a readable
/// fingerprint), and the peer / paired-at / auto-pair come off the config with the
/// legacy `ground_station.*` fallback on the GS profile.
fn read_pair_status(config_path: &Path, key_dir: &Path, role: &str) -> PairStatus {
    // The role-appropriate key file: tx.key for a drone, rx.key for a GS.
    let key_path = if role == "drone" {
        key_dir.join("tx.key")
    } else {
        key_dir.join("rx.key")
    };

    // paired := the file exists AND is exactly 64 bytes. A readable fingerprint is
    // then required; a 64-byte file whose fingerprint cannot be read reverts paired
    // to false, matching the residual `except (OSError, ValueError): paired = False`.
    let mut paired = std::fs::metadata(&key_path)
        .map(|m| m.is_file() && m.len() == WFB_KEY_FILE_BYTES as u64)
        .unwrap_or(false);
    let mut fingerprint: Value = Value::Null;
    if paired {
        match read_public_fingerprint(&key_path) {
            Some(fp) => fingerprint = json!(fp),
            None => paired = false,
        }
    }

    // Peer / paired-at / auto-pair off the raw config dict, mirroring the residual
    // `_load_config_dict()` read (a present-but-non-string peer/paired-at reads as
    // null, an absent auto-pair flag defaults to true).
    let raw = load_config_value(config_path);
    let wfb_section = raw
        .get("video")
        .filter(|v| v.is_object())
        .and_then(|v| v.get("wfb"))
        .filter(|v| v.is_object());

    let mut peer = wfb_section
        .and_then(|w| w.get("paired_with_device_id"))
        .filter(|v| v.is_string())
        .cloned()
        .unwrap_or(Value::Null);
    let mut paired_at = wfb_section
        .and_then(|w| w.get("paired_at"))
        .map(paired_at_string)
        .unwrap_or(Value::Null);
    let auto_pair_enabled = wfb_section
        .and_then(|w| w.get("auto_pair_enabled"))
        .map(json_truthy)
        .unwrap_or(true);

    // GS-profile fallback: a rig migrated from an older config may carry the pair
    // state under `ground_station.*` without the `video.wfb.*` mirror.
    if role == "gs" && peer.is_null() {
        let gs = raw.get("ground_station").filter(|v| v.is_object());
        peer = gs
            .and_then(|g| g.get("paired_drone_id"))
            .filter(|v| v.is_string())
            .cloned()
            .unwrap_or(Value::Null);
        if paired_at.is_null() {
            paired_at = gs
                .and_then(|g| g.get("paired_at"))
                .map(paired_at_string)
                .unwrap_or(Value::Null);
        }
    }

    PairStatus {
        paired,
        peer,
        paired_at,
        fingerprint,
        auto_pair_enabled,
        role: role.to_string(),
    }
}

/// The 16-hex-char public-key fingerprint of a WFB key file, or `None` when the
/// file is absent or not exactly 64 bytes. The peer-public half is the second 32
/// bytes; the fingerprint is `blake2b(pub, digest_size=8)` rendered as 16
/// lowercase hex chars. Byte-identical to `key_mgr.read_public_fingerprint`.
fn read_public_fingerprint(path: &Path) -> Option<String> {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;
    let data = std::fs::read(path).ok()?;
    if data.len() != WFB_KEY_FILE_BYTES {
        return None;
    }
    let mut hasher = Blake2bVar::new(8).ok()?;
    hasher.update(&data[WFB_PUBLIC_HALF_OFFSET..]);
    let mut out = [0u8; 8];
    hasher.finalize_variable(&mut out).ok()?;
    Some(hex::encode(out))
}

/// Load `/etc/ados/config.yaml` as a raw JSON value (objects/arrays/scalars
/// preserved), tolerating absence / a parse error / a non-object root with an
/// empty object. Mirrors the residual `_load_config_dict` read.
fn load_config_value(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return json!({}),
    };
    match serde_norway::from_str::<Value>(&text) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

/// The `paired_at` field value the status read reports, mirroring the residual
/// pair-status read's `paired_at if isinstance(paired_at, str) else None`: a
/// non-string value is null, and a YAML-timestamp-shaped string (which the
/// residual's YAML loader resolves to a `datetime`, so its `isinstance(str)` guard
/// demotes it to null) is also null. A non-timestamp string passes through.
fn paired_at_string(v: &Value) -> Value {
    match v.as_str() {
        Some(s) if !is_yaml_timestamp(s) => json!(s),
        _ => Value::Null,
    }
}

/// Python `bool(x)` truthiness over a JSON value, for the `auto_pair_enabled`
/// coercion: `null`/`false`/`0`/`0.0`/`""`/`[]`/`{}` are falsey, everything else
/// truthy. Mirrors `bool(wfb_section.get("auto_pair_enabled", True))`.
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// True when `s` matches the YAML implicit timestamp grammar a standard YAML
/// loader resolves to a date/datetime (and therefore not a plain string).
/// Reproduces the loader's implicit resolver: either a bare `YYYY-MM-DD` date, or
/// a full datetime `YYYY-M-D` (single- or double-digit month/day) followed by a
/// `T`/whitespace separator, `H:MM:SS`, an optional fractional second, and an
/// optional `Z` or numeric timezone offset. Mirrors the sibling read module's
/// `is_yaml_timestamp`.
fn is_yaml_timestamp(s: &str) -> bool {
    let b = s.as_bytes();

    fn digits(b: &[u8], i: usize) -> usize {
        let mut j = i;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        j - i
    }

    if digits(b, 0) != 4 {
        return false;
    }
    let mut i = 4;
    if b.get(i) != Some(&b'-') {
        return false;
    }
    i += 1;
    let month = digits(b, i);
    if month == 0 || month > 2 {
        return false;
    }
    i += month;
    if b.get(i) != Some(&b'-') {
        return false;
    }
    i += 1;
    let day = digits(b, i);
    if day == 0 || day > 2 {
        return false;
    }
    i += day;

    if i == s.len() {
        return month == 2 && day == 2;
    }

    match b.get(i) {
        Some(b'T') | Some(b't') => i += 1,
        Some(b' ') | Some(b'\t') => {
            while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
                i += 1;
            }
        }
        _ => return false,
    }
    let hour = digits(b, i);
    if hour == 0 || hour > 2 {
        return false;
    }
    i += hour;
    if b.get(i) != Some(&b':') {
        return false;
    }
    i += 1;
    if digits(b, i) != 2 {
        return false;
    }
    i += 2;
    if b.get(i) != Some(&b':') {
        return false;
    }
    i += 1;
    if digits(b, i) != 2 {
        return false;
    }
    i += 2;

    if b.get(i) == Some(&b'.') {
        i += 1;
        i += digits(b, i);
    }

    while matches!(b.get(i), Some(b' ') | Some(b'\t')) {
        i += 1;
    }
    match b.get(i) {
        None => return true,
        Some(b'Z') | Some(b'z') => {
            i += 1;
        }
        Some(b'+') | Some(b'-') => {
            i += 1;
            let tz_hour = digits(b, i);
            if tz_hour == 0 || tz_hour > 2 {
                return false;
            }
            i += tz_hour;
            if b.get(i) == Some(&b':') {
                i += 1;
                if digits(b, i) != 2 {
                    return false;
                }
                i += 2;
            }
        }
        _ => return false,
    }
    i == s.len()
}

// ---------------------------------------------------------------------------
// Persist: re-write the canonical pair state with the new arm flag, mirroring
// the residual `_persist_pair_state`.
// ---------------------------------------------------------------------------

/// Re-write the persisted pair fields under `video.wfb` (canonical) and mirror
/// onto `ground_station.{paired_drone_id, paired_at}` for the GS profile, from the
/// values the status read computed plus the new arm flag. Mirrors the residual
/// `_persist_pair_state(role, peer_device_id, paired_at, auto_pair_enabled)`:
///
/// - a null `peer` pops `video.wfb.paired_with_device_id` (and the GS mirror keys);
/// - a null `paired_at` pops `video.wfb.paired_at`;
/// - `auto_pair_enabled` is always set;
/// - a string `peer` sets it (and, on the GS profile, the legacy mirror keys).
///
/// Returns `Ok(())` on a clean write, `Err(message)` on any read/parse/write fault
/// (e.g. the EPERM a non-root front gets on the 0600 config). The caller ignores
/// the result — the response echoes the status values regardless — matching the
/// residual route, which ignores `_save_config_dict`'s boolean.
fn persist_pair_state(
    config_path: &Path,
    role: &str,
    peer: &Value,
    paired_at: &Value,
    auto_pair_enabled: bool,
) -> Result<(), String> {
    use serde_norway::{Mapping, Value as Yaml};

    // An absent / non-mapping file starts from an empty mapping (the residual
    // `data = {}` seed when the config is fresh).
    let mut data: Yaml = match std::fs::read_to_string(config_path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(Mapping::new()),
        },
        Err(_) => Yaml::Mapping(Mapping::new()),
    };

    // Convert the JSON status fields to their YAML scalar (a string or, when null,
    // nothing — the absence is what pops the key).
    let peer_yaml: Option<Yaml> = peer.as_str().map(|s| Yaml::String(s.to_string()));
    let paired_at_yaml: Option<Yaml> = paired_at.as_str().map(|s| Yaml::String(s.to_string()));

    {
        let root = data
            .as_mapping_mut()
            .ok_or_else(|| "config root is not a mapping".to_string())?;

        // video.wfb — the canonical pair state.
        let wfb = section_mut(root, "video", "wfb")?;
        match &peer_yaml {
            Some(p) => {
                wfb.insert(Yaml::String("paired_with_device_id".to_string()), p.clone());
            }
            None => {
                wfb.remove("paired_with_device_id");
            }
        }
        match &paired_at_yaml {
            Some(pa) => {
                wfb.insert(Yaml::String("paired_at".to_string()), pa.clone());
            }
            None => {
                wfb.remove("paired_at");
            }
        }
        wfb.insert(
            Yaml::String("auto_pair_enabled".to_string()),
            Yaml::Bool(auto_pair_enabled),
        );

        // GS-profile legacy mirror under ground_station.*.
        if role == "gs" {
            let gs = top_section_mut(root, "ground_station")?;
            match &peer_yaml {
                None => {
                    gs.remove("paired_drone_id");
                    gs.remove("paired_at");
                }
                Some(p) => {
                    gs.insert(Yaml::String("paired_drone_id".to_string()), p.clone());
                    // The residual writes `gs["paired_at"] = paired_at` whenever the
                    // peer is present — a string when present, else a YAML null (not a
                    // key removal). Mirror both.
                    let pa_value = paired_at_yaml.clone().unwrap_or(Yaml::Null);
                    gs.insert(Yaml::String("paired_at".to_string()), pa_value);
                }
            }
        }
    }

    let body = serde_norway::to_string(&data).map_err(|e| e.to_string())?;
    write_atomic(config_path, body.as_bytes())
}

/// Navigate/create a nested `parent.child` mapping, returning the child as a
/// mutable mapping. A node along the path that exists but is not a mapping is
/// replaced with an empty mapping (matching the residual `_get_section`, which
/// overwrites a non-dict section with `{}`). Only the document root being a
/// non-mapping fails (handled by the caller's earlier `as_mapping_mut`).
fn section_mut<'a>(
    root: &'a mut serde_norway::Mapping,
    parent: &str,
    child: &str,
) -> Result<&'a mut serde_norway::Mapping, String> {
    use serde_norway::{Mapping, Value as Yaml};
    let parent_node = root
        .entry(Yaml::String(parent.to_string()))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    if !parent_node.is_mapping() {
        *parent_node = Yaml::Mapping(Mapping::new());
    }
    let parent_map = parent_node
        .as_mapping_mut()
        .ok_or_else(|| format!("{parent} section is not a mapping"))?;
    let child_node = parent_map
        .entry(Yaml::String(child.to_string()))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    if !child_node.is_mapping() {
        *child_node = Yaml::Mapping(Mapping::new());
    }
    child_node
        .as_mapping_mut()
        .ok_or_else(|| format!("{child} section is not a mapping"))
}

/// Navigate/create a top-level mapping section, returning it as a mutable mapping.
/// A non-mapping section is replaced with an empty mapping (the residual
/// `_get_section` behavior).
fn top_section_mut<'a>(
    root: &'a mut serde_norway::Mapping,
    key: &str,
) -> Result<&'a mut serde_norway::Mapping, String> {
    use serde_norway::{Mapping, Value as Yaml};
    let node = root
        .entry(Yaml::String(key.to_string()))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    if !node.is_mapping() {
        *node = Yaml::Mapping(Mapping::new());
    }
    node.as_mapping_mut()
        .ok_or_else(|| format!("{key} section is not a mapping"))
}

/// Write `bytes` to `path` atomically: ensure the parent dir, write a `.tmp`
/// sibling, then rename over the target. Mirrors the residual tmp-write +
/// `os.rename` idiom. Returns `Err(message)` on any I/O fault.
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
// PUT /api/wfb/pair/auto-pair — toggle the arm flag.
// ---------------------------------------------------------------------------

/// The `PUT /api/wfb/pair/auto-pair` request body. Mirrors the residual
/// `AutoPairToggleRequest`: a single required `enabled` bool.
#[derive(Debug, Deserialize)]
pub struct AutoPairToggleRequest {
    pub enabled: bool,
}

/// `PUT /api/wfb/pair/auto-pair` → toggle the auto-bind arm flag.
///
/// Resolves the role from the profile, computes the live pair status, and either
/// refuses a re-arm on a paired rig (returning the status with `rearm_blocked:
/// true`, no persist) or persists the new flag and returns the status with the
/// requested value. Always a `200`; the body is the pair-status field set with the
/// resolved arm flag (plus `rearm_blocked` on the refuse path).
pub async fn put_auto_pair(
    State(state): State<AppState>,
    Json(req): Json<AutoPairToggleRequest>,
) -> Response {
    let cfg = crate::config::PairingConfig::load_from(&state.pairing_paths.config);
    let role = current_role(&cfg.agent.profile);
    put_auto_pair_at(
        &config_yaml_path(),
        &state.pairing_paths.wfb_key_dir,
        &role,
        req.enabled,
    )
}

/// The auto-pair toggle logic against explicit config + key-dir paths + a resolved
/// role. The public handler resolves all three from the app state / env; this
/// takes them directly so a test can point them at temp paths.
fn put_auto_pair_at(config_path: &Path, key_dir: &Path, role: &str, enabled: bool) -> Response {
    let status = read_pair_status(config_path, key_dir, role);

    // Re-arm on a paired rig is refused: the status snapshot with auto_pair_enabled
    // forced false and rearm_blocked added. NOTHING is persisted.
    if enabled && status.paired {
        return Json(json!({
            "paired": status.paired,
            "paired_with_device_id": status.peer,
            "paired_at": status.paired_at,
            "fingerprint": status.fingerprint,
            "auto_pair_enabled": false,
            "rearm_blocked": true,
            "role": status.role,
        }))
        .into_response();
    }

    // Persist the new flag from the values the status read computed. The result is
    // ignored: a non-root front (EPERM on the 0600 config) lands the same response,
    // matching the residual route, which ignores `_save_config_dict`'s boolean.
    let _ = persist_pair_state(config_path, role, &status.peer, &status.paired_at, enabled);

    Json(json!({
        "paired": status.paired,
        "paired_with_device_id": status.peer,
        "paired_at": status.paired_at,
        "fingerprint": status.fingerprint,
        "auto_pair_enabled": enabled,
        "role": status.role,
    }))
    .into_response()
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

    // ── role resolution ───────────────────────────────────────────────────────

    #[test]
    fn role_is_drone_only_for_the_drone_profile() {
        let _env = crate::lock_env_blocking();
        let dir = tempfile::tempdir().unwrap();
        // No profile.conf / mesh role sentinels around.
        std::env::set_var("ADOS_PROFILE_CONF", dir.path().join("absent.conf"));
        std::env::set_var("ADOS_MESH_ROLE", dir.path().join("absent.role"));
        assert_eq!(current_role("drone"), "drone");
        assert_eq!(current_role("ground_station"), "gs");
        // auto/empty with no sentinel falls back to drone → "drone".
        assert_eq!(current_role("auto"), "drone");
        std::env::remove_var("ADOS_PROFILE_CONF");
        std::env::remove_var("ADOS_MESH_ROLE");
    }

    // ── the disable path (enabled=false): always persists ─────────────────────

    #[tokio::test]
    async fn disable_on_an_unpaired_drone_persists_false_and_echoes_status() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        std::fs::create_dir_all(&keys).unwrap();
        // A drone with no key file, an existing auto-pair flag + an unrelated key.
        std::fs::write(
            &cfg,
            "agent:\n  name: my-drone\nvideo:\n  wfb:\n    channel: 149\n    auto_pair_enabled: true\n",
        )
        .unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, "drone", false);
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
        std::fs::create_dir_all(&keys).unwrap();
        // Unpaired (no key) → enable is allowed and persists true.
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, "drone", true);
        let body = body_json(resp).await;
        assert_eq!(body["auto_pair_enabled"], json!(true));
        assert!(body.get("rearm_blocked").is_none());
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
        std::fs::create_dir_all(&keys).unwrap();
        let fp = write_key(&keys, "tx.key");
        // A paired drone (tx.key present) with a peer + a disarmed flag on disk.
        std::fs::write(
            &cfg,
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: false\n",
        )
        .unwrap();
        let before = std::fs::read_to_string(&cfg).unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, "drone", true);
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
            })
        );
        // The file is unchanged — the refuse path persists nothing.
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
    }

    #[tokio::test]
    async fn disable_on_a_paired_drone_is_allowed_and_persists() {
        // enabled=false is never blocked, even when paired.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        std::fs::create_dir_all(&keys).unwrap();
        write_key(&keys, "tx.key");
        std::fs::write(
            &cfg,
            "agent:\n  profile: drone\nvideo:\n  wfb:\n    paired_with_device_id: peer-xyz\n    auto_pair_enabled: true\n",
        )
        .unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, "drone", false);
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

    // ── GS profile: the legacy ground_station.* mirror on persist ─────────────

    #[tokio::test]
    async fn gs_persist_mirrors_the_peer_onto_ground_station() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        std::fs::create_dir_all(&keys).unwrap();
        // A GS with a peer recorded under the canonical spot, unpaired (no rx.key),
        // so the enable persist runs (not blocked).
        std::fs::write(
            &cfg,
            "agent:\n  profile: ground_station\nvideo:\n  wfb:\n    paired_with_device_id: drone-1\n    auto_pair_enabled: false\n",
        )
        .unwrap();

        let resp = put_auto_pair_at(&cfg, &keys, "gs", true);
        let body = body_json(resp).await;
        assert_eq!(body["auto_pair_enabled"], json!(true));
        assert_eq!(body["role"], json!("gs"));
        assert_eq!(body["paired_with_device_id"], json!("drone-1"));

        // The peer mirrors onto ground_station.paired_drone_id.
        let parsed: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            parsed
                .get("ground_station")
                .and_then(|g| g.get("paired_drone_id"))
                .and_then(|v| v.as_str()),
            Some("drone-1")
        );
        assert_eq!(
            parsed
                .get("video")
                .and_then(|v| v.get("wfb"))
                .and_then(|w| w.get("auto_pair_enabled"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    // ── status read parity: paired-at demotion + auto-pair default ────────────

    #[test]
    fn status_demotes_a_yaml_timestamp_paired_at_to_null() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    paired_at: 2026-06-13T07:59:59+00:00\n    paired_with_device_id: drone-abc\n",
        )
        .unwrap();
        let st = read_pair_status(&cfg, &keys, "drone");
        // The timestamp-shaped paired_at demotes to null; the peer passes through.
        assert_eq!(st.paired_at, Value::Null);
        assert_eq!(st.peer, json!("drone-abc"));
        // Absent auto-pair flag defaults true.
        assert!(st.auto_pair_enabled);
        assert!(!st.paired);
    }

    #[test]
    fn status_reads_the_gs_legacy_peer_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        std::fs::create_dir_all(&keys).unwrap();
        // No canonical peer, only the legacy ground_station.paired_drone_id.
        std::fs::write(&cfg, "ground_station:\n  paired_drone_id: drone-legacy\n").unwrap();
        let st = read_pair_status(&cfg, &keys, "gs");
        assert_eq!(st.peer, json!("drone-legacy"));
    }

    // ── persist: a null peer pops the keys ────────────────────────────────────

    #[tokio::test]
    async fn disable_with_no_peer_leaves_no_peer_keys() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let keys = dir.path().join("wfb");
        std::fs::create_dir_all(&keys).unwrap();
        // Unpaired, no peer → the persist pops paired_with_device_id / paired_at.
        std::fs::write(&cfg, "agent:\n  profile: drone\n").unwrap();
        let resp = put_auto_pair_at(&cfg, &keys, "drone", false);
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
