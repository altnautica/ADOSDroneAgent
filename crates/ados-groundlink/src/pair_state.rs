//! WFB radio-link pair state for the ground station.
//!
//! Ports the `gs` legs of `pair_manager.py`: install a 64-byte rx-side wfb-ng
//! key, persist the pair state, drop the setup-complete sentinel, restart the
//! receive unit; and the unpair path that wipes both key files and clears the
//! persisted pair state. The drone legs stay where they are (the drone profile
//! does not run this service); this module is the receive-side half the native
//! front forwards to over the command socket.
//!
//! The wire format wfb-ng requires is the 64-byte libsodium crypto_box keypair
//! file (`gs.key` from `wfb_keygen`); the GS persists those bytes at
//! `/etc/ados/wfb/rx.key`. The public-key fingerprint is `blake2b(pub_half,
//! digest_size=8)` rendered as 16 lowercase hex chars, byte-identical to the
//! radio manager's `read_public_fingerprint`.

use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{json, Value};

use ados_supervisor::process_manager::select;

use crate::paths::{CONFIG_YAML, SETUP_COMPLETE_PATH};

/// The 64-byte wfb-ng key file size. Mirrors `key_mgr.WFB_KEY_FILE_BYTES`.
const WFB_KEY_FILE_BYTES: usize = 64;

/// The offset of the peer-public half within a 64-byte key file. Mirrors
/// `key_mgr.WFB_PUBLIC_HALF_OFFSET`.
const WFB_PUBLIC_HALF_OFFSET: usize = 32;

/// The GS receive systemd unit restarted on a key change. Mirrors the Python
/// `pair_manager._WFB_GS_UNIT`.
const WFB_GS_UNIT: &str = "ados-wfb-rx.service";

/// A pair-key install failure, mapped to the FastAPI error codes by the caller.
#[derive(Debug)]
pub enum PairError {
    /// The base64 blob failed to decode.
    BadBase64(String),
    /// The decoded blob is not exactly 64 bytes.
    BadBlob(String),
    /// A file write / read fault.
    Io(String),
}

/// The wfb key directory (`/etc/ados/wfb`), honouring `ADOS_WFB_KEY_DIR` for
/// tests. Mirrors `key_mgr.get_key_paths`' base.
fn key_dir() -> PathBuf {
    std::env::var("ADOS_WFB_KEY_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(ados_radio::paths::WFB_KEY_DIR))
}

/// The config path the pair-state persist round-trips, honouring `ADOS_CONFIG`.
fn config_path() -> PathBuf {
    std::env::var("ADOS_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(CONFIG_YAML))
}

/// The setup-complete sentinel path, honouring `ADOS_SETUP_COMPLETE` for tests.
fn setup_complete_path() -> PathBuf {
    std::env::var("ADOS_SETUP_COMPLETE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(SETUP_COMPLETE_PATH))
}

/// The on-disk paths the pair/unpair flow touches, resolved once from the env
/// seams and threaded into the path-injectable cores. A test builds one pointing at
/// a tempdir, so no test mutates the process environment.
struct PairPaths {
    /// The agent config the pair-state persist round-trips.
    config: PathBuf,
    /// The wfb key directory holding `tx.key` / `rx.key`.
    wfb_key_dir: PathBuf,
    /// The setup-complete sentinel dropped on a successful pair.
    setup_complete: PathBuf,
}

impl PairPaths {
    /// The drone-side (tx) key file path.
    fn tx_key(&self) -> PathBuf {
        self.wfb_key_dir.join("tx.key")
    }
    /// The ground-station (rx) key file path.
    fn rx_key(&self) -> PathBuf {
        self.wfb_key_dir.join("rx.key")
    }
}

/// Resolve the pair paths from the live env seams (the production layout).
fn pair_paths() -> PairPaths {
    PairPaths {
        config: config_path(),
        wfb_key_dir: key_dir(),
        setup_complete: setup_complete_path(),
    }
}

/// Compute the 16-hex public-key fingerprint of a key file, or `None` when the
/// file is absent / not exactly 64 bytes. The peer-public half is the second 32
/// bytes; the fingerprint is `blake2b(pub, digest_size=8)` rendered as 16
/// lowercase hex chars. Byte-identical to `key_mgr.read_public_fingerprint` and
/// the native `wfb` read route's fingerprint.
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

/// The current UTC timestamp in ISO 8601 form (seconds resolution), matching the
/// Python `datetime.now(UTC).isoformat(timespec="seconds")`.
fn iso_now() -> String {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    // RFC3339 with second resolution: `2026-06-16T12:34:56+00:00`. Python's
    // `isoformat(timespec="seconds")` on a UTC-aware datetime renders the same
    // shape (the `+00:00` offset, not a `Z`).
    let now = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap_or_else(|_| OffsetDateTime::now_utc());
    now.format(&Rfc3339).unwrap_or_default()
}

/// Atomically write `data` to `path` with a specific mode (tmp + chmod + rename).
/// Mirrors `pair_manager._atomic_write`.
fn atomic_write(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        f.write_all(data).map_err(|e| e.to_string())?;
        f.sync_all().map_err(|e| e.to_string())?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Install a 64-byte rx-side key on the GS. Mirrors
/// `PairManager.apply_keypair(blob, "gs", peer_device_id)`:
/// decode + validate the blob, write `rx.key` (0600), persist the pair state
/// (`auto_pair_enabled=false`), drop the setup-complete sentinel, restart the
/// receive unit, and return `{paired, paired_with_device_id, paired_at,
/// fingerprint, role}`.
pub async fn apply_keypair_gs(
    blob_b64: &str,
    peer_device_id: Option<&str>,
) -> Result<Value, PairError> {
    apply_keypair_gs_at(&pair_paths(), blob_b64, peer_device_id).await
}

/// The path-injectable core of [`apply_keypair_gs`]: every on-disk seam is threaded
/// in via `paths`, so a test drives the full install against a tempdir without
/// mutating the process environment.
async fn apply_keypair_gs_at(
    paths: &PairPaths,
    blob_b64: &str,
    peer_device_id: Option<&str>,
) -> Result<Value, PairError> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(blob_b64.as_bytes())
        .map_err(|e| PairError::BadBase64(e.to_string()))?;
    if blob.len() != WFB_KEY_FILE_BYTES {
        return Err(PairError::BadBlob(format!(
            "key blob is {} bytes, expected {WFB_KEY_FILE_BYTES}",
            blob.len()
        )));
    }

    let rx = paths.rx_key();
    atomic_write(&rx, &blob, 0o600).map_err(PairError::Io)?;
    let fingerprint = read_public_fingerprint(&rx);
    let paired_at = iso_now();

    // A real pair: disarm auto_pair regardless of whether a peer device-id was
    // exchanged (the local radio-bind protocol carries no device-id).
    persist_pair_state_at(&paths.config, peer_device_id, Some(&paired_at), Some(false))
        .map_err(PairError::Io)?;

    // Drop the setup-complete sentinel so captive DNS stands down (best-effort).
    if let Err(e) = atomic_write(
        &paths.setup_complete,
        format!("{paired_at}\n").as_bytes(),
        0o644,
    ) {
        tracing::warn!(error = %e, "setup_complete_sentinel_failed");
    }

    // restart over reload: a unit restart is the prompt path to a fresh spawn
    // cycle that picks up the freshly written key.
    let pm = select();
    if !pm.restart(WFB_GS_UNIT).await {
        tracing::info!(unit = WFB_GS_UNIT, "wfb_unit_restart_skipped");
    }

    Ok(json!({
        "paired": true,
        "paired_with_device_id": peer_device_id,
        "paired_at": paired_at,
        "fingerprint": fingerprint,
        "role": "gs",
    }))
}

/// Wipe both key files and clear the persisted pair state. Mirrors
/// `PairManager.unpair("gs")`: leaves `auto_pair_enabled=false`, restarts the
/// receive unit, returns `{paired: false, role: "gs"}`.
pub async fn unpair_gs() -> Result<Value, String> {
    unpair_gs_at(&pair_paths()).await
}

/// The path-injectable core of [`unpair_gs`], for tests.
async fn unpair_gs_at(paths: &PairPaths) -> Result<Value, String> {
    let tx = paths.tx_key();
    let rx = paths.rx_key();
    for path in [&tx, &rx] {
        if path.is_file() {
            if let Err(e) = std::fs::remove_file(path) {
                tracing::warn!(path = %path.display(), error = %e, "key_delete_failed");
            }
        }
    }
    persist_pair_state_at(&paths.config, None, None, Some(false))?;
    let pm = select();
    let _ = pm.restart(WFB_GS_UNIT).await;
    tracing::warn!(role = "gs", "unpair_complete");
    Ok(json!({"paired": false, "role": "gs"}))
}

/// Merge the persisted pair fields under `video.wfb` (canonical) and mirror onto
/// `ground_station.{paired_drone_id,paired_at}` for the GS profile. Mirrors
/// `pair_manager._persist_pair_state` for `role == "gs"`. A `None` peer/paired-at
/// clears the field; `auto_pair_enabled` is set when supplied. The merge
/// preserves every other config key and is atomic (tmp + rename), 0600.
fn persist_pair_state_at(
    path: &Path,
    peer_device_id: Option<&str>,
    paired_at: Option<&str>,
    auto_pair_enabled: Option<bool>,
) -> Result<(), String> {
    use serde_norway::Value as Yaml;

    let mut data: Yaml = match std::fs::read_to_string(path) {
        Ok(text) => match serde_norway::from_str::<Yaml>(&text) {
            Ok(v) if v.is_mapping() => v,
            _ => Yaml::Mapping(serde_norway::Mapping::new()),
        },
        Err(_) => Yaml::Mapping(serde_norway::Mapping::new()),
    };

    {
        let wfb = video_wfb_section_mut(&mut data).ok_or("config root is not a mapping")?;
        set_or_remove_str(wfb, "paired_with_device_id", peer_device_id);
        set_or_remove_str(wfb, "paired_at", paired_at);
        if let Some(flag) = auto_pair_enabled {
            wfb.insert(
                Yaml::String("auto_pair_enabled".to_string()),
                Yaml::Bool(flag),
            );
        }
    }
    {
        let gs = ground_station_section_mut(&mut data).ok_or("config root is not a mapping")?;
        if peer_device_id.is_none() {
            gs.remove(Yaml::String("paired_drone_id".to_string()));
            gs.remove(Yaml::String("paired_at".to_string()));
        } else {
            set_or_remove_str(gs, "paired_drone_id", peer_device_id);
            set_or_remove_str(gs, "paired_at", paired_at);
        }
    }

    write_config_atomic(path, &data)
}

/// Set a string field, or remove it when the value is `None`. Mirrors the
/// Python `wfb.pop(key)` / `wfb[key] = value` pattern.
fn set_or_remove_str(map: &mut serde_norway::Mapping, key: &str, value: Option<&str>) {
    use serde_norway::Value as Yaml;
    let k = Yaml::String(key.to_string());
    match value {
        Some(v) => {
            map.insert(k, Yaml::String(v.to_string()));
        }
        None => {
            map.remove(k);
        }
    }
}

/// Navigate/create `video.wfb` as a mutable mapping. `None` only when the root is
/// not a mapping (mirrors the create-on-conflict behaviour of the tx-power /
/// gs-wfb merges).
fn video_wfb_section_mut(data: &mut serde_norway::Value) -> Option<&mut serde_norway::Mapping> {
    section_path_mut(data, &["video", "wfb"])
}

/// Navigate/create the `ground_station` mapping.
fn ground_station_section_mut(
    data: &mut serde_norway::Value,
) -> Option<&mut serde_norway::Mapping> {
    section_path_mut(data, &["ground_station"])
}

/// Navigate/create a nested mapping path, replacing a non-mapping node along the
/// way with an empty mapping. Returns `None` only when the document root is not a
/// mapping.
fn section_path_mut<'a>(
    data: &'a mut serde_norway::Value,
    path: &[&str],
) -> Option<&'a mut serde_norway::Mapping> {
    use serde_norway::Value as Yaml;
    let mut cur = data.as_mapping_mut()?;
    for (i, key) in path.iter().enumerate() {
        let entry = cur
            .entry(Yaml::String((*key).to_string()))
            .or_insert_with(|| Yaml::Mapping(serde_norway::Mapping::new()));
        if !entry.is_mapping() {
            *entry = Yaml::Mapping(serde_norway::Mapping::new());
        }
        cur = entry.as_mapping_mut()?;
        let _ = i;
    }
    Some(cur)
}

/// Serialize + atomically write the config (tmp + rename), 0600 (the file carries
/// secrets). Mirrors `pair_manager._save_config_dict`'s atomic write (without the
/// flock, which the front's single-writer position makes unnecessary).
fn write_config_atomic(path: &Path, data: &serde_norway::Value) -> Result<(), String> {
    let body = serde_norway::to_string(data).map_err(|e| e.to_string())?;
    atomic_write(path, body.as_bytes(), 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A tempdir wired as a [`PairPaths`] pointing the config + key-dir + sentinel
    /// seams into it. Every path is threaded into the `_at` cores, so no test
    /// mutates the process environment and none can race a sibling test.
    struct Env {
        _dir: tempfile::TempDir,
        paths: PairPaths,
    }
    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let paths = PairPaths {
            config: dir.path().join("config.yaml"),
            wfb_key_dir: dir.path().join("wfb"),
            setup_complete: dir.path().join("setup-complete"),
        };
        Env { _dir: dir, paths }
    }

    fn b64_64_bytes(byte: u8) -> String {
        base64::engine::general_purpose::STANDARD.encode([byte; 64])
    }

    #[test]
    fn fingerprint_matches_the_blake2b8_of_the_public_half() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("rx.key");
        // First 32 bytes = secret half, last 32 = public half. The fingerprint is
        // over the public half only.
        let mut bytes = vec![1u8; 32];
        bytes.extend(std::iter::repeat_n(7u8, 32));
        std::fs::write(&key, &bytes).unwrap();
        let fp = read_public_fingerprint(&key).unwrap();
        // 16 lowercase hex chars.
        assert_eq!(fp.len(), 16);
        assert!(fp
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Independent blake2b-8 over the public half (bytes 32..64 = all 7s).
        use blake2::digest::{Update, VariableOutput};
        use blake2::Blake2bVar;
        let mut h = Blake2bVar::new(8).unwrap();
        h.update(&[7u8; 32]);
        let mut out = [0u8; 8];
        h.finalize_variable(&mut out).unwrap();
        assert_eq!(fp, hex::encode(out));
        // A short file has no fingerprint.
        std::fs::write(&key, b"short").unwrap();
        assert!(read_public_fingerprint(&key).is_none());
    }

    #[tokio::test]
    async fn apply_keypair_rejects_bad_base64() {
        let e = env();
        let err = apply_keypair_gs_at(&e.paths, "!!!not-base64!!!", None).await;
        assert!(matches!(err, Err(PairError::BadBase64(_))));
    }

    #[tokio::test]
    async fn apply_keypair_rejects_a_wrong_size_blob() {
        let e = env();
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let err = apply_keypair_gs_at(&e.paths, &short, None).await;
        match err {
            Err(PairError::BadBlob(msg)) => assert!(msg.contains("32 bytes")),
            other => panic!("expected BadBlob, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_keypair_writes_the_key_0600_and_persists_state() {
        let e = env();
        let reply = apply_keypair_gs_at(&e.paths, &b64_64_bytes(9), Some("drone-7"))
            .await
            .unwrap();
        // The reply shape matches the Python apply_keypair return.
        assert_eq!(reply["paired"], true);
        assert_eq!(reply["paired_with_device_id"], "drone-7");
        assert_eq!(reply["role"], "gs");
        assert!(reply["fingerprint"].as_str().unwrap().len() == 16);
        assert!(reply["paired_at"].as_str().is_some());

        // rx.key written 0600 with the 64 bytes.
        let rx = e.paths.rx_key();
        let mode = std::fs::metadata(&rx).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read(&rx).unwrap(), vec![9u8; 64]);

        // The pair state persisted under video.wfb + ground_station.
        let cfg: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&e.paths.config).unwrap()).unwrap();
        let wfb = cfg.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert_eq!(
            wfb.get("paired_with_device_id").and_then(|v| v.as_str()),
            Some("drone-7")
        );
        assert_eq!(
            wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        let gs = cfg.get("ground_station").unwrap();
        assert_eq!(
            gs.get("paired_drone_id").and_then(|v| v.as_str()),
            Some("drone-7")
        );

        // The setup-complete sentinel was dropped.
        assert!(e.paths.setup_complete.is_file());
    }

    #[tokio::test]
    async fn unpair_wipes_keys_and_clears_state() {
        let e = env();
        // Pair first.
        apply_keypair_gs_at(&e.paths, &b64_64_bytes(3), Some("drone-x"))
            .await
            .unwrap();
        let tx = e.paths.tx_key();
        let rx = e.paths.rx_key();
        // Drop a stray tx.key too, to prove unpair wipes both.
        std::fs::write(&tx, vec![0u8; 64]).unwrap();

        let reply = unpair_gs_at(&e.paths).await.unwrap();
        assert_eq!(reply, json!({"paired": false, "role": "gs"}));
        assert!(!rx.is_file());
        assert!(!tx.is_file());

        // The persisted pair state is cleared.
        let cfg: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&e.paths.config).unwrap()).unwrap();
        let wfb = cfg.get("video").and_then(|v| v.get("wfb")).unwrap();
        assert!(wfb.get("paired_with_device_id").is_none());
        assert_eq!(
            wfb.get("auto_pair_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        let gs = cfg.get("ground_station").unwrap();
        assert!(gs.get("paired_drone_id").is_none());
    }

    #[tokio::test]
    async fn persist_preserves_unrelated_config_keys() {
        let e = env();
        std::fs::write(
            &e.paths.config,
            "agent:\n  name: gs-1\nvideo:\n  wfb:\n    channel: 149\n",
        )
        .unwrap();
        apply_keypair_gs_at(&e.paths, &b64_64_bytes(2), None)
            .await
            .unwrap();
        let cfg: serde_norway::Value =
            serde_norway::from_str(&std::fs::read_to_string(&e.paths.config).unwrap()).unwrap();
        // The unrelated agent.name + the existing channel survive.
        assert_eq!(
            cfg.get("agent")
                .and_then(|a| a.get("name"))
                .and_then(|n| n.as_str()),
            Some("gs-1")
        );
        assert_eq!(
            cfg.get("video")
                .and_then(|v| v.get("wfb"))
                .and_then(|w| w.get("channel"))
                .and_then(|c| c.as_i64()),
            Some(149)
        );
    }
}
