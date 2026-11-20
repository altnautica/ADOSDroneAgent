//! The radio-pair state every pair surface answers from.
//!
//! `GET /api/wfb/pair`, the `PUT /api/wfb/pair/auto-pair` toggle and
//! `GET /api/pairing/info` all ask the same question — is this rig radio-paired,
//! and with whom — so they read one module:
//!
//! - [`bind_role`] resolves the bind-protocol role (`"drone"` / `"gs"`) off the
//!   agent profile;
//! - [`role_key_fingerprint`] (`ados_protocol::pair_proof`) is the single
//!   paired predicate: the role's own key file (`tx.key` on a drone, `rx.key`
//!   on a ground station), exactly 64 bytes, with a readable blake2b-8
//!   fingerprint;
//! - [`status`] is the full snapshot: that predicate plus the peer, `paired_at`
//!   and the auto-pair flag off the config, with the legacy `ground_station.*`
//!   fallback on the GS profile.
//!
//! The snapshot is a read projection and is lossy (a timestamp-shaped
//! `paired_at` reads as null), so nothing writes it back to disk: the pair record
//! belongs to the bind that wrote it.

use std::path::Path;

use ados_protocol::pair_proof::{rearm_view_at, role_key_fingerprint, PAIR_PROOF_PATH};
use ados_protocol::wfb_status::json_truthy;
use serde_json::{json, Map, Value};

use crate::state::PairingPaths;

/// The bind-protocol role for a resolved (hyphen-wire) profile: `"drone"` only
/// for the drone profile, `"gs"` otherwise.
pub(crate) fn bind_role_for(profile: &str) -> &'static str {
    if profile == "drone" {
        "drone"
    } else {
        "gs"
    }
}

/// Resolve the bind-protocol role off the agent config, profile.conf and the
/// mesh-role sentinel the paths name.
pub(crate) fn bind_role(paths: &PairingPaths) -> &'static str {
    let cfg = crate::config::PairingConfig::load_from(&paths.config);
    let (profile, _role) = crate::profile::current_profile_and_role_at(
        &cfg.agent.profile,
        &paths.profile_conf,
        &paths.mesh_role,
    );
    bind_role_for(&profile)
}

/// The live pair-status snapshot for a role.
#[derive(Debug)]
pub(crate) struct PairStatus {
    pub paired: bool,
    /// The peer device-id off the config (or the GS legacy mirror), or null.
    pub peer: Value,
    /// The paired-at string off the config, null for a non-string or a
    /// YAML-timestamp-shaped value.
    pub paired_at: Value,
    /// The blake2b-8 key fingerprint, or null when not paired.
    pub fingerprint: Value,
    /// The stored arm flag (`video.wfb.auto_pair_enabled`, default true).
    pub auto_pair_enabled: bool,
    /// `"drone"` or `"gs"`.
    pub role: &'static str,
}

impl PairStatus {
    /// The snapshot as the JSON object the pair routes serve.
    pub(crate) fn to_json(&self) -> Map<String, Value> {
        Map::from_iter([
            ("paired".to_string(), Value::Bool(self.paired)),
            ("paired_with_device_id".to_string(), self.peer.clone()),
            ("paired_at".to_string(), self.paired_at.clone()),
            ("fingerprint".to_string(), self.fingerprint.clone()),
            (
                "auto_pair_enabled".to_string(),
                Value::Bool(self.auto_pair_enabled),
            ),
            ("role".to_string(), Value::from(self.role)),
        ])
    }
}

/// Compute the pair-status snapshot for `role` off the config and key dir.
pub(crate) fn status(config_path: &Path, key_dir: &Path, role: &'static str) -> PairStatus {
    let fingerprint = role_key_fingerprint(key_dir, role);
    let paired = fingerprint.is_some();
    let fingerprint = fingerprint.map_or(Value::Null, Value::String);

    // A present-but-non-string peer/paired-at reads as null; an absent arm flag
    // defaults to true.
    let raw = crate::config::load_config_object(config_path);
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

    // GS-profile fallback: a rig migrated from an older config may carry the
    // pair state under `ground_station.*` without the `video.wfb.*` mirror.
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
        role,
    }
}

/// The auto-pair re-arm the latch last took for the key on disk
/// (`ados_protocol::pair_proof::rearm_view_at`, the view the cloud heartbeat
/// carries too), served as `pair_rearm` on the radio status reads (`GET
/// /api/wfb` and the ground-station `/status` `link` block):
/// `{cause, at, episodes, active}`, or `null` when this key was never
/// re-armed.
pub(crate) fn rearm_view(paths: &PairingPaths, role: &str) -> Value {
    rearm_view_at(
        Path::new(PAIR_PROOF_PATH),
        &paths.bind_state,
        &paths.wfb_key_dir,
        role,
    )
    .and_then(|v| serde_json::to_value(v).ok())
    .unwrap_or(Value::Null)
}

/// The `paired_at` value the snapshot reports.
///
/// The bind writes the timestamp unquoted, which a standard YAML loader resolves
/// to a datetime rather than a string, so the pair-status read has always
/// reported it as null. This YAML parser flattens timestamps back to strings, so
/// a timestamp-shaped string is demoted to null here to keep that contract. A
/// non-string value is null; any other string passes through.
fn paired_at_string(v: &Value) -> Value {
    match v.as_str() {
        Some(s) if !is_yaml_timestamp(s) => json!(s),
        _ => Value::Null,
    }
}

/// True when `s` matches the YAML implicit timestamp grammar: a bare
/// `YYYY-MM-DD` date, or `YYYY-M-D` (one- or two-digit month/day) followed by a
/// `T`/whitespace separator, `H:MM:SS`, an optional fractional second, and an
/// optional `Z` or numeric timezone offset.
fn is_yaml_timestamp(s: &str) -> bool {
    let b = s.as_bytes();

    // Read a run of ASCII digits from `i`, returning the count consumed.
    fn digits(b: &[u8], i: usize) -> usize {
        let mut j = i;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        j - i
    }

    // The date head is mandatory: four digits, `-`, then month/day groups.
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

    // Bare date: exactly `YYYY-MM-DD` with nothing trailing.
    if i == s.len() {
        return month == 2 && day == 2;
    }

    // Datetime: a `T`/`t` or whitespace separator, then `H:MM:SS`.
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

    // Optional fractional second.
    if b.get(i) == Some(&b'.') {
        i += 1;
        i += digits(b, i);
    }

    // Optional timezone, possibly preceded by whitespace: `Z`/`z`, or a signed
    // `HH` / `HH:MM` offset.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(dir: &Path) -> PairingPaths {
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

    #[test]
    fn role_is_drone_only_for_the_drone_profile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        std::fs::write(&paths.config, "agent:\n  profile: drone\n").unwrap();
        assert_eq!(bind_role(&paths), "drone");
        std::fs::write(&paths.config, "agent:\n  profile: ground_station\n").unwrap();
        assert_eq!(bind_role(&paths), "gs");
        // auto with no profile.conf / sentinel falls back to drone.
        std::fs::write(&paths.config, "agent:\n  profile: auto\n").unwrap();
        assert_eq!(bind_role(&paths), "drone");
    }

    #[test]
    fn unpaired_drone_status_is_the_default_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        std::fs::write(&paths.config, "agent:\n  profile: drone\n").unwrap();
        let st = status(&paths.config, &paths.wfb_key_dir, "drone");
        assert_eq!(
            Value::Object(st.to_json()),
            json!({
                "paired": false,
                "paired_with_device_id": null,
                "paired_at": null,
                "fingerprint": null,
                "auto_pair_enabled": true,
                "role": "drone",
            })
        );
    }

    #[test]
    fn status_demotes_a_yaml_timestamp_paired_at_to_null() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        std::fs::write(
            &paths.config,
            "video:\n  wfb:\n    paired_at: 2026-06-13T07:59:59+00:00\n    paired_with_device_id: drone-abc\n",
        )
        .unwrap();
        let st = status(&paths.config, &paths.wfb_key_dir, "drone");
        assert_eq!(st.paired_at, Value::Null);
        assert_eq!(st.peer, json!("drone-abc"));
        assert!(st.auto_pair_enabled);
        assert!(!st.paired);
    }

    #[test]
    fn status_reads_the_gs_legacy_peer_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        std::fs::write(
            &paths.config,
            "ground_station:\n  paired_drone_id: drone-legacy\n",
        )
        .unwrap();
        assert_eq!(
            status(&paths.config, &paths.wfb_key_dir, "gs").peer,
            json!("drone-legacy")
        );
        // The legacy mirror is a ground-station fallback only.
        assert_eq!(
            status(&paths.config, &paths.wfb_key_dir, "drone").peer,
            Value::Null
        );
    }

    #[test]
    fn status_of_a_paired_rig_reports_its_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        std::fs::create_dir_all(&paths.wfb_key_dir).unwrap();
        std::fs::write(paths.wfb_key_dir.join("tx.key"), [3u8; 64]).unwrap();
        let st = status(&paths.config, &paths.wfb_key_dir, "drone");
        assert!(st.paired);
        assert_eq!(
            st.fingerprint,
            json!(
                ados_protocol::pair_proof::key_fingerprint(&paths.wfb_key_dir.join("tx.key"))
                    .unwrap()
            )
        );
    }

    #[test]
    fn is_yaml_timestamp_matches_the_loader_resolution() {
        assert!(is_yaml_timestamp("2026-06-13T07:59:59+00:00"));
        assert!(is_yaml_timestamp("2026-06-13"));
        assert!(is_yaml_timestamp("2026-06-13 07:59:59"));
        assert!(is_yaml_timestamp("2026-06-13T07:59:59"));
        assert!(is_yaml_timestamp("2026-06-13T07:59:59.123456+05:30"));
        assert!(is_yaml_timestamp("2026-06-13t07:59:59z"));

        assert!(!is_yaml_timestamp("not-a-date"));
        assert!(!is_yaml_timestamp("unknown"));
        assert!(!is_yaml_timestamp("drone-abc"));
        assert!(!is_yaml_timestamp("07:59:59"));
        assert!(!is_yaml_timestamp("2026-06-13X07:59:59"));
        assert!(!is_yaml_timestamp("2026-06-13T07:59"));
        assert!(!is_yaml_timestamp(""));
    }

    #[test]
    fn paired_at_string_passes_a_non_timestamp_and_nulls_non_strings() {
        assert_eq!(
            paired_at_string(&json!("custom-label")),
            json!("custom-label")
        );
        assert_eq!(
            paired_at_string(&json!("2026-06-13T07:59:59+00:00")),
            Value::Null
        );
        assert_eq!(paired_at_string(&json!(123)), Value::Null);
        assert_eq!(paired_at_string(&json!(true)), Value::Null);
        assert_eq!(paired_at_string(&Value::Null), Value::Null);
    }
}
