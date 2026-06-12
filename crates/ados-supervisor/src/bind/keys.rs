//! WFB key-apply + persisted pair-state. Ports the key half of
//! `ground_station/pair_manager.py` + the fingerprint from `wfb/key_mgr.py`.
//!
//! Parity-critical, called once at `RESTARTING_SERVICES`:
//!   1. validate the inbound blob is exactly 64 bytes (libsodium crypto_box);
//!   2. atomic 0600 write to the role's canonical key (`tx.key`/`rx.key`);
//!   3. blake2b-8 fingerprint of the peer-public half (16 hex chars);
//!   4. persist `video.wfb.{paired_with_device_id,paired_at,auto_pair_enabled}`
//!      (+ the `ground_station.*` mirror on GS) under an flock + atomic rewrite;
//!   5. drop the setup-complete sentinel; restart the normal wfb unit.
//!
//! Three invariants the Python comments call out, preserved here:
//!   - `auto_pair_enabled` is disarmed **unconditionally** on a real key write,
//!     never gated on `peer_device_id` (a local bind carries no device-id;
//!     gating left every local bind armed → the next boot re-bound and wiped the
//!     fresh key → pairings evaporated across reboot);
//!   - the config rewrite is flock-serialised + atomic + 0600 + euid-0 so a
//!     non-locked writer racing a GCS PUT cannot silently lose a write;
//!   - the key write is atomic (tmp + fsync + chmod + rename).

use std::path::Path;

use serde_norway::{Mapping, Value};

use super::fsm::iso_now;
use super::BindRole;

/// libsodium crypto_box key-file size: 32-byte secret + 32-byte peer-public.
pub const WFB_KEY_FILE_BYTES: usize = 64;
/// Offset of the peer-public half used for the fingerprint.
pub const WFB_PUBLIC_HALF_OFFSET: usize = 32;

/// Outcome of a successful [`apply_keypair`].
#[derive(Debug, Clone)]
pub struct PairResult {
    pub paired: bool,
    pub peer_device_id: Option<String>,
    pub paired_at: String,
    pub fingerprint: Option<String>,
    pub role: BindRole,
}

/// Reject anything that is not exactly a 64-byte key file. Mirrors
/// `_validate_blob` — wfb-ng silently fails decryption on a wrong-shaped key.
pub fn validate_blob(blob: &[u8]) -> Result<(), String> {
    if blob.len() != WFB_KEY_FILE_BYTES {
        return Err(format!(
            "key blob is {} bytes, expected {WFB_KEY_FILE_BYTES}",
            blob.len()
        ));
    }
    Ok(())
}

/// Atomic write with an explicit mode: tmp sibling (`<path>.tmp`) + fsync +
/// chmod + rename. Creates the parent. Mirrors `_atomic_write`.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    // Re-chmod in case a umask altered the create-time mode (matches Python).
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// blake2b-8 fingerprint (16 lowercase hex chars) of the peer-public half.
/// Both rigs of a pair compute the same value, so a heartbeat cross-check is a
/// string compare. Mirrors `read_public_fingerprint`.
pub fn read_public_fingerprint(path: &Path) -> Result<String, String> {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;

    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() != WFB_KEY_FILE_BYTES {
        return Err(format!(
            "key file at {} is {} bytes, expected {WFB_KEY_FILE_BYTES}",
            path.display(),
            data.len()
        ));
    }
    let pub_half = &data[WFB_PUBLIC_HALF_OFFSET..];
    let mut hasher = Blake2bVar::new(8).map_err(|e| e.to_string())?;
    hasher.update(pub_half);
    let mut out = [0u8; 8];
    hasher
        .finalize_variable(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(hex::encode(out))
}

/// Load `config.yaml` as a mapping, tolerating absence / a non-mapping root.
fn load_config_mapping(path: &Path) -> Mapping {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_norway::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_mapping().cloned())
        .unwrap_or_default()
}

/// Get (materialising if absent) the nested mapping at `key`, replacing a
/// non-mapping value the way `_get_section` does.
fn ensure_map<'a>(parent: &'a mut Mapping, key: &str) -> &'a mut Mapping {
    if !matches!(parent.get(key), Some(Value::Mapping(_))) {
        parent.insert(
            Value::String(key.to_string()),
            Value::Mapping(Mapping::new()),
        );
    }
    parent
        .get_mut(key)
        .and_then(Value::as_mapping_mut)
        .expect("ensured mapping present")
}

fn set_str(map: &mut Mapping, key: &str, val: &str) {
    map.insert(
        Value::String(key.to_string()),
        Value::String(val.to_string()),
    );
}

/// Apply the pair fields to an in-memory config mapping. Pure — split out from
/// the I/O so it is unit-testable without root / a real config path. Mirrors
/// `_persist_pair_state`'s field logic exactly (set vs pop, the GS mirror).
pub fn apply_pair_fields(
    root: &mut Mapping,
    role: BindRole,
    peer_device_id: Option<&str>,
    paired_at: Option<&str>,
    auto_pair_enabled: Option<bool>,
) {
    {
        let wfb = ensure_map(ensure_map(root, "video"), "wfb");
        match peer_device_id {
            Some(id) => set_str(wfb, "paired_with_device_id", id),
            None => {
                wfb.remove("paired_with_device_id");
            }
        }
        match paired_at {
            Some(p) => set_str(wfb, "paired_at", p),
            None => {
                wfb.remove("paired_at");
            }
        }
        if let Some(b) = auto_pair_enabled {
            wfb.insert(
                Value::String("auto_pair_enabled".to_string()),
                Value::Bool(b),
            );
        }
    }

    if role == BindRole::Gs {
        let gs = ensure_map(root, "ground_station");
        match peer_device_id {
            Some(id) => {
                set_str(gs, "paired_drone_id", id);
                match paired_at {
                    Some(p) => set_str(gs, "paired_at", p),
                    None => {
                        gs.insert(Value::String("paired_at".to_string()), Value::Null);
                    }
                }
            }
            None => {
                gs.remove("paired_drone_id");
                gs.remove("paired_at");
            }
        }
    }
}

/// flock guard for the config rewrite (Linux only).
#[cfg(target_os = "linux")]
fn acquire_config_lock(lock_path: &Path) -> Option<nix::fcntl::Flock<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        // The lock file is a flock target only; never truncate it (matches the
        // Python `O_CREAT | O_WRONLY` open with no `O_TRUNC`).
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .ok()?;
    // Losing the lock is worse than racing (the rename is still atomic), so a
    // failed lock falls through to the write with no guard — matches Python.
    nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive).ok()
}

/// Load → mutate → atomically rewrite the config, serialised by an flock and
/// gated on euid 0 (the file is 0600 root). Returns false (no write) for a
/// non-root caller on Linux, matching `_save_config_dict`.
pub fn persist_pair_state(
    config_path: &Path,
    lock_path: &Path,
    role: BindRole,
    peer_device_id: Option<&str>,
    paired_at: Option<&str>,
    auto_pair_enabled: Option<bool>,
) -> bool {
    #[cfg(target_os = "linux")]
    {
        if !nix::unistd::geteuid().is_root() {
            tracing::error!(
                path = %config_path.display(),
                euid = nix::unistd::geteuid().as_raw(),
                "config_write_requires_root"
            );
            return false;
        }
    }

    #[cfg(target_os = "linux")]
    let _lock = acquire_config_lock(lock_path);
    #[cfg(not(target_os = "linux"))]
    let _ = lock_path;

    let mut root = load_config_mapping(config_path);
    apply_pair_fields(
        &mut root,
        role,
        peer_device_id,
        paired_at,
        auto_pair_enabled,
    );

    let body = match serde_norway::to_string(&Value::Mapping(root)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, path = %config_path.display(), "config_write_failed");
            return false;
        }
    };
    match atomic_write(config_path, body.as_bytes(), 0o600) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(error = %e, path = %config_path.display(), "config_write_failed");
            false
        }
    }
}

/// Persist an inbound 64-byte key file + pair state and prompt the normal wfb
/// unit to pick it up. The async surface mirrors `PairManager.apply_keypair`;
/// the `Err(String)` path is what the orchestrator wraps into a phase-tagged
/// `BindError` at `RESTARTING_SERVICES`.
pub async fn apply_keypair(
    blob: &[u8],
    role: BindRole,
    peer_device_id: Option<&str>,
) -> Result<PairResult, String> {
    validate_blob(blob)?;

    // Stop the consumer unit BEFORE writing the key, and confirm it actually
    // went inactive. The prior write-then-restart order raced the supervisor's
    // own service-recovery loop: a `systemctl restart` issued while a start job
    // is already in flight coalesces into it, leaving a process that loaded the
    // OLD key running — bench-observed as a ground station whose wfb_rx started
    // one second before the new rx.key landed and silently decoded nothing.
    // stop → confirm-inactive → write → start guarantees any process running
    // after the start read the new key.
    let unit = role.normal_unit();
    let _ = crate::systemctl::stop(unit).await;
    let mut confirmed_inactive = false;
    for _ in 0..10 {
        if !crate::systemctl::is_active(unit).await {
            confirmed_inactive = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if !confirmed_inactive {
        tracing::warn!(unit, "wfb_unit_still_active_before_key_write");
    }

    let target = Path::new(role.key_path());
    atomic_write(target, blob, 0o600).map_err(|e| format!("key write failed: {e}"))?;
    let fingerprint = read_public_fingerprint(target).ok();
    let paired_at = iso_now();

    // Disarm auto_pair UNCONDITIONALLY (see module doc) — never gate on peer id.
    // Safe now that the orchestrator's peer-evidence gate keeps an unverified
    // (solo/phantom) bind from ever reaching this apply.
    persist_pair_state(
        Path::new(super::CONFIG_YAML),
        Path::new(super::CONFIG_LOCK_PATH),
        role,
        peer_device_id,
        Some(&paired_at),
        Some(false),
    );

    // Best-effort setup-complete sentinel (captive_dns stops redirecting).
    let body = format!("{paired_at}\n");
    if let Err(e) = atomic_write(
        Path::new(super::SETUP_COMPLETE_PATH),
        body.as_bytes(),
        0o644,
    ) {
        tracing::warn!(error = %e, "setup_complete_sentinel_failed");
    }

    // Start (not restart): the unit was stopped above, so this always spawns a
    // fresh process tree that reads the key written this session.
    if !crate::systemctl::start(unit).await {
        tracing::info!(
            unit,
            "wfb_unit_start_skipped (unit may not be installed yet)"
        );
    }

    // The radio re-brings-up with the new key, but the drone's video pipeline
    // (camera → mediamtx → tee → UDP 5600 → wfb_tx) does not re-attach to the
    // freshly restarted wfb_tx on its own — it recovers only via its slow
    // backoff FSM, so video can be silent for many seconds after a bind. Restart
    // it too so the feed re-establishes promptly without a drone reboot (Rule 26).
    if role == BindRole::Drone && !crate::systemctl::restart("ados-video.service").await {
        tracing::info!("ados_video_restart_skipped (not active / no video pipeline)");
    }

    Ok(PairResult {
        paired: true,
        peer_device_id: peer_device_id.map(|s| s.to_string()),
        paired_at,
        fingerprint,
        role,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_non_64() {
        assert!(validate_blob(&[0u8; 64]).is_ok());
        assert!(validate_blob(&[0u8; 63]).is_err());
        assert!(validate_blob(&[0u8; 65]).is_err());
        assert!(validate_blob(&[]).is_err());
    }

    #[test]
    fn atomic_write_sets_mode_and_content() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("tx.key");
        atomic_write(&p, &[7u8; 64], 0o600).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), vec![7u8; 64]);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // No leftover temp file.
        assert!(!dir.path().join("sub").join("tx.key.tmp").exists());
    }

    #[test]
    fn fingerprint_is_16_hex_deterministic_and_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let mut key_a = [0u8; 64];
        key_a[32..].copy_from_slice(&[1u8; 32]); // pub half = all 1s
        let mut key_b = [0u8; 64];
        key_b[32..].copy_from_slice(&[2u8; 32]); // pub half = all 2s
        let pa = dir.path().join("a.key");
        let pb = dir.path().join("b.key");
        atomic_write(&pa, &key_a, 0o600).unwrap();
        atomic_write(&pb, &key_b, 0o600).unwrap();
        let fa = read_public_fingerprint(&pa).unwrap();
        let fb = read_public_fingerprint(&pb).unwrap();
        assert_eq!(fa.len(), 16);
        assert!(fa
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_eq!(fa, read_public_fingerprint(&pa).unwrap()); // deterministic
        assert_ne!(fa, fb); // sensitive to the pub half
                            // A wrong-sized file is rejected.
        let pbad = dir.path().join("bad.key");
        atomic_write(&pbad, &[0u8; 10], 0o600).unwrap();
        assert!(read_public_fingerprint(&pbad).is_err());
    }

    #[test]
    fn apply_fields_drone_sets_then_clears() {
        let mut root = Mapping::new();
        apply_pair_fields(
            &mut root,
            BindRole::Drone,
            Some("dev-123"),
            Some("2026-05-29T00:00:00+00:00"),
            Some(false),
        );
        let wfb = root
            .get("video")
            .and_then(Value::as_mapping)
            .and_then(|m| m.get("wfb"))
            .and_then(Value::as_mapping)
            .unwrap();
        assert_eq!(
            wfb.get("paired_with_device_id").unwrap(),
            &Value::String("dev-123".into())
        );
        assert_eq!(
            wfb.get("paired_at").unwrap(),
            &Value::String("2026-05-29T00:00:00+00:00".into())
        );
        assert_eq!(wfb.get("auto_pair_enabled").unwrap(), &Value::Bool(false));
        // Drone never writes the ground_station mirror.
        assert!(root.get("ground_station").is_none());

        // Clearing (unpair shape): peer None + paired_at None → keys removed.
        apply_pair_fields(&mut root, BindRole::Drone, None, None, Some(true));
        let wfb = root
            .get("video")
            .and_then(Value::as_mapping)
            .and_then(|m| m.get("wfb"))
            .and_then(Value::as_mapping)
            .unwrap();
        assert!(wfb.get("paired_with_device_id").is_none());
        assert!(wfb.get("paired_at").is_none());
        assert_eq!(wfb.get("auto_pair_enabled").unwrap(), &Value::Bool(true));
    }

    #[test]
    fn apply_fields_gs_mirrors_ground_station() {
        let mut root = Mapping::new();
        apply_pair_fields(
            &mut root,
            BindRole::Gs,
            Some("drone-9"),
            Some("ts"),
            Some(false),
        );
        let gs = root
            .get("ground_station")
            .and_then(Value::as_mapping)
            .unwrap();
        assert_eq!(
            gs.get("paired_drone_id").unwrap(),
            &Value::String("drone-9".into())
        );
        assert_eq!(gs.get("paired_at").unwrap(), &Value::String("ts".into()));
    }

    #[test]
    fn persist_round_trips_via_serde_on_dev_host() {
        // On a non-Linux dev host the euid/flock gates are cfg'd out, so this
        // exercises load → mutate → atomic-write → reload end-to-end.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    channel: 149\nagent:\n  profile: drone\n",
        )
        .unwrap();
        let lock = dir.path().join("config.yaml.lock");
        let ok = persist_pair_state(
            &cfg,
            &lock,
            BindRole::Drone,
            Some("peer-1"),
            Some("ts-1"),
            Some(false),
        );
        // On Linux as non-root this returns false; on the dev host it writes.
        if cfg!(target_os = "linux") {
            return;
        }
        assert!(ok);
        let reloaded = load_config_mapping(&cfg);
        let wfb = reloaded
            .get("video")
            .and_then(Value::as_mapping)
            .and_then(|m| m.get("wfb"))
            .and_then(Value::as_mapping)
            .unwrap();
        assert_eq!(
            wfb.get("paired_with_device_id").unwrap(),
            &Value::String("peer-1".into())
        );
        assert_eq!(wfb.get("auto_pair_enabled").unwrap(), &Value::Bool(false));
        // Pre-existing keys preserved.
        assert_eq!(wfb.get("channel").unwrap(), &Value::Number(149.into()));
        assert!(reloaded.get("agent").is_some());
    }
}
