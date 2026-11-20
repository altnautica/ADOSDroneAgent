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
//!
//! ## Who reads it
//!
//! [`rearm_view_at`] projects the record into the `pair_rearm` view every
//! status surface serves (the LAN radio routes in ados-control and the cloud
//! heartbeat's radio block), so the LAN and cloud reads cannot disagree. The
//! WFB key fingerprint the record is keyed on ([`key_fingerprint`]) lives here
//! for the same reason.

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
    /// Why the most recent re-arm opened a bind window: `unproven` (a key that
    /// never worked), `mis_keyed` (a key that worked, its peer heard on the air
    /// with another key) or `forced` (an operator asked). `None` before the
    /// first re-arm, or on a record written before the cause was kept. The pair
    /// status surfaces it so an operator sees why the rig is re-pairing.
    #[serde(default)]
    pub last_rearm_cause: Option<String>,
    /// An operator asked for one re-arm regardless of the latch. Self-clearing:
    /// consumed by the next arm.
    #[serde(default)]
    pub force_rearm: bool,
}

/// The bland `cause` of a re-arm: the operator asked.
pub const REARM_CAUSE_FORCED: &str = "forced";
/// The bland `cause` of a re-arm: a key that worked, its peer on the air re-keyed.
pub const REARM_CAUSE_MIS_KEYED: &str = "mis_keyed";
/// The bland `cause` of a re-arm: a key that has never once worked.
pub const REARM_CAUSE_UNPROVEN: &str = "unproven";

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
            last_rearm_cause: None,
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

    /// Why a re-arm taken now opens its window. The latch arms a forced record
    /// on the force alone, a proven key only on the mis-keyed hold and an
    /// unproven key on its unproven hold, so the record itself names the cause.
    pub fn rearm_cause(&self) -> &'static str {
        if self.force_rearm {
            REARM_CAUSE_FORCED
        } else if self.is_proven() {
            REARM_CAUSE_MIS_KEYED
        } else {
            REARM_CAUSE_UNPROVEN
        }
    }

    /// Consume one re-arm episode at `now_unix`: bump the budget, stamp the
    /// wall-clock cooldown anchor, keep the cause, and clear any operator force
    /// (it is a one-shot).
    pub fn record_rearm(&mut self, now_unix: u64) {
        self.last_rearm_cause = Some(self.rearm_cause().to_string());
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

/// Atomically write `proof` to `path` through
/// [`crate::sidecar::write_atomic`]: unique temp sibling, explicit mode, fsync,
/// rename, directory fsync. The fsync is the point — this record is the only
/// thing bounding a bind storm across a reboot, so it must survive a power cut
/// mid-write.
pub fn write_pair_proof_to(path: &Path, proof: &PairProof) -> std::io::Result<()> {
    crate::sidecar::write_atomic(path, &proof.to_bytes(), PAIR_PROOF_MODE)
}

/// The operator's "retry the local link" request, consumed by the supervisor's
/// auto-pair loop.
///
/// Once the loop has spent its local-bind attempts it parks on the cloud relay
/// and stops trying. Re-arming the flag in the config does not reach that
/// in-memory verdict, so the auto-pair route drops this file and the loop takes
/// it on its next tick: it clears the park and the attempt count and binds
/// again. `/run`, deliberately: a supervisor restart resets the park anyway, so
/// a request must not outlive the boot it was made in.
pub const AUTO_PAIR_RETRY_PATH: &str = "/run/ados/auto-pair-retry.request";

/// Record a local-retry request at `path` (the content is informational; the
/// file's presence is the request).
pub fn request_local_retry_at(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, b"retry-local\n")
}

/// Consume a pending local-retry request at `path`. `true` exactly once per
/// request: the removal is the acknowledgement, so two ticks cannot both act on
/// one request. An unreadable directory reads as no request.
pub fn take_local_retry_request_at(path: &Path) -> bool {
    std::fs::remove_file(path).is_ok()
}

/// The WFB key directory (`tx.key` on a drone, `rx.key` on a ground station).
pub const WFB_KEY_DIR: &str = "/etc/ados/wfb";

/// The exact size of a complete WFB-ng key file. Mirrors
/// `WFB_KEY_FILE_BYTES` in the Python key manager.
const WFB_KEY_FILE_BYTES: usize = 64;

/// The byte offset of the peer-public half (the second 32 bytes) inside a
/// 64-byte WFB key file. Mirrors `WFB_PUBLIC_HALF_OFFSET`.
const WFB_PUBLIC_HALF_OFFSET: usize = 32;

/// The 16-hex-char public-key fingerprint of a WFB key file, or `None` when the
/// file is absent or not exactly 64 bytes: `blake2b(public_half,
/// digest_size=8)` as lowercase hex. The one fingerprint reader: the radio,
/// ground link, supervisor and status routes all read keys through it.
/// Mirrors `read_public_fingerprint` in the Python key manager.
pub fn key_fingerprint(path: &Path) -> Option<String> {
    blob_fingerprint(&std::fs::read(path).ok()?)
}

/// [`key_fingerprint`] of an in-memory key file: `None` unless `blob` is
/// exactly a 64-byte key.
pub fn blob_fingerprint(blob: &[u8]) -> Option<String> {
    use blake2::digest::{consts::U8, Digest};
    use blake2::Blake2b;
    (blob.len() == WFB_KEY_FILE_BYTES)
        .then(|| hex::encode(Blake2b::<U8>::digest(&blob[WFB_PUBLIC_HALF_OFFSET..])))
}

/// The radio-pair predicate: the bind role's own key file (`tx.key` for
/// `drone`, `rx.key` otherwise) is a complete key with a fingerprint, which is
/// returned. A truncated key, or a key left over from the other role, is not a
/// pairing.
pub fn role_key_fingerprint(key_dir: &Path, bind_role: &str) -> Option<String> {
    let name = if bind_role == "drone" {
        "tx.key"
    } else {
        "rx.key"
    };
    key_fingerprint(&key_dir.join(name))
}

/// The auto-pair re-arm the latch last took for the key on disk, as the status
/// surfaces serve it (`pair_rearm`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PairRearmView {
    /// `unproven`, `mis_keyed` or `forced`; null on a record written before
    /// the cause was kept.
    pub cause: Option<String>,
    /// Unix seconds of the re-arm.
    pub at: u64,
    /// How many re-arms this key has taken.
    pub episodes: u32,
    /// The bind window the re-arm opened is still running.
    pub active: bool,
}

/// The re-arm view for the `bind_role` key in `key_dir`, from the proof record
/// at `proof` and the bind-state sentinel at `bind_state`.
///
/// `None` when the rig holds no complete key, the record describes another key
/// or role, or this key was never re-armed. A successful re-pair writes a new
/// key, so the view clears itself the moment the re-pair lands. `active` is
/// true while the sentinel reports a running bind the auto-pair loop started:
/// on a rig that holds a key the loop binds only when the latch opens a window,
/// so that bind is the re-arm's.
pub fn rearm_view_at(
    proof: &Path,
    bind_state: &Path,
    key_dir: &Path,
    bind_role: &str,
) -> Option<PairRearmView> {
    let fingerprint = role_key_fingerprint(key_dir, bind_role)?;
    let proof = read_pair_proof_from(proof).filter(|p| p.matches(bind_role, &fingerprint))?;
    Some(PairRearmView {
        at: proof.last_rearm_at?,
        cause: proof.last_rearm_cause,
        episodes: proof.rearm_episodes,
        active: auto_bind_running(bind_state),
    })
}

/// [`rearm_view_at`] on the canonical record, sentinel
/// (`<run dir>/bind-state.json`) and key directory.
pub fn rearm_view(bind_role: &str) -> Option<PairRearmView> {
    rearm_view_at(
        Path::new(PAIR_PROOF_PATH),
        &crate::paths::run_dir().join(crate::paths::BIND_STATE_JSON),
        Path::new(WFB_KEY_DIR),
        bind_role,
    )
}

/// Whether the bind-state sentinel reports a running bind the auto-pair loop
/// started.
fn auto_bind_running(bind_state: &Path) -> bool {
    let Some(v) = std::fs::read(bind_state)
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    else {
        return false;
    };
    v.get("active").and_then(serde_json::Value::as_bool) == Some(true)
        && v.get("source").and_then(serde_json::Value::as_str) == Some("auto")
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
    fn a_rearm_keeps_its_cause() {
        let mut p = PairProof::fresh("gs", "1234");
        assert_eq!(p.last_rearm_cause, None);
        p.record_rearm(1);
        assert_eq!(p.last_rearm_cause.as_deref(), Some(REARM_CAUSE_UNPROVEN));
        p.mark_proven(2);
        p.record_rearm(3);
        assert_eq!(p.last_rearm_cause.as_deref(), Some(REARM_CAUSE_MIS_KEYED));
        // The force wins over the proof, and is spent by the arm it caused.
        p.force_rearm = true;
        p.record_rearm(4);
        assert_eq!(p.last_rearm_cause.as_deref(), Some(REARM_CAUSE_FORCED));
        assert_eq!(p.rearm_cause(), REARM_CAUSE_MIS_KEYED);
    }

    #[test]
    fn a_record_without_a_cause_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proof.json");
        std::fs::write(
            &path,
            br#"{"version":1,"role":"gs","key_fingerprint":"k","last_rearm_at":9,"rearm_episodes":1}"#,
        )
        .unwrap();
        let p = read_pair_proof_from(&path).unwrap();
        assert_eq!(p.last_rearm_at, Some(9));
        assert_eq!(p.last_rearm_cause, None);
    }

    #[test]
    fn a_record_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wfb-pair-proof.json");
        let mut p = PairProof::fresh("drone", "abc123");
        p.mark_proven(42);
        p.record_rearm(7);
        write_pair_proof_to(&path, &p).unwrap();
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".tmp"));
        assert!(!stray);
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

    #[test]
    fn a_local_retry_request_is_consumed_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run").join("auto-pair-retry.request");
        assert!(!take_local_retry_request_at(&path));
        request_local_retry_at(&path).unwrap();
        // A second request before the loop ticks is still one request.
        request_local_retry_at(&path).unwrap();
        assert!(take_local_retry_request_at(&path));
        assert!(!take_local_retry_request_at(&path));
    }

    /// The vectors the radio's, ground link's and supervisor's own readers were
    /// pinned to before they were folded into this one, so every caller reads
    /// the same fingerprint it always did, from the file and from a blob alike.
    #[test]
    fn fingerprint_is_16_hex_of_blake2b_8_over_the_public_half() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tx.key");
        // Python `hashlib.blake2b(bytes(range(32, 64)), digest_size=8)`.
        let ranged: Vec<u8> = (0u8..64).collect();
        // Python `hashlib.blake2b(bytes([7] * 32), digest_size=8)`.
        let mut sevens = vec![1u8; 32];
        sevens.extend([7u8; 32]);
        for (bytes, want) in [(ranged, "e054fa6440b441f9"), (sevens, "0574f1613c2bca8f")] {
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(key_fingerprint(&path).as_deref(), Some(want));
            assert_eq!(blob_fingerprint(&bytes).as_deref(), Some(want));
        }
        assert_eq!(blob_fingerprint(&[0u8; 63]), None);
        assert_eq!(blob_fingerprint(&[0u8; 65]), None);
    }

    #[test]
    fn fingerprint_rejects_a_wrong_size_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rx.key");
        std::fs::write(&path, vec![0u8; 32]).unwrap();
        assert_eq!(key_fingerprint(&path), None);
        assert_eq!(key_fingerprint(&dir.path().join("absent.key")), None);
    }

    /// A truncated key, or a stale key of the other role, is not a radio pairing.
    #[test]
    fn paired_is_the_roles_own_complete_key() {
        let dir = tempfile::tempdir().unwrap();
        let key_dir = dir.path();
        assert!(role_key_fingerprint(key_dir, "drone").is_none());
        std::fs::write(key_dir.join("tx.key"), b"x").unwrap();
        assert!(role_key_fingerprint(key_dir, "drone").is_none());
        std::fs::write(key_dir.join("rx.key"), [7u8; 64]).unwrap();
        assert!(role_key_fingerprint(key_dir, "drone").is_none());
        assert!(role_key_fingerprint(key_dir, "gs").is_some());
        std::fs::write(key_dir.join("tx.key"), [7u8; 64]).unwrap();
        assert!(role_key_fingerprint(key_dir, "drone").is_some());
    }

    /// A ground station holding a key that worked, re-armed because its drone
    /// is on the air with another key: the view names the cause while the auto
    /// bind runs, and stays (inactive) once the window closes unpaired.
    #[test]
    fn rearm_view_names_the_cause_while_the_auto_bind_runs() {
        let dir = tempfile::tempdir().unwrap();
        let key_dir = dir.path().join("wfb");
        std::fs::create_dir_all(&key_dir).unwrap();
        std::fs::write(key_dir.join("rx.key"), [7u8; 64]).unwrap();
        let fp = role_key_fingerprint(&key_dir, "gs").unwrap();
        let proof = dir.path().join("proof.json");
        let sentinel = dir.path().join("bind-state.json");
        let view = || rearm_view_at(&proof, &sentinel, &key_dir, "gs");

        // Never re-armed: nothing to show.
        assert_eq!(view(), None);
        let mut p = PairProof::fresh("gs", &fp);
        p.mark_proven(100);
        write_pair_proof_to(&proof, &p).unwrap();
        assert_eq!(view(), None);

        p.record_rearm(700);
        write_pair_proof_to(&proof, &p).unwrap();
        std::fs::write(
            &sentinel,
            r#"{"active":true,"source":"auto","state":"waiting_peer"}"#,
        )
        .unwrap();
        let running = PairRearmView {
            cause: Some(REARM_CAUSE_MIS_KEYED.to_string()),
            at: 700,
            episodes: 1,
            active: true,
        };
        assert_eq!(view(), Some(running.clone()));
        assert_eq!(
            serde_json::to_value(&running).unwrap(),
            serde_json::json!({"cause": "mis_keyed", "at": 700, "episodes": 1, "active": true})
        );

        // An operator bind is not the re-arm's window.
        std::fs::write(&sentinel, r#"{"active":true,"source":"operator"}"#).unwrap();
        assert_eq!(view().map(|v| v.active), Some(false));
        std::fs::write(
            &sentinel,
            r#"{"active":false,"source":"auto","state":"failed"}"#,
        )
        .unwrap();
        assert_eq!(view().map(|v| v.active), Some(false));
    }

    /// The record belongs to one key and one role: a re-pair that wrote a new
    /// key, or a record left by the other role, is not this key's re-arm.
    #[test]
    fn rearm_view_ignores_another_keys_record() {
        let dir = tempfile::tempdir().unwrap();
        let key_dir = dir.path().join("wfb");
        std::fs::create_dir_all(&key_dir).unwrap();
        std::fs::write(key_dir.join("rx.key"), [7u8; 64]).unwrap();
        let fp = role_key_fingerprint(&key_dir, "gs").unwrap();
        let proof = dir.path().join("proof.json");
        let sentinel = dir.path().join("bind-state.json");
        let view = || rearm_view_at(&proof, &sentinel, &key_dir, "gs");

        let mut other = PairProof::fresh("gs", "0000000000000000");
        other.record_rearm(5);
        write_pair_proof_to(&proof, &other).unwrap();
        assert_eq!(view(), None);

        let mut drone = PairProof::fresh("drone", &fp);
        drone.record_rearm(5);
        write_pair_proof_to(&proof, &drone).unwrap();
        assert_eq!(view(), None);

        // No key on disk at all: an unpaired rig is not re-pairing.
        std::fs::remove_file(key_dir.join("rx.key")).unwrap();
        assert_eq!(view(), None);
    }
}
