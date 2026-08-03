//! The WFB pair-proof record: per-key evidence that a radio key has ever
//! actually carried a link.
//!
//! ## Why this exists
//!
//! Auto-pair used to decide "am I already paired?" by looking at the shape of
//! the key file — 64 bytes with a fingerprintable peer-public half. That is a
//! FILE check, not a LINK check, and a key left behind by a ground station that
//! was since reflashed is a perfectly well-formed key. Two rigs sat unlinked for
//! an entire session on exactly that: the drone injected into a void holding a
//! structurally valid key from a peer that no longer existed, its own radio
//! reporting `rf_unverified` (transmitting, zero confirmed reception), while
//! auto-pair stayed permanently disarmed because the file looked fine.
//!
//! ## The rule this record encodes
//!
//! Auto-pair may re-arm itself ONLY for a key whose fingerprint has NEVER ONCE
//! been confirmed to work. The fingerprint is the lifetime key:
//!
//! - A stale key from a reflashed peer re-arms, because its fingerprint never
//!   entered the proof record — the peer that would have proven it is gone.
//! - A healthy link NEVER re-arms. Once a fingerprint is latched as proven, no
//!   amount of downtime re-arms it, so a transient (or multi-day) RF dropout is
//!   structurally incapable of triggering a re-bind. Silently re-binding a
//!   proven pair would be a worse failure than the deadlock this fixes.
//! - A fingerprint mismatch is a FULL reset, so a successful re-bind hands the
//!   new key a clean lifetime automatically.
//!
//! Deciding to OPEN a bind window is all this governs. Whether a bind may then
//! DECLARE a pair is unchanged: the orchestrator's peer-evidence gate remains
//! the sole authority on success.
//!
//! ## Where it lives
//!
//! `/var/lib`, deliberately — NOT `/run`. On tmpfs a reboot would erase the
//! episode budget and the cooldown, and a rig stuck in a boot loop would
//! reintroduce the bind storm the budget exists to bound.
//!
//! Two processes write it: the supervisor's re-arm reconciler (every episode)
//! and the operator force escape hatch on the auto-pair route. The record type
//! and its one atomic writer therefore live here, beside the other sidecar
//! records, rather than in either writer's own crate.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The pair-proof record path. Persistent by design — see the module docs.
pub const PAIR_PROOF_PATH: &str = "/var/lib/ados/wfb-pair-proof.json";

/// The record schema version, stamped on write and checked (warn-only) on read.
pub const PAIR_PROOF_SIDECAR_VERSION: u16 = 1;

/// File mode. World-readable so a diagnostic can show the latch state without
/// root; only root writes it.
pub const PAIR_PROOF_MODE: u32 = 0o644;

/// How often `proven_at` may be re-stamped while the key stays proven. The
/// latch only needs the boolean, so the timestamp is a courtesy for an operator
/// reading the file — and this is a flash card, so it is refreshed at most
/// hourly rather than on every tick.
pub const PROOF_RESTAMP_INTERVAL_S: u64 = 3600;

/// What a `mark_proven` call did, so the caller knows whether a write is owed
/// and whether this was the first-ever proof for the key (worth an event).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofStamp {
    /// The key had never been proven; it is now. Write, and announce it.
    First,
    /// Already proven, and the stamp was older than the re-stamp interval.
    /// Write, but there is nothing new to announce.
    Refreshed,
    /// Already proven and re-stamped recently: nothing to do, no write.
    Unchanged,
}

/// The per-key proof record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairProof {
    /// Schema version (absent ⇒ `0` from an older writer).
    #[serde(default)]
    pub version: u16,
    /// The bind role this record belongs to (`drone` / `gs`). A box re-profiled
    /// to the other role gets a fresh record.
    pub role: String,
    /// The blake2b-8 fingerprint of the key's peer-public half. THE lifetime
    /// key: a different fingerprint is a different key and resets everything.
    pub key_fingerprint: String,
    /// Unix seconds when this fingerprint was last confirmed to carry a link.
    /// `None` ⇒ never proven ⇒ the latch may re-arm.
    #[serde(default)]
    pub proven_at: Option<u64>,
    /// How many re-arm episodes this fingerprint has already consumed.
    #[serde(default)]
    pub rearm_episodes: u32,
    /// Unix seconds of the most recent re-arm, so the cooldown is wall-clock and
    /// survives a restart.
    #[serde(default)]
    pub last_rearm_at: Option<u64>,
    /// An operator asked for one re-arm regardless of the latch. Self-clearing:
    /// consumed by the next arm.
    #[serde(default)]
    pub force_rearm: bool,
}

impl PairProof {
    /// A never-proven record for `role` + `fingerprint`.
    pub fn fresh(role: &str, fingerprint: &str) -> Self {
        Self {
            version: PAIR_PROOF_SIDECAR_VERSION,
            role: role.to_string(),
            key_fingerprint: fingerprint.to_string(),
            proven_at: None,
            rearm_episodes: 0,
            last_rearm_at: None,
            force_rearm: false,
        }
    }

    /// True once this fingerprint has been confirmed to carry a link at least
    /// once. The single fact the never-re-arm-a-healthy-link rule turns on.
    pub fn is_proven(&self) -> bool {
        self.proven_at.is_some()
    }

    /// Whether this record describes the given role + key.
    pub fn matches(&self, role: &str, fingerprint: &str) -> bool {
        self.role == role && self.key_fingerprint == fingerprint
    }

    /// Record that the key was observed carrying a link at `now_unix`. Returns
    /// what changed so the caller can skip a needless write (flash) and can
    /// announce only the first-ever proof.
    pub fn mark_proven(&mut self, now_unix: u64) -> ProofStamp {
        match self.proven_at {
            None => {
                self.proven_at = Some(now_unix);
                ProofStamp::First
            }
            Some(at) if now_unix.saturating_sub(at) >= PROOF_RESTAMP_INTERVAL_S => {
                self.proven_at = Some(now_unix);
                ProofStamp::Refreshed
            }
            Some(_) => ProofStamp::Unchanged,
        }
    }

    /// Consume one re-arm episode at `now_unix`: bump the budget, stamp the
    /// wall-clock cooldown anchor, and clear any operator force (it is a
    /// one-shot).
    pub fn record_rearm(&mut self, now_unix: u64) {
        self.rearm_episodes = self.rearm_episodes.saturating_add(1);
        self.last_rearm_at = Some(now_unix);
        self.force_rearm = false;
    }

    /// The on-disk body: pretty JSON plus a trailing newline, so an operator can
    /// read the latch state with `cat`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = serde_json::to_vec_pretty(self).unwrap_or_default();
        body.push(b'\n');
        body
    }
}

/// The result of loading a record for a given role + key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedProof {
    /// The record to act on: the stored one when it matches, else a fresh one.
    pub proof: PairProof,
    /// True only when a PARSED record was discarded because it described a
    /// different role or key — i.e. the key genuinely changed under us, which is
    /// worth announcing. An absent or unparseable file is not a discard.
    pub reset: bool,
}

/// Read + parse the record at `path`. `None` when absent or unparseable — both
/// of which the caller must treat as never-proven with a zero budget, failing
/// toward recoverability rather than crashing the loop.
pub fn read_pair_proof_from(path: &Path) -> Option<PairProof> {
    let text = std::fs::read_to_string(path).ok()?;
    let proof: PairProof = serde_json::from_str(&text).ok()?;
    crate::sidecar::check_sidecar_version(
        "wfb-pair-proof",
        proof.version,
        PAIR_PROOF_SIDECAR_VERSION,
    );
    Some(proof)
}

/// Load the record for `role` + `fingerprint`, resetting on any mismatch.
///
/// A stored record for a DIFFERENT fingerprint is not partially reusable: its
/// proof, its episode count and its cooldown all describe a key that is no
/// longer on disk. Reusing any of it would either hand a brand-new key a spent
/// budget or, worse, hand it someone else's proof.
pub fn load_for(path: &Path, role: &str, fingerprint: &str) -> LoadedProof {
    match read_pair_proof_from(path) {
        Some(stored) if stored.matches(role, fingerprint) => LoadedProof {
            proof: stored,
            reset: false,
        },
        Some(_) => LoadedProof {
            proof: PairProof::fresh(role, fingerprint),
            reset: true,
        },
        None => LoadedProof {
            proof: PairProof::fresh(role, fingerprint),
            reset: false,
        },
    }
}

/// Atomically write `proof` to `path`: tmp sibling, fsync, explicit mode,
/// rename. The fsync is the point — this record is the only thing bounding a
/// bind storm across a reboot, so it must survive a power cut mid-write.
pub fn write_pair_proof_to(path: &Path, proof: &PairProof) -> std::io::Result<()> {
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
            .mode(PAIR_PROOF_MODE)
            .open(&tmp)?;
        f.write_all(&proof.to_bytes())?;
        f.sync_all()?;
    }
    // Re-chmod in case a umask altered the create-time mode.
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(PAIR_PROOF_MODE))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_version_matches_the_registry() {
        assert_eq!(
            PAIR_PROOF_SIDECAR_VERSION,
            crate::contracts::sidecar_version("wfb-pair-proof").unwrap()
        );
    }

    #[test]
    fn the_record_lives_on_persistent_storage_not_tmpfs() {
        // On tmpfs a reboot would erase the episode budget and the cooldown, so a
        // rig in a boot loop would reintroduce the bind storm the budget bounds.
        assert!(
            PAIR_PROOF_PATH.starts_with("/var/lib/"),
            "the proof record must survive a reboot, got {PAIR_PROOF_PATH}"
        );
    }

    #[test]
    fn a_fresh_record_is_never_proven_with_a_zero_budget() {
        let p = PairProof::fresh("drone", "aabbccdd11223344");
        assert!(!p.is_proven());
        assert_eq!(p.rearm_episodes, 0);
        assert_eq!(p.last_rearm_at, None);
        assert!(!p.force_rearm);
    }

    #[test]
    fn first_proof_stamps_and_later_proofs_are_rate_limited() {
        let mut p = PairProof::fresh("drone", "ff00");
        assert_eq!(p.mark_proven(1_000), ProofStamp::First);
        assert!(p.is_proven());
        // Inside the re-stamp interval: no write owed (this is a flash card).
        assert_eq!(p.mark_proven(1_001), ProofStamp::Unchanged);
        assert_eq!(
            p.mark_proven(1_000 + PROOF_RESTAMP_INTERVAL_S - 1),
            ProofStamp::Unchanged
        );
        // Past it: refreshed, but not announced again.
        assert_eq!(
            p.mark_proven(1_000 + PROOF_RESTAMP_INTERVAL_S),
            ProofStamp::Refreshed
        );
        assert_eq!(p.proven_at, Some(1_000 + PROOF_RESTAMP_INTERVAL_S));
    }

    #[test]
    fn recording_a_rearm_spends_budget_stamps_the_clock_and_clears_force() {
        let mut p = PairProof::fresh("gs", "1234");
        p.force_rearm = true;
        p.record_rearm(500);
        assert_eq!(p.rearm_episodes, 1);
        assert_eq!(p.last_rearm_at, Some(500));
        assert!(!p.force_rearm, "force is a one-shot");
        p.record_rearm(900);
        assert_eq!(p.rearm_episodes, 2);
        assert_eq!(p.last_rearm_at, Some(900));
    }

    #[test]
    fn a_record_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-pair-proof.json");
        let mut p = PairProof::fresh("drone", "abc123");
        p.mark_proven(42);
        p.record_rearm(7);
        write_pair_proof_to(&path, &p).unwrap();
        assert!(!dir.path().join("wfb-pair-proof.json.tmp").exists());
        assert_eq!(read_pair_proof_from(&path).unwrap(), p);

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, PAIR_PROOF_MODE);
    }

    #[test]
    fn a_matching_record_loads_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proof.json");
        let mut p = PairProof::fresh("drone", "same");
        p.mark_proven(10);
        p.record_rearm(11);
        write_pair_proof_to(&path, &p).unwrap();

        let loaded = load_for(&path, "drone", "same");
        assert!(!loaded.reset);
        assert!(loaded.proof.is_proven());
        assert_eq!(loaded.proof.rearm_episodes, 1);
    }

    #[test]
    fn a_new_fingerprint_resets_the_whole_record() {
        // A successful re-bind writes a new key; that key must start with a clean
        // lifetime, never inheriting the old key's proof or its spent budget.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proof.json");
        let mut old = PairProof::fresh("drone", "old-key");
        old.mark_proven(10);
        old.record_rearm(11);
        old.record_rearm(12);
        write_pair_proof_to(&path, &old).unwrap();

        let loaded = load_for(&path, "drone", "new-key");
        assert!(loaded.reset, "a discarded record is worth announcing");
        assert!(!loaded.proof.is_proven());
        assert_eq!(loaded.proof.rearm_episodes, 0);
        assert_eq!(loaded.proof.last_rearm_at, None);
        assert_eq!(loaded.proof.key_fingerprint, "new-key");
    }

    #[test]
    fn a_role_change_resets_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proof.json");
        let mut old = PairProof::fresh("drone", "k");
        old.mark_proven(10);
        write_pair_proof_to(&path, &old).unwrap();
        let loaded = load_for(&path, "gs", "k");
        assert!(loaded.reset);
        assert!(!loaded.proof.is_proven());
    }

    #[test]
    fn an_absent_or_unparseable_record_reads_as_never_proven_without_announcing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        let loaded = load_for(&missing, "drone", "k");
        assert!(!loaded.proof.is_proven());
        assert_eq!(loaded.proof.rearm_episodes, 0);
        assert!(!loaded.reset, "nothing known was discarded");

        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, b"{ not json at all").unwrap();
        assert!(read_pair_proof_from(&junk).is_none());
        let loaded = load_for(&junk, "drone", "k");
        assert!(!loaded.proof.is_proven());
        assert_eq!(loaded.proof.rearm_episodes, 0);
        assert!(!loaded.reset);
        // And it can be rewritten over rather than crashing the caller.
        write_pair_proof_to(&junk, &loaded.proof).unwrap();
        assert!(read_pair_proof_from(&junk).is_some());
    }
}
