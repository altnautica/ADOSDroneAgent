//! The ground station's fleet slot registry: which drone owns which
//! `fleet_slot`, persisted across restarts.
//!
//! A slot is the 8 low bits of a wfb-ng `link_id` (see
//! [`ados_radio::config::link_id`]), so it is the addressing primitive the whole
//! N-drone transport rests on. Two drones sharing a slot share a `channel_id`,
//! and the wfb-ng `Aggregator` re-inits its FEC decoder on every foreign session
//! packet — session packets re-announce at ~1 Hz, so a duplicate slot thrashes
//! both drones' decoders about once a second and presents as unexplained link
//! loss. Slots are therefore ISSUED here, centrally, at pair time; they are
//! never negotiated between drones at runtime.
//!
//! [`FleetRegistry::allocate`] is idempotent by device id: re-pairing a drone
//! that already holds a slot returns the SAME slot rather than renumbering it.
//! Renumbering a flying drone would silently retune its transmitters mid-air.
//!
//! Persisted at [`FLEET_REGISTRY_PATH`] with the temp-file-plus-rename write the
//! router's `ParamCache::save` uses, so a crash mid-write can never leave a
//! truncated registry — the worst case is the previous complete generation.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// The registry's capacity: slots `1..=FLEET_MAX_SLOTS` are issuable. Re-exported
/// from the radio crate (which owns the `link_id` layout the bound comes from) so
/// a caller holding a `FleetRegistry` can name its limit without also depending
/// on `ados-radio`.
pub use ados_radio::config::FLEET_MAX_SLOTS;

/// Canonical on-disk location of the fleet registry.
pub const FLEET_REGISTRY_PATH: &str = "/var/lib/ados/fleet.json";

/// How often a running service re-reads this registry to pick up a slot issued
/// after it started. A drone can be paired long after a service is up, because
/// the pair route deliberately skips re-installing the receive unit when the
/// fleet key is unchanged, so every consumer of the registry needs the same
/// cadence rather than its own.
pub const FLEET_RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// One registered drone: its issued slot, the device it was issued to, and when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetSlot {
    pub slot: u8,
    pub device_id: String,
    /// When the slot was issued, as INTEGER unix milliseconds.
    ///
    /// Integer ms, not fractional seconds, because this value is persisted as
    /// JSON and compared for equality. `SystemTime::as_secs_f64()` at a present-day
    /// epoch (~1.79e9) has an f64 ULP of about 2.4e-7, so a nanosecond-derived
    /// timestamp is not representable and does not reliably survive a JSON
    /// round-trip: it lost 1 ULP intermittently, which made
    /// `FleetRegistry`'s own `PartialEq` unreliable after `persist` + `load`.
    /// Integer ms is exact in both f64 and JSON out to the year 285000, so
    /// exactness is structural here rather than a convention a later edit can
    /// quietly break. Millisecond resolution is far more than a pair timestamp
    /// needs.
    pub paired_at_ms: u64,
    /// The per-pair relay secret, hex-encoded, or `None` for a registration
    /// made before this field existed.
    ///
    /// Per PAIR, not per fleet. Every member holds the shared radio keypair —
    /// that is what makes them a member — so a credential derived from it
    /// proves only that the caller is on the radio, which being on the radio
    /// already proved. This is the material that lets a drone tell its own
    /// ground station from anything else that can reach the air, including
    /// another drone in the same fleet.
    ///
    /// Optional so an existing registry loads unchanged; a registration
    /// without one is issued a secret the next time it is allocated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_secret: Option<String>,
}

/// The slot table, keyed by slot so iteration is always in slot order (the
/// order the receive-chain reconciler spawns instances in, and the order the
/// pair-status route renders).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FleetRegistry {
    by_slot: BTreeMap<u8, FleetSlot>,
}

/// Whether two device ids name the same aircraft.
///
/// True only when one is a strict prefix of the other. That is exactly how the
/// short form is produced — a device id is `uuid4().hex[:12]` and the
/// 8-character form is that truncated again for naming — so a prefix match
/// reproduces the derivation rather than guessing at a resemblance.
///
/// Deliberately narrow: it matches ONLY the exact 8-from-12 hex derivation, not
/// any prefix of anything. A loose "one starts with the other" rule looked
/// equivalent and was not — it merged `drone-1` into `drone-11`, which an
/// existing test caught immediately. Identifiers that merely share a leading
/// substring are not the same aircraft, and a rule broad enough to merge them
/// would eventually bind a command to the wrong airframe. That is a worse
/// failure than the duplicate slot this fixes.
pub fn id_refers_to_same_device(a: &str, b: &str) -> bool {
    const FULL: usize = 12;
    const SHORT: usize = 8;
    let (a, b) = (a.trim(), b.trim());
    let (long, short) = if a.len() > b.len() { (a, b) } else { (b, a) };
    long.len() == FULL
        && short.len() == SHORT
        && long.chars().all(|c| c.is_ascii_hexdigit())
        && short.chars().all(|c| c.is_ascii_hexdigit())
        && long.starts_with(short)
}

impl FleetRegistry {
    /// Read the registry from `path`. A missing file is an empty fleet (the
    /// pre-first-pair state).
    ///
    /// An unparseable one also starts empty — a corrupt registry must not wedge
    /// the ground station into refusing every pair — but it is QUARANTINED
    /// first, because starting empty alone was quietly destructive. The next
    /// successful [`persist`](Self::persist) rewrites the file, so every other
    /// drone's slot assignment was gone and those slots were free to be issued
    /// again to different devices. Two drones on one slot share a `channel_id`
    /// and thrash each other's FEC decoder, which is the precise failure the
    /// slot registry exists to prevent, and it would have arrived with only a
    /// warning in the log to explain it.
    ///
    /// Moving the file aside keeps the assignments recoverable by hand and
    /// leaves evidence that outlives the log.
    pub fn load(path: &Path) -> Self {
        let Ok(body) = std::fs::read(path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&body) {
            Ok(reg) => reg,
            Err(e) => {
                let quarantined = quarantine(path);
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    quarantined = quarantined.as_deref().unwrap_or("FAILED"),
                    "fleet_registry_unparseable_quarantined_starting_empty"
                );
                Self::default()
            }
        }
    }

    /// Issue `device_id` a slot, or return the one it already holds.
    ///
    /// Idempotent by device id: a re-pair never renumbers a drone that may be
    /// airborne. A fresh device takes the LOWEST free slot in
    /// `1..=FLEET_MAX_SLOTS`, so a released slot is reused before the table
    /// grows. `None` when all [`FLEET_MAX_SLOTS`] slots are taken — the caller
    /// surfaces that as `E_FLEET_FULL` rather than evicting a registered drone.
    ///
    /// `paired_at_ms` on an existing entry is left alone: the field records when
    /// the slot was first issued, which is what makes a re-pair observably
    /// idempotent rather than looking like a new registration.
    pub fn allocate(&mut self, device_id: &str) -> Option<u8> {
        if let Some(slot) = self.slot_of(device_id) {
            return Some(slot);
        }
        let slot = (1..=FLEET_MAX_SLOTS).find(|s| !self.by_slot.contains_key(s))?;
        self.by_slot.insert(
            slot,
            FleetSlot {
                slot,
                device_id: device_id.to_string(),
                paired_at_ms: now_unix_ms(),
                relay_secret: generate_relay_secret_opt(),
            },
        );
        Some(slot)
    }

    /// Drop `device_id`'s registration, freeing its slot for the next
    /// allocation. `true` when a registration was removed.
    pub fn release(&mut self, device_id: &str) -> bool {
        let Some(slot) = self.slot_of(device_id) else {
            return false;
        };
        self.by_slot.remove(&slot).is_some()
    }

    /// The slot `device_id` holds, if it is registered.
    ///
    /// Matches the SHORT form of an id against its full form, because they are
    /// the same aircraft. A device id is `uuid4().hex[:12]` (`identity.py`), and
    /// the 8-character form seen on some surfaces is that same id truncated
    /// again for naming (`config/root.py`). Exact string equality therefore gave
    /// `f6aa0aa4` and `f6aa0aa41193` two different slots for one airframe, and
    /// the hero fan-out promoted and demoted the same aircraft in a single call.
    ///
    /// Only a genuine prefix relationship counts, and only in the direction the
    /// truncation actually goes — never a partial overlap, never an equal-length
    /// near-miss. An ambiguous prefix (two registered ids sharing it) resolves to
    /// NOTHING rather than to a guess: with 8 hex characters a collision inside
    /// a fleet is vanishingly unlikely, but silently binding a command to the
    /// wrong airframe is not a risk worth taking for a convenience.
    pub fn slot_of(&self, device_id: &str) -> Option<u8> {
        let needle = device_id.trim();
        if needle.is_empty() {
            return None;
        }
        if let Some(s) = self.by_slot.values().find(|s| s.device_id == needle) {
            return Some(s.slot);
        }
        let mut matches = self
            .by_slot
            .values()
            .filter(|s| id_refers_to_same_device(&s.device_id, needle));
        let first = matches.next()?;
        // A second match means the prefix does not identify one aircraft.
        if matches.next().is_some() {
            return None;
        }
        Some(first.slot)
    }

    /// Every registered slot, in ascending slot order.
    pub fn slots(&self) -> impl Iterator<Item = &FleetSlot> {
        self.by_slot.values()
    }

    /// True when no drone is registered (the pre-first-pair state).
    pub fn is_empty(&self) -> bool {
        self.by_slot.is_empty()
    }

    /// Number of registered drones.
    pub fn len(&self) -> usize {
        self.by_slot.len()
    }

    /// Write the registry to `path` atomically (temp file + rename), creating
    /// the parent directory if needed. Blocking disk I/O — call from a
    /// synchronous context, never directly on the reactor.
    pub fn persist(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        // 0600, like every other file this agent writes that describes who is
        // paired with whom. It landed 0644 because the mode was never set —
        // world-readable, and the registry is where per-device relay
        // credentials are due to live. Setting it now means that arrives on a
        // file that is already private rather than needing to be remembered
        // later.
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        // An existing temp file keeps its original mode through `open`, so set
        // it explicitly rather than trusting the create-time mode.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Move an unparseable registry aside, returning where it went.
///
/// Never clobbers an earlier quarantine: repeated corruption is exactly when
/// the OLDEST copy is the one most likely to still hold the real assignments,
/// so a suffix is added until a free name is found. Best-effort — a failure
/// here must not stop the ground station coming up, and the caller reports it.
fn quarantine(path: &Path) -> Option<String> {
    for n in 0..100 {
        let candidate = if n == 0 {
            path.with_extension("json.corrupt")
        } else {
            path.with_extension(format!("json.corrupt.{n}"))
        };
        if candidate.exists() {
            continue;
        }
        return match std::fs::rename(path, &candidate) {
            Ok(()) => Some(candidate.display().to_string()),
            Err(_) => None,
        };
    }
    None
}

/// A fresh per-pair relay secret, hex-encoded.
///
/// Fails closed: if the OS cannot give us randomness we return `None` rather
/// than a predictable value, and the pairing proceeds without a secret. A
/// missing secret degrades to today's behaviour — the relay presents nothing —
/// which is honest. A guessable one would look like a credential and not be
/// one, which is worse than having none.
fn generate_relay_secret_opt() -> Option<String> {
    let mut secret = [0u8; ados_protocol::relay_ticket::RELAY_SECRET_LEN];
    getrandom::getrandom(&mut secret).ok()?;
    Some(secret.iter().map(|b| format!("{b:02x}")).collect())
}

/// Wall clock as integer unix milliseconds. See [`FleetSlot::paired_at_ms`] for
/// why this is not fractional seconds.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ados-fleet-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_persisted_registry_is_not_world_readable() {
        // It landed 0644 because the mode was never set. The registry records
        // who is paired with whom, and is where per-device relay credentials
        // are due to live.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("mode");
        let path = dir.join("fleet.json");
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a");
        reg.persist(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the fleet registry must not be world-readable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewriting_over_a_stale_temp_file_still_lands_private() {
        // `open` on an existing file keeps its original mode, so a leftover
        // world-readable temp from an older build must not survive into the
        // renamed registry.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("mode-stale");
        let path = dir.join("fleet.json");
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, b"stale").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a");
        reg.persist(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_registry_is_quarantined_rather_than_overwritten() {
        // Starting empty on a parse failure is right — a corrupt registry must
        // not wedge the ground station into refusing every pair — but the next
        // persist then rewrote the file, so every drone's slot was gone and
        // those slots were free to reissue to different devices. Two drones on
        // one slot share a channel_id and thrash each other's FEC decoder,
        // which is the exact failure this registry exists to prevent.
        let dir = tmpdir("corrupt");
        let path = dir.join("fleet.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let reg = FleetRegistry::load(&path);
        assert!(reg.is_empty(), "a corrupt registry starts empty");

        let quarantined = dir.join("fleet.json.corrupt");
        assert!(
            quarantined.exists(),
            "the unreadable registry must be kept, not left to be overwritten"
        );
        assert_eq!(
            std::fs::read(&quarantined).unwrap(),
            b"{ this is not json",
            "the quarantined copy must be the original bytes"
        );
        assert!(
            !path.exists(),
            "the corrupt file is moved aside, not copied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repeated_corruption_never_clobbers_an_earlier_quarantine() {
        // Repeated corruption is exactly when the OLDEST copy is most likely to
        // still hold the real assignments.
        let dir = tmpdir("corrupt-twice");
        let path = dir.join("fleet.json");

        std::fs::write(&path, b"first corrupt").unwrap();
        let _ = FleetRegistry::load(&path);
        std::fs::write(&path, b"second corrupt").unwrap();
        let _ = FleetRegistry::load(&path);

        assert_eq!(
            std::fs::read(dir.join("fleet.json.corrupt")).unwrap(),
            b"first corrupt",
            "the first quarantine must survive the second"
        );
        assert_eq!(
            std::fs::read(dir.join("fleet.json.corrupt.1")).unwrap(),
            b"second corrupt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_valid_registry_is_never_quarantined() {
        let dir = tmpdir("valid");
        let path = dir.join("fleet.json");
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a");
        reg.persist(&path).unwrap();

        let back = FleetRegistry::load(&path);
        assert_eq!(back.len(), 1);
        assert!(path.exists(), "a good registry stays where it was");
        assert!(!dir.join("fleet.json.corrupt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_registry_is_the_pre_pair_state_not_a_corruption() {
        let dir = tmpdir("missing");
        let reg = FleetRegistry::load(&dir.join("fleet.json"));
        assert!(reg.is_empty());
        assert!(!dir.join("fleet.json.corrupt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn allocation_hands_out_the_lowest_free_slot_from_one() {
        // Slot 0 is the ground station and is never issued to a drone; the first
        // drone must land on 1, not 0, or it keys its transmitters onto the
        // shared uplink channel_id.
        let mut reg = FleetRegistry::default();
        assert_eq!(reg.allocate("drone-a"), Some(1));
        assert_eq!(reg.allocate("drone-b"), Some(2));
        assert_eq!(reg.allocate("drone-c"), Some(3));
        let slots: Vec<u8> = reg.slots().map(|s| s.slot).collect();
        assert_eq!(slots, vec![1, 2, 3]);
    }

    #[test]
    fn allocation_is_idempotent_by_device_id() {
        // A re-pair must return the SAME slot: renumbering silently retunes a
        // flying drone's transmitters onto a different channel_id.
        let mut reg = FleetRegistry::default();
        let first = reg.allocate("drone-a").unwrap();
        reg.allocate("drone-b").unwrap();
        let again = reg.allocate("drone-a").unwrap();
        assert_eq!(first, again);
        assert_eq!(reg.len(), 2, "a re-pair must not add a second registration");
    }

    #[test]
    fn each_drone_gets_its_own_relay_secret() {
        // Per PAIR, not per fleet: a shared secret would have the same holder
        // set as the radio key it is meant to improve on, so one drone could
        // impersonate the ground station to another.
        let mut r = FleetRegistry::default();
        r.allocate("aaaa");
        r.allocate("bbbb");
        let a = r
            .slots()
            .find(|s| s.device_id == "aaaa")
            .unwrap()
            .relay_secret
            .clone();
        let b = r
            .slots()
            .find(|s| s.device_id == "bbbb")
            .unwrap()
            .relay_secret
            .clone();
        assert!(a.is_some() && b.is_some());
        assert_ne!(a, b, "two drones must not share a relay secret");
    }

    #[test]
    fn a_relay_secret_is_never_empty_or_short() {
        // An empty or truncated value would look like a credential on every
        // surface that reports one, while authenticating nothing.
        let mut r = FleetRegistry::default();
        r.allocate("aaaa");
        let secret = r.slots().next().unwrap().relay_secret.clone().unwrap();
        assert_eq!(
            secret.len(),
            ados_protocol::relay_ticket::RELAY_SECRET_LEN * 2,
            "hex of {} bytes",
            ados_protocol::relay_ticket::RELAY_SECRET_LEN
        );
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn re_pairing_does_not_re_key_a_drone() {
        // allocate() is idempotent by device id. Issuing a fresh secret on
        // re-pair would silently invalidate the one the drone already holds,
        // and it may be airborne.
        let mut r = FleetRegistry::default();
        r.allocate("aaaa");
        let first = r.slots().next().unwrap().relay_secret.clone();
        r.allocate("aaaa");
        let second = r.slots().next().unwrap().relay_secret.clone();
        assert_eq!(first, second);
    }

    #[test]
    fn a_relay_secret_survives_persist_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let mut r = FleetRegistry::default();
        r.allocate("aaaa");
        r.persist(&path).unwrap();
        let loaded = FleetRegistry::load(&path);
        assert_eq!(
            loaded.slots().next().unwrap().relay_secret,
            r.slots().next().unwrap().relay_secret
        );
    }

    #[test]
    fn a_registry_written_before_the_field_existed_still_loads() {
        // The field is optional so an existing fleet survives the upgrade
        // rather than the whole registry failing to parse.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        std::fs::write(
            &path,
            r#"{"1":{"slot":1,"device_id":"aaaa","paired_at_ms":123}}"#,
        )
        .unwrap();
        let loaded = FleetRegistry::load(&path);
        let slot = loaded.slots().next().unwrap();
        assert_eq!(slot.device_id, "aaaa");
        assert_eq!(slot.relay_secret, None);
    }

    #[test]
    fn re_pair_keeps_the_original_paired_at_ms() {
        // paired_at_ms records when the slot was ISSUED. If a re-pair rewrote it,
        // an idempotent re-pair would be indistinguishable from a fresh one.
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a").unwrap();
        let issued = reg.slots().next().unwrap().paired_at_ms;
        reg.allocate("drone-a").unwrap();
        assert_eq!(reg.slots().next().unwrap().paired_at_ms, issued);
    }

    #[test]
    fn release_frees_the_slot_for_reuse() {
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a").unwrap();
        reg.allocate("drone-b").unwrap();
        reg.allocate("drone-c").unwrap();
        assert!(reg.release("drone-b"));
        assert_eq!(reg.slot_of("drone-b"), None);
        // The freed middle slot is reused before the table grows.
        assert_eq!(reg.allocate("drone-d"), Some(2));
        assert_eq!(reg.len(), 3);
    }

    #[test]
    fn releasing_an_unregistered_device_is_a_no_op() {
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a").unwrap();
        assert!(!reg.release("drone-nobody"));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn the_short_and_long_form_of_one_id_are_one_aircraft() {
        // A device id is uuid4().hex[:12]; the 8-character form seen on some
        // surfaces is that same id truncated again for naming. Exact string
        // equality gave one airframe two slots, and the hero fan-out then
        // promoted and demoted it in a single call.
        let mut r = FleetRegistry::default();
        let slot = r.allocate("f6aa0aa41193").unwrap();
        assert_eq!(r.slot_of("f6aa0aa4"), Some(slot));
        assert_eq!(
            r.allocate("f6aa0aa4"),
            Some(slot),
            "re-allocating under the short form must not issue a second slot"
        );
        assert_eq!(r.slots().count(), 1, "still one aircraft");
    }

    #[test]
    fn only_the_exact_eight_from_twelve_derivation_counts() {
        // Narrow on purpose. A loose "one starts with the other" rule merged
        // drone-1 into drone-11 and broke a fleet-full test on the first run.
        assert!(id_refers_to_same_device("f6aa0aa41193", "f6aa0aa4"));
        assert!(!id_refers_to_same_device("drone-11", "drone-1"));
        assert!(!id_refers_to_same_device("f6aa0aa41193", "f6aa0a"));
        assert!(!id_refers_to_same_device("f6aa0aa4119312", "f6aa0aa4"));
        // Non-hex of the right lengths is not a device id.
        assert!(!id_refers_to_same_device("zzzzzzzzzzzz", "zzzzzzzz"));
    }

    #[test]
    fn two_different_aircraft_are_never_merged() {
        // Binding a command to the wrong airframe is worse than the bug.
        assert!(!id_refers_to_same_device("f6aa0aa41193", "f6aa0aa41194"));
        assert!(!id_refers_to_same_device("aaaaaaaa", "bbbbbbbb"));
        assert!(!id_refers_to_same_device("f6aa0aa41193", "6aa0aa41"));
    }

    #[test]
    fn an_ambiguous_prefix_resolves_to_nothing_rather_than_a_guess() {
        // Two registered ids sharing the queried prefix means it identifies no
        // single aircraft. Returning either would be a coin flip.
        let mut r = FleetRegistry::default();
        r.allocate("abcd1234aaaa");
        r.allocate("abcd1234bbbb");
        assert_eq!(r.slot_of("abcd1234"), None);
    }

    #[test]
    fn an_empty_id_matches_nothing() {
        // An absent id is not a wildcard.
        let mut r = FleetRegistry::default();
        r.allocate("f6aa0aa41193");
        assert_eq!(r.slot_of(""), None);
        assert_eq!(r.slot_of("   "), None);
        assert!(!id_refers_to_same_device("", "f6aa0aa41193"));
    }

    #[test]
    fn a_full_registry_refuses_a_new_device_but_still_serves_a_known_one() {
        let mut reg = FleetRegistry::default();
        for i in 1..=FLEET_MAX_SLOTS {
            assert_eq!(reg.allocate(&format!("drone-{i}")), Some(i));
        }
        // Full: a new device gets None rather than evicting a registered drone.
        assert_eq!(reg.allocate("drone-25"), None);
        // A known device still resolves — a full fleet must not break re-pairs.
        assert_eq!(reg.allocate("drone-7"), Some(7));
        assert_eq!(reg.len(), FLEET_MAX_SLOTS as usize);
    }

    #[test]
    fn persist_then_load_round_trips_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a").unwrap();
        reg.allocate("drone-b").unwrap();
        reg.persist(&path).unwrap();

        let loaded = FleetRegistry::load(&path);
        assert_eq!(loaded, reg);
        assert_eq!(loaded.slot_of("drone-b"), Some(2));
        // The temp sibling must not survive the rename.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn persist_creates_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/ados/fleet.json");
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a").unwrap();
        reg.persist(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn a_present_day_timestamp_survives_the_json_round_trip_bit_for_bit() {
        // Regression. `paired_at` used to be fractional unix seconds from
        // `Duration::as_secs_f64()`. At a present-day epoch (~1.79e9) the f64 ULP
        // is about 2.4e-7, so a nanosecond-derived value is not representable and
        // did not reliably survive serialization: roughly one run in three lost
        // 1 ULP, which broke `FleetRegistry`'s own `PartialEq` after
        // `persist` + `load` and made the failure look like flaky infrastructure
        // rather than a representation bug.
        //
        // Integer milliseconds are exact in f64 and in JSON, so the round trip is
        // lossless by construction. Every millisecond across a full second is
        // checked, because the old failure depended on the low digits.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let base_ms: u64 = 1_785_251_808_084;
        for offset in 0..1000 {
            let mut reg = FleetRegistry::default();
            reg.allocate("drone-a").unwrap();
            // Overwrite the clock-derived value with a known one.
            let stamped = base_ms + offset;
            reg.by_slot.get_mut(&1).unwrap().paired_at_ms = stamped;
            reg.persist(&path).unwrap();

            let loaded = FleetRegistry::load(&path);
            assert_eq!(
                loaded.slots().next().unwrap().paired_at_ms,
                stamped,
                "timestamp {stamped} did not round-trip"
            );
            assert_eq!(loaded, reg, "registry equality broke at {stamped}");
        }
    }

    #[test]
    fn the_issued_timestamp_is_whole_milliseconds() {
        // A fractional value here would reintroduce the round-trip loss above, so
        // the type is the guard: `u64` cannot carry a fraction. This pins that the
        // clock helper feeds it a plausible present-day millisecond value rather
        // than seconds, which would silently date every pair to 1970.
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a").unwrap();
        let ms = reg.slots().next().unwrap().paired_at_ms;
        // Sanity band: after 2020-01-01 and before 2100-01-01, in MILLISECONDS.
        assert!(
            ms > 1_577_836_800_000 && ms < 4_102_444_800_000,
            "paired_at_ms {ms} is not a present-day millisecond timestamp"
        );
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        assert!(FleetRegistry::load(&missing).is_empty());

        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, b"{not json at all").unwrap();
        assert!(
            FleetRegistry::load(&corrupt).is_empty(),
            "a corrupt registry must not wedge the ground station"
        );
    }

    #[test]
    fn the_persisted_form_is_a_slot_keyed_object() {
        // The on-disk shape is read by the pair-status route and by an operator
        // debugging a fleet; pin it so a serde change cannot silently reshape it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet.json");
        let mut reg = FleetRegistry::default();
        reg.allocate("ados-abc123").unwrap();
        reg.persist(&path).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["1"]["slot"], 1);
        assert_eq!(v["1"]["device_id"], "ados-abc123");
        assert!(v["1"]["paired_at_ms"].as_u64().unwrap() > 0);
    }
}
