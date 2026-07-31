//! The fleet's hero slot, published as a sidecar so the video fan-out follows
//! the operator's selection.
//!
//! Hero selection lives in `ados-control`: it promotes the chosen drone to the
//! full video profile and demotes every other registered drone to a thumbnail,
//! each over the radio's aux RPC lane. The fan-out that feeds the mediamtx
//! ingest and the LCD tap lives here, in `ados-groundlink`, and until this
//! sidecar existed it served whichever slot the generation happened to start on
//! — the LOWEST registered one. An operator who picked a hero on any other slot
//! got the hero promoted to full rate while the ground station kept forwarding a
//! different, now-thumbnailed drone. The screen showed the wrong aircraft.
//!
//! A file under `/run/ados` rather than another IPC hop, following the
//! `video-profile.json` precedent: the owner stamps it on every decision and the
//! consumer reads it on a slow poll, so a restart on either side re-derives its
//! view from disk instead of the two drifting apart silently.
//!
//! The device id travels with the slot deliberately. A slot number alone cannot
//! be validated — every slot in `1..=FLEET_MAX_SLOTS` is a plausible number —
//! whereas a slot PLUS the device that holds it can be checked against the fleet
//! registry, so a selection left behind by a drone that has since unpaired is
//! recognised as stale rather than pointing the fan-out at a port nothing
//! transmits on.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fleet::FLEET_MAX_SLOTS;

/// The sidecar's own version integer, carried in the file so a reader can refuse
/// a shape it does not understand rather than mis-parsing it.
pub const FLEET_HERO_SIDECAR_VERSION: u32 = 1;

/// The hero selection as published for the fan-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetHero {
    /// Sidecar version. A reader ignores anything it does not recognise.
    pub v: u32,
    /// The hero's fleet slot — the actionable field: the fan-out listens on this
    /// slot's video egress port.
    pub slot: u8,
    /// The device that holds the slot, so a reader can tell a live selection
    /// from one left behind by a drone that has unpaired.
    pub device_id: String,
    /// When the selection was published, as integer unix milliseconds. Purely
    /// diagnostic; no reader gates on it.
    pub updated_at_ms: u64,
}

/// The canonical on-disk location of the published selection.
pub const FLEET_HERO_JSON: &str = "/run/ados/fleet-hero.json";

/// The bare filename, so [`hero_path`] and [`FLEET_HERO_JSON`] cannot drift.
const FLEET_HERO_FILENAME: &str = "fleet-hero.json";

/// The path to use, honouring `ADOS_RUN_DIR` so a test (or a non-root dev host)
/// can redirect the tmpfs layout.
pub fn hero_path() -> String {
    crate::paths::run_path(FLEET_HERO_FILENAME)
}

/// Publish `device_id` on `slot` as the fleet's hero.
///
/// Atomic (temp sibling plus rename), so the fan-out's poll reads either the
/// previous selection or the new one, never a half-written file. World-readable
/// like the other run-dir sidecars: it names a slot and a device, no secrets.
pub fn write_hero_to(path: &Path, slot: u8, device_id: &str) -> std::io::Result<()> {
    let hero = FleetHero {
        v: FLEET_HERO_SIDECAR_VERSION,
        slot,
        device_id: device_id.to_string(),
        updated_at_ms: now_unix_ms(),
    };
    crate::sidecars::write_json_atomic(path, &hero, 0o644)
}

/// Read the published selection, or `None` when there is nothing usable to read.
///
/// `None` covers every not-a-live-selection case with one answer — absent (the
/// boot state, and the state of every single-drone ground station that has never
/// been asked to choose), unreadable, malformed, a version this build does not
/// know, or a slot outside the issuable range. The caller's fallback is the
/// same for all of them, and treating a malformed file as "no selection" is the
/// only reading that cannot point the fan-out somewhere arbitrary.
pub fn read_hero_from(path: &Path) -> Option<FleetHero> {
    let body = std::fs::read(path).ok()?;
    let hero: FleetHero = serde_json::from_slice(&body).ok()?;
    if hero.v != FLEET_HERO_SIDECAR_VERSION {
        return None;
    }
    if hero.slot == 0 || hero.slot > FLEET_MAX_SLOTS || hero.device_id.is_empty() {
        return None;
    }
    Some(hero)
}

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
    fn the_canonical_path_and_the_filename_agree() {
        assert_eq!(FLEET_HERO_JSON, format!("/run/ados/{FLEET_HERO_FILENAME}"));
    }

    #[test]
    fn a_published_selection_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-hero.json");
        write_hero_to(&path, 3, "drone-c").unwrap();
        let hero = read_hero_from(&path).expect("a freshly written selection must read back");
        assert_eq!(hero.slot, 3);
        assert_eq!(hero.device_id, "drone-c");
        assert_eq!(hero.v, FLEET_HERO_SIDECAR_VERSION);
        // No temp sibling left behind for a reader to trip over.
        assert!(!dir.path().join("fleet-hero.tmp").exists());
    }

    #[test]
    fn an_absent_or_unusable_file_reads_as_no_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-hero.json");
        // Absent: the boot state.
        assert_eq!(read_hero_from(&path), None);

        // Malformed.
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(read_hero_from(&path), None);

        // A version this build does not know.
        std::fs::write(
            &path,
            br#"{"v":99,"slot":2,"device_id":"d","updated_at_ms":0}"#,
        )
        .unwrap();
        assert_eq!(read_hero_from(&path), None);

        // Slot 0 is not issuable, and neither is anything past the fleet cap.
        std::fs::write(
            &path,
            br#"{"v":1,"slot":0,"device_id":"d","updated_at_ms":0}"#,
        )
        .unwrap();
        assert_eq!(read_hero_from(&path), None);
        let past_cap = format!(
            r#"{{"v":1,"slot":{},"device_id":"d","updated_at_ms":0}}"#,
            FLEET_MAX_SLOTS as u16 + 1
        );
        std::fs::write(&path, past_cap).unwrap();
        assert_eq!(read_hero_from(&path), None);

        // A slot with no device cannot be validated against the registry, so it
        // is not a usable selection either.
        std::fs::write(
            &path,
            br#"{"v":1,"slot":2,"device_id":"","updated_at_ms":0}"#,
        )
        .unwrap();
        assert_eq!(read_hero_from(&path), None);
    }

    #[test]
    fn a_republished_selection_replaces_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-hero.json");
        write_hero_to(&path, 1, "drone-a").unwrap();
        write_hero_to(&path, 4, "drone-d").unwrap();
        let hero = read_hero_from(&path).unwrap();
        assert_eq!((hero.slot, hero.device_id.as_str()), (4, "drone-d"));
    }
}
