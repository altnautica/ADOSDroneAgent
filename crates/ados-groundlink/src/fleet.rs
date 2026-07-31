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
}

/// The slot table, keyed by slot so iteration is always in slot order (the
/// order the receive-chain reconciler spawns instances in, and the order the
/// pair-status route renders).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FleetRegistry {
    by_slot: BTreeMap<u8, FleetSlot>,
}

impl FleetRegistry {
    /// Read the registry from `path`. A missing file is an empty fleet (the
    /// pre-first-pair state); so is an unparseable one — a corrupt registry must
    /// not wedge the ground station into refusing every pair, and the next
    /// successful [`persist`](Self::persist) rewrites it cleanly.
    pub fn load(path: &Path) -> Self {
        let Ok(body) = std::fs::read(path) else {
            return Self::default();
        };
        match serde_json::from_slice::<Self>(&body) {
            Ok(reg) => reg,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "fleet_registry_unparseable_starting_empty"
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
    pub fn slot_of(&self, device_id: &str) -> Option<u8> {
        self.by_slot
            .values()
            .find(|s| s.device_id == device_id)
            .map(|s| s.slot)
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
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
