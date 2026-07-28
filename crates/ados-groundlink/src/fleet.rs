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

/// One registered drone: its issued slot, the device it was issued to, and when.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FleetSlot {
    pub slot: u8,
    pub device_id: String,
    /// Unix seconds (fractional), matching the `paired_at` the pair routes emit.
    pub paired_at: f64,
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
    /// `paired_at` on an existing entry is left alone: the field records when
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
                paired_at: now_unix(),
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

/// Wall clock as fractional unix seconds, the `paired_at` encoding the pair
/// routes already use.
fn now_unix() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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
    fn re_pair_keeps_the_original_paired_at() {
        // paired_at records when the slot was ISSUED. If a re-pair rewrote it,
        // an idempotent re-pair would be indistinguishable from a fresh one.
        let mut reg = FleetRegistry::default();
        reg.allocate("drone-a").unwrap();
        let issued = reg.slots().next().unwrap().paired_at;
        reg.allocate("drone-a").unwrap();
        assert_eq!(reg.slots().next().unwrap().paired_at, issued);
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
        assert!(v["1"]["paired_at"].as_f64().unwrap() > 0.0);
    }
}
