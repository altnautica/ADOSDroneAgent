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
use std::sync::Arc;

use serde_norway::{Mapping, Value};

use crate::process_manager::ProcessManager;

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

/// Load the config mapping for a WRITE, distinguishing an absent file (a fresh
/// start — Ok(empty)) from a present-but-unparseable one (Err). A present file
/// that will not parse must NEVER be overwritten: a load-then-write over it would
/// silently drop every existing key (WFB pairing, `agent.profile`, network). The
/// caller aborts the write on Err rather than clobbering.
fn try_load_config_mapping(path: &Path) -> Result<Mapping, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Mapping::new()),
        Err(e) => return Err(format!("read failed: {e}")),
    };
    if text.trim().is_empty() {
        return Ok(Mapping::new());
    }
    match serde_norway::from_str::<Value>(&text) {
        Ok(Value::Mapping(m)) => Ok(m),
        Ok(Value::Null) => Ok(Mapping::new()),
        Ok(_) => Err("config root is not a mapping".to_string()),
        Err(e) => Err(format!("config yaml is unparseable: {e}")),
    }
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

    let mut root = match try_load_config_mapping(config_path) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, path = %config_path.display(), "config_write_aborted_unparseable");
            return false;
        }
    };
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

/// The owner tag of an existing config leg, defaulting to `operator` for a leg
/// written before the field existed (legacy legs are operator-managed).
fn leg_owner(leg: &serde_json::Value) -> String {
    leg.get("owner")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("operator")
        .to_string()
}

/// Stamp every incoming leg with the writer's `owner`, so the merge can attribute
/// each declared leg to the operator or to the plugin that declared it. Only
/// object legs are stamped (a malformed non-object is passed through untouched;
/// the caller has already validated the shape).
fn stamp_owner(legs: &[serde_json::Value], owner: &str) -> Vec<serde_json::Value> {
    legs.iter()
        .map(|leg| {
            let mut leg = leg.clone();
            if let Some(obj) = leg.as_object_mut() {
                obj.insert(
                    "owner".to_string(),
                    serde_json::Value::String(owner.to_string()),
                );
            }
            leg
        })
        .collect()
}

/// Merge an owner's incoming leg list into the existing list.
///
/// This is the key of the merge-by-owner persist: an operator write preserves a
/// plugin's declared legs (a smart pod's streams) and a plugin write preserves
/// the operator's legs. An existing leg is dropped when it is (a) owned by the
/// same writer — so a shrinking write removes the writer's stale legs — or (b)
/// shares an id with an incoming leg — so a re-declared leg (including a legacy
/// leg written before the owner field) is replaced in place with no duplicate.
/// Every other-owner leg is kept in its original position; the incoming block is
/// spliced where the writer's first replaced leg was, or appended when the writer
/// had none.
fn merge_camera_legs(
    existing: &[serde_json::Value],
    incoming_stamped: &[serde_json::Value],
    owner: &str,
) -> Vec<serde_json::Value> {
    let incoming_ids: std::collections::HashSet<&str> = incoming_stamped
        .iter()
        .filter_map(|l| l.get("id").and_then(serde_json::Value::as_str))
        .collect();
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut spliced = false;
    for leg in existing {
        let same_owner = leg_owner(leg) == owner;
        let id_collision = leg
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(|id| incoming_ids.contains(id))
            .unwrap_or(false);
        if same_owner || id_collision {
            if !spliced {
                out.extend(incoming_stamped.iter().cloned());
                spliced = true;
            }
            // Drop the replaced leg.
        } else {
            out.push(leg.clone());
        }
    }
    if !spliced {
        out.extend(incoming_stamped.iter().cloned());
    }
    out
}

/// Merge the writer's `cameras` (attributed to `owner`) into the existing
/// `video.cameras` list on the config mapping, then mirror the resulting primary
/// leg's source into `video.camera.source`. A non-array `cameras` leaves the
/// config untouched — the caller validates the list shape before this is reached,
/// so this is defence-in-depth.
pub fn apply_video_cameras(root: &mut Mapping, cameras: &serde_json::Value, owner: &str) {
    let Some(incoming) = cameras.as_array() else {
        return;
    };

    // The existing declared legs, read back as JSON so the merge works in one data
    // model (an absent / non-sequence `video.cameras` reads as empty).
    let existing: Vec<serde_json::Value> = root
        .get("video")
        .and_then(Value::as_mapping)
        .and_then(|v| v.get("cameras"))
        .and_then(Value::as_sequence)
        .map(|seq| {
            seq.iter()
                .filter_map(|v| serde_json::to_value(v).ok())
                .collect()
        })
        .unwrap_or_default();

    let stamped = stamp_owner(incoming, owner);
    let merged = merge_camera_legs(&existing, &stamped, owner);

    // Mirror the PRIMARY merged leg's source into `video.camera.source`, so the
    // inline video pipeline (which reads `video.camera`) serves the primary leg. A
    // pod-only drone has no local camera — its primary leg is a network RTSP the
    // existing IP-camera path pulls into `/main`. Without this, the pipeline runs
    // local V4L2/CSI discovery, finds nothing, and never starts (zero video). The
    // primary is the leg with role `primary`, else the first (matching
    // `VideoConfig::resolve_legs`).
    let primary = merged
        .iter()
        .find(|c| c.get("role").and_then(serde_json::Value::as_str) == Some("primary"))
        .or_else(|| merged.first());
    if let Some(src) = primary
        .and_then(|c| c.get("source"))
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let camera = ensure_map(ensure_map(root, "video"), "camera");
        set_str(camera, "source", src);
    }

    // Transcode the merged JSON list into the YAML data model and write it back.
    let Ok(value) = serde_norway::to_value(&merged) else {
        return;
    };
    let video = ensure_map(root, "video");
    video.insert(Value::String("cameras".to_string()), value);
}

/// Load → merge `video.cameras` by `owner` → atomically rewrite the config,
/// serialised by the same flock and euid-0 gate as [`persist_pair_state`] (the
/// file is 0600 root). Returns false (no write) for a non-root caller on Linux.
/// The video pipeline reads the new source list on its next start, so the caller
/// restarts `ados-video` after a `true`.
pub fn persist_video_cameras(
    config_path: &Path,
    lock_path: &Path,
    cameras: &serde_json::Value,
    owner: &str,
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

    let mut root = match try_load_config_mapping(config_path) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(error = %e, path = %config_path.display(), "config_write_aborted_unparseable");
            return false;
        }
    };
    apply_video_cameras(&mut root, cameras, owner);

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
///
/// The stop → confirm-inactive → write → start sequence runs inside a spawned
/// task that this function JOINS rather than awaiting inline. A `JoinHandle`
/// await is cancellation-safe: if the caller's future is dropped mid-apply (a
/// bind cancel / watchdog firing on the outer select), the spawned task keeps
/// running to completion on the runtime, so the key is never left half-applied
/// with the unit stopped. Once the blob has arrived here the apply is atomic.
pub async fn apply_keypair(
    pm: Arc<dyn ProcessManager>,
    blob: &[u8],
    role: BindRole,
    peer_device_id: Option<&str>,
) -> Result<PairResult, String> {
    validate_blob(blob)?;

    // Own the inputs so the apply task is self-contained (no borrow can be
    // invalidated by a dropped caller future). The process-manager backend is an
    // `Arc`, cheap to move into the task.
    let blob = blob.to_vec();
    let peer_device_id = peer_device_id.map(|s| s.to_string());
    let handle = tokio::spawn(async move {
        apply_keypair_inner(pm, &blob, role, peer_device_id.as_deref()).await
    });
    // Joining propagates the inner result; a JoinError (the task panicked) is the
    // only way to land here without a result, surfaced as a write failure.
    match handle.await {
        Ok(inner) => inner,
        Err(e) => Err(format!("key apply task failed: {e}")),
    }
}

/// The atomic stop → confirm-inactive → write → start body of [`apply_keypair`],
/// run inside the spawned, joined task so a dropped caller cannot interrupt it
/// mid-sequence.
async fn apply_keypair_inner(
    pm: Arc<dyn ProcessManager>,
    blob: &[u8],
    role: BindRole,
    peer_device_id: Option<&str>,
) -> Result<PairResult, String> {
    // Stop the consumer unit BEFORE writing the key, and confirm it actually
    // went inactive. The prior write-then-restart order raced the supervisor's
    // own service-recovery loop: a `systemctl restart` issued while a start job
    // is already in flight coalesces into it, leaving a process that loaded the
    // OLD key running — bench-observed as a ground station whose wfb_rx started
    // one second before the new rx.key landed and silently decoded nothing.
    // stop → confirm-inactive → write → start guarantees any process running
    // after the start read the new key.
    let unit = role.normal_unit();
    let _ = pm.stop(unit).await;
    let mut confirmed_inactive = false;
    for _ in 0..10 {
        if !pm.is_active(unit).await {
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
    if !pm.start(unit).await {
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
    if role == BindRole::Drone && !pm.restart("ados-video.service").await {
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

    #[tokio::test]
    async fn apply_keypair_rejects_a_bad_blob_before_touching_any_unit() {
        // The blob is validated up front, before the apply task is spawned: a
        // bad-length blob returns Err without ever stopping a unit or writing a
        // key, so the atomic stop→write→start sequence only runs once a real key
        // blob has arrived (the precondition the cancellation-safe spawn relies on).
        let err = apply_keypair(
            crate::process_manager::select(),
            &[0u8; 10],
            BindRole::Drone,
            None,
        )
        .await
        .expect_err("a 10-byte blob must be rejected");
        assert!(
            err.contains("expected"),
            "error names the size mismatch: {err}"
        );
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
        let reloaded = try_load_config_mapping(&cfg).unwrap();
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

    #[test]
    fn persist_video_cameras_writes_the_leg_list_and_preserves_the_rest() {
        // Dev-host end-to-end: load → set video.cameras → atomic-write → reload.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        std::fs::write(
            &cfg,
            "video:\n  wfb:\n    channel: 149\nagent:\n  profile: drone\n",
        )
        .unwrap();
        let lock = dir.path().join("config.yaml.lock");
        let cameras = serde_json::json!([
            {"id": "main", "source": "rtsp://192.168.144.25:8554/main", "role": "eo"},
            {"id": "ir", "source": "rtsp://192.168.144.25:8554/ir", "role": "ir", "codec": "h264"},
        ]);
        let ok = persist_video_cameras(&cfg, &lock, &cameras, "operator");
        // On Linux as non-root this returns false; on the dev host it writes.
        if cfg!(target_os = "linux") {
            return;
        }
        assert!(ok);
        let reloaded = try_load_config_mapping(&cfg).unwrap();
        let video = reloaded.get("video").and_then(Value::as_mapping).unwrap();
        let legs = video.get("cameras").and_then(Value::as_sequence).unwrap();
        assert_eq!(legs.len(), 2);
        let first = legs[0].as_mapping().unwrap();
        assert_eq!(first.get("id").unwrap(), &Value::String("main".into()));
        assert_eq!(
            first.get("source").unwrap(),
            &Value::String("rtsp://192.168.144.25:8554/main".into())
        );
        // Each written leg is stamped with the writer's owner.
        assert_eq!(
            first.get("owner").unwrap(),
            &Value::String("operator".into())
        );
        // Pre-existing video.wfb keys preserved (the set is a merge, not a replace).
        let wfb = video.get("wfb").and_then(Value::as_mapping).unwrap();
        assert_eq!(wfb.get("channel").unwrap(), &Value::Number(149.into()));
        assert!(reloaded.get("agent").is_some());
        // A1: the primary leg's source is mirrored into video.camera.source so
        // the inline pipeline serves it (a pod-only drone has no local camera).
        let camera = video.get("camera").and_then(Value::as_mapping).unwrap();
        assert_eq!(
            camera.get("source").unwrap(),
            &Value::String("rtsp://192.168.144.25:8554/main".into())
        );
    }

    #[test]
    fn persist_video_cameras_refuses_to_clobber_an_unparseable_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        // A present but unparseable config (a corrupt / truncated write).
        std::fs::write(&cfg, "agent:\n  profile: drone\nvideo: [unclosed\n").unwrap();
        let lock = dir.path().join("config.yaml.lock");
        let before = std::fs::read_to_string(&cfg).unwrap();
        let cameras = serde_json::json!([{"id":"main","source":"rtsp://x/main","role":"eo"}]);
        let ok = persist_video_cameras(&cfg, &lock, &cameras, "operator");
        assert!(!ok, "must refuse to write over an unparseable config");
        // The corrupt file is left untouched, not clobbered with an empty config
        // that would drop WFB pairing + profile.
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
    }

    #[test]
    fn apply_video_cameras_ignores_a_non_array() {
        // Defence-in-depth: a non-sequence value never mutates the config.
        let mut root = Mapping::new();
        apply_video_cameras(&mut root, &serde_json::json!({"id": "main"}), "operator");
        assert!(root.get("video").is_none());
        apply_video_cameras(&mut root, &serde_json::json!("main"), "operator");
        assert!(root.get("video").is_none());
    }

    fn cameras_after_apply(root: &Mapping) -> Vec<serde_json::Value> {
        root.get("video")
            .and_then(Value::as_mapping)
            .and_then(|v| v.get("cameras"))
            .and_then(Value::as_sequence)
            .map(|s| {
                s.iter()
                    .filter_map(|v| serde_json::to_value(v).ok())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn merge_by_owner_operator_write_preserves_plugin_legs() {
        // A plugin declared its pod legs; an operator write of a local cam must
        // keep the plugin legs (and vice versa).
        let mut root = Mapping::new();
        apply_video_cameras(
            &mut root,
            &serde_json::json!([
                {"id": "eo", "source": "rtsp://pod/main", "role": "primary"},
                {"id": "ir", "source": "rtsp://pod/ir", "role": "ir"},
            ]),
            "com.altnautica.siyi-pod",
        );
        apply_video_cameras(
            &mut root,
            &serde_json::json!([{"id": "belly", "source": "/dev/video2"}]),
            "operator",
        );
        let legs = cameras_after_apply(&root);
        let ids: Vec<&str> = legs
            .iter()
            .filter_map(|l| l.get("id").and_then(serde_json::Value::as_str))
            .collect();
        // Both plugin legs and the operator leg survive.
        assert!(ids.contains(&"eo"));
        assert!(ids.contains(&"ir"));
        assert!(ids.contains(&"belly"));
        assert_eq!(legs.len(), 3);
        // The operator leg carries the operator owner.
        let belly = legs.iter().find(|l| l["id"] == "belly").unwrap();
        assert_eq!(belly["owner"], "operator");
        let eo = legs.iter().find(|l| l["id"] == "eo").unwrap();
        assert_eq!(eo["owner"], "com.altnautica.siyi-pod");
    }

    #[test]
    fn merge_by_owner_plugin_rewrite_drops_its_own_stale_leg() {
        // A plugin that previously declared [eo, ir, wide] and now declares
        // [eo, ir] must drop its own stale `wide` leg, while an operator leg is
        // untouched.
        let mut root = Mapping::new();
        apply_video_cameras(
            &mut root,
            &serde_json::json!([{"id": "belly", "source": "/dev/video2"}]),
            "operator",
        );
        apply_video_cameras(
            &mut root,
            &serde_json::json!([
                {"id": "eo", "source": "rtsp://pod/main", "role": "primary"},
                {"id": "ir", "source": "rtsp://pod/ir"},
                {"id": "wide", "source": "rtsp://pod/wide"},
            ]),
            "com.altnautica.siyi-pod",
        );
        // Re-declare with `wide` removed.
        apply_video_cameras(
            &mut root,
            &serde_json::json!([
                {"id": "eo", "source": "rtsp://pod/main", "role": "primary"},
                {"id": "ir", "source": "rtsp://pod/ir"},
            ]),
            "com.altnautica.siyi-pod",
        );
        let legs = cameras_after_apply(&root);
        let ids: Vec<&str> = legs
            .iter()
            .filter_map(|l| l.get("id").and_then(serde_json::Value::as_str))
            .collect();
        assert!(!ids.contains(&"wide"), "the plugin's stale leg is dropped");
        assert!(ids.contains(&"belly"), "the operator leg is preserved");
        assert!(ids.contains(&"eo"));
        assert!(ids.contains(&"ir"));
        assert_eq!(legs.len(), 3);
    }

    #[test]
    fn merge_by_owner_replaces_a_legacy_ownerless_leg_in_place_without_a_duplicate() {
        // A leg written before the owner field existed (no owner ⇒ operator) with
        // the same id is replaced in place, not duplicated, when the plugin that
        // owns it re-declares.
        let mut root = Mapping::new();
        // Seed a legacy (ownerless) leg with id "eo".
        {
            let video = ensure_map(&mut root, "video");
            let seq = serde_json::json!([{"id": "eo", "source": "rtsp://old/eo"}]);
            video.insert(
                Value::String("cameras".into()),
                serde_norway::to_value(&seq).unwrap(),
            );
        }
        apply_video_cameras(
            &mut root,
            &serde_json::json!([{"id": "eo", "source": "rtsp://pod/main", "role": "primary"}]),
            "com.altnautica.siyi-pod",
        );
        let legs = cameras_after_apply(&root);
        assert_eq!(legs.len(), 1, "no duplicate id");
        assert_eq!(legs[0]["source"], "rtsp://pod/main");
        assert_eq!(legs[0]["owner"], "com.altnautica.siyi-pod");
        // The primary source was mirrored into video.camera.source.
        let camera_source = root
            .get("video")
            .and_then(Value::as_mapping)
            .and_then(|v| v.get("camera"))
            .and_then(Value::as_mapping)
            .and_then(|c| c.get("source"))
            .and_then(Value::as_str);
        assert_eq!(camera_source, Some("rtsp://pod/main"));
    }
}
