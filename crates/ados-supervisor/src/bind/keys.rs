//! WFB key-apply + persisted pair-state, plus the key fingerprint that
//! `wfb/key_mgr.py` also computes.
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
//!   - the key write is atomic (unique temp + fsync + chmod + rename + dir fsync).

use std::path::Path;
use std::sync::Arc;

use ados_protocol::sidecar::write_atomic;

use serde_norway::{Mapping, Value};

use crate::process_manager::ProcessManager;

use super::fsm::iso_now;
use super::BindRole;

mod cameras;
pub use cameras::{apply_video_cameras, persist_video_cameras, CamerasPersist, OPERATOR_OWNER};

/// libsodium crypto_box key-file size: 32-byte secret + 32-byte peer-public.
pub const WFB_KEY_FILE_BYTES: usize = 64;
/// Offset of the peer-public half used for the fingerprint.
pub const WFB_PUBLIC_HALF_OFFSET: usize = 32;

/// The fleet addressing a bind assigns, persisted alongside the pair state.
///
/// A drone's slot is ISSUED by the ground station's `FleetRegistry` at pair
/// time and written here; it is never negotiated at runtime. Two drones sharing
/// a slot share a wfb-ng `channel_id`, and the `Aggregator` re-inits its FEC
/// decoder on every foreign session packet, so the pair thrash each other about
/// once a second — which presents as unexplained link loss, not as a config
/// fault. The ground station always takes slot 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetIdentity {
    pub fleet_id: u16,
    pub fleet_slot: u8,
}

/// Outcome of a successful [`apply_keypair`].
#[derive(Debug, Clone)]
pub struct PairResult {
    pub paired: bool,
    pub peer_device_id: Option<String>,
    pub paired_at: String,
    pub fingerprint: Option<String>,
    pub role: BindRole,
}

/// Reject anything that is not exactly a 64-byte key file: wfb-ng silently
/// fails decryption on a wrong-shaped key.
pub fn validate_blob(blob: &[u8]) -> Result<(), String> {
    if blob.len() != WFB_KEY_FILE_BYTES {
        return Err(format!(
            "key blob is {} bytes, expected {WFB_KEY_FILE_BYTES}",
            blob.len()
        ));
    }
    Ok(())
}

/// blake2b-8 fingerprint (16 lowercase hex chars) of the peer-public half.
/// Both rigs of a pair compute the same value, so a heartbeat cross-check is a
/// string compare. Mirrors `read_public_fingerprint`.
pub fn read_public_fingerprint(path: &Path) -> Result<String, String> {
    use blake2::digest::{consts::U8, Digest};
    use blake2::Blake2b;

    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() != WFB_KEY_FILE_BYTES {
        return Err(format!(
            "key file at {} is {} bytes, expected {WFB_KEY_FILE_BYTES}",
            path.display(),
            data.len()
        ));
    }
    Ok(hex::encode(Blake2b::<U8>::digest(
        &data[WFB_PUBLIC_HALF_OFFSET..],
    )))
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
/// the I/O so it is unit-testable without root / a real config path. Sets or
/// pops each pair field, mirrors it onto the ground-station keys on a GS, and
/// writes the fleet addressing the bind assigned.
///
/// `fleet` is `None` when the caller has no assignment to write, and the
/// existing `video.wfb.fleet_id` / `fleet_slot` are then left untouched rather
/// than cleared: a re-pair that carries no assignment must not silently strip a
/// flying drone's slot, which would park its radio on the next restart.
pub fn apply_pair_fields(
    root: &mut Mapping,
    role: BindRole,
    peer_device_id: Option<&str>,
    paired_at: Option<&str>,
    auto_pair_enabled: Option<bool>,
    fleet: Option<FleetIdentity>,
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
        if let Some(f) = fleet {
            wfb.insert(
                Value::String("fleet_id".to_string()),
                Value::Number(f.fleet_id.into()),
            );
            wfb.insert(
                Value::String("fleet_slot".to_string()),
                Value::Number(f.fleet_slot.into()),
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
    fleet: Option<FleetIdentity>,
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
        fleet,
    );

    let body = match serde_norway::to_string(&Value::Mapping(root)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, path = %config_path.display(), "config_write_failed");
            return false;
        }
    };
    match write_atomic(config_path, body.as_bytes(), 0o600) {
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
/// `fleet` is the addressing the ground station issued for this pair — slot 0
/// on a GS, the registry-allocated drone slot on a drone. `None` leaves any
/// existing `video.wfb.fleet_id` / `fleet_slot` alone, so a bind that carries
/// no assignment never strips a drone's slot.
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
    fleet: Option<FleetIdentity>,
) -> Result<PairResult, String> {
    validate_blob(blob)?;

    // Own the inputs so the apply task is self-contained (no borrow can be
    // invalidated by a dropped caller future). The process-manager backend is an
    // `Arc`, cheap to move into the task.
    let blob = blob.to_vec();
    let peer_device_id = peer_device_id.map(|s| s.to_string());
    let handle = tokio::spawn(async move {
        apply_keypair_inner(pm, &blob, role, peer_device_id.as_deref(), fleet).await
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
    fleet: Option<FleetIdentity>,
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
        // Only a definite answer confirms the stop; an unanswered probe is not
        // evidence the old process is gone.
        if pm.is_active(unit).await == Some(false) {
            confirmed_inactive = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    if !confirmed_inactive {
        tracing::warn!(unit, "wfb_unit_still_active_before_key_write");
    }

    let target = Path::new(role.key_path());
    write_atomic(target, blob, 0o600).map_err(|e| format!("key write failed: {e}"))?;
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
        fleet,
    );

    // Best-effort setup-complete sentinel (the captive portal stands down).
    let body = format!("{paired_at}\n");
    if let Err(e) = write_atomic(
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
    // it too so the feed re-establishes promptly without a drone reboot.
    // `try_restart`: only a pipeline that is running is cycled; one the
    // supervisor never started (video off, no camera) stays stopped.
    if role == BindRole::Drone && !pm.try_restart("ados-video.service").await {
        tracing::info!("ados_video_restart_failed");
    }
    // The swarm bus derives its beacon key from the shared key file this bind
    // just delivered, on both roles. It also re-reads that file on its own
    // cadence; the restart makes the new key take effect now rather than within
    // the next poll, and re-attaches it to the freshly re-brought-up adapter.
    if !pm.try_restart(super::ADOS_SWARMBUS_UNIT).await {
        tracing::info!("ados_swarmbus_restart_failed");
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
mod tests;
