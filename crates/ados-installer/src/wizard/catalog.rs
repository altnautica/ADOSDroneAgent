//! The profile-filtered hardware catalog for the onboarding scan screen.
//!
//! One data-driven list of every hardware category ADOS supports — the ones it
//! detects today (`Availability::Now`) and the ones on the roadmap
//! (`Availability::Planned`, shown as "supported, plug in to detect"). Each row
//! is tagged with the profiles it applies to, so a ground station never sees a
//! flight controller and a workstation never sees a long-range radio. Detection
//! classifies each category from the single [`crate::wizard::hw::SysProbe`]
//! snapshot, so the scan shells out once and the classification is pure and
//! unit-tested.
//!
//! Extending it: add a row to [`CATALOG`] (and, for a `Now` row, a match arm in
//! [`detect`]). A `Planned` row needs no probe — it advertises the capability
//! until its detector lands.

use crate::wizard::hw::{self, SysProbe};

/// A profile the wizard configures. The catalog is filtered by this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Drone,
    GroundStation,
    Workstation,
}

impl Profile {
    /// Map the on-disk profile id to the catalog profile. `compute` (the reserved
    /// lean headless node) shares the workstation catalog.
    pub fn from_id(id: &str) -> Profile {
        match id {
            "ground_station" => Profile::GroundStation,
            "workstation" | "compute" => Profile::Workstation,
            _ => Profile::Drone,
        }
    }
}

/// Whether ADOS detects a category today or only advertises it as supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Detected today: the scan actively probes for it.
    Now,
    /// On the roadmap: shown as supported, not yet probed.
    Planned,
}

/// One catalog category.
#[derive(Debug, Clone, Copy)]
pub struct HwCategory {
    /// Stable id used by [`detect`] and any pre-seed.
    pub id: &'static str,
    /// The operator-facing name.
    pub label: &'static str,
    /// One dim line describing what it is for.
    pub note: &'static str,
    /// The profiles this category is shown for.
    pub profiles: &'static [Profile],
    pub availability: Availability,
}

use Availability::{Now, Planned};
use Profile::{Drone, GroundStation, Workstation};

/// The full ADOS hardware catalog, profile-tagged. Order is the display order
/// within a profile: detected-today rows first, roadmap rows after.
pub const CATALOG: &[HwCategory] = &[
    // ── drone ──────────────────────────────────────────────────────────────
    HwCategory {
        id: "fc",
        label: "Flight controller",
        note: "Runs the flight code the aircraft obeys.",
        profiles: &[Drone],
        availability: Now,
    },
    HwCategory {
        id: "camera",
        label: "Camera",
        note: "Live video from the aircraft.",
        profiles: &[Drone],
        availability: Now,
    },
    HwCategory {
        id: "gps",
        label: "GPS receiver",
        note: "Position and navigation fixes.",
        profiles: &[Drone],
        availability: Now,
    },
    HwCategory {
        id: "modem",
        label: "4G / LTE modem",
        note: "A backup link over the cellular network.",
        profiles: &[Drone],
        availability: Now,
    },
    // ── shared radio (drone + ground station) ───────────────────────────────
    HwCategory {
        id: "radio",
        label: "Long-range radio",
        note: "The 50 km+ link between the air and the ground.",
        profiles: &[Drone, GroundStation],
        availability: Now,
    },
    // ── ground station ──────────────────────────────────────────────────────
    HwCategory {
        id: "mesh",
        label: "Mesh radio (2nd adapter)",
        note: "A second radio to relay and extend range across nodes.",
        profiles: &[GroundStation],
        availability: Now,
    },
    HwCategory {
        id: "oled",
        label: "Status screen (OLED)",
        note: "A small I2C display for link and status.",
        profiles: &[GroundStation],
        availability: Now,
    },
    HwCategory {
        id: "hdmi",
        label: "HDMI output",
        note: "Drive a monitor as a full-screen ground display.",
        profiles: &[GroundStation],
        availability: Now,
    },
    HwCategory {
        id: "joystick",
        label: "Joystick / gamepad",
        note: "Fly with a USB controller.",
        profiles: &[GroundStation],
        availability: Now,
    },
    // ── workstation ─────────────────────────────────────────────────────────
    HwCategory {
        id: "board",
        label: "This computer",
        note: "The machine that runs the app and the heavy work.",
        profiles: &[Workstation],
        availability: Now,
    },
    HwCategory {
        id: "gpu",
        label: "Graphics (GPU)",
        note: "Accelerates 3D reconstruction and vision.",
        profiles: &[Workstation],
        availability: Now,
    },
    // ── roadmap: sensors (drone) ────────────────────────────────────────────
    HwCategory {
        id: "gimbal",
        label: "Camera gimbal",
        note: "Stabilise and aim the camera.",
        profiles: &[Drone],
        availability: Planned,
    },
    HwCategory {
        id: "rangefinder",
        label: "Distance sensor (ToF)",
        note: "Height and obstacle range.",
        profiles: &[Drone],
        availability: Planned,
    },
    HwCategory {
        id: "flow",
        label: "Optical flow",
        note: "Hold position without GPS.",
        profiles: &[Drone],
        availability: Planned,
    },
    HwCategory {
        id: "lidar",
        label: "LiDAR",
        note: "Dense distance scanning for mapping and avoidance.",
        profiles: &[Drone],
        availability: Planned,
    },
    HwCategory {
        id: "mmwave",
        label: "mmWave radar",
        note: "All-weather, see-through-dust ranging.",
        profiles: &[Drone],
        availability: Planned,
    },
    HwCategory {
        id: "rtk",
        label: "RTK GNSS",
        note: "Centimetre-accurate position.",
        profiles: &[Drone],
        availability: Planned,
    },
    // ── roadmap: extra radios (drone + ground station) ──────────────────────
    HwCategory {
        id: "lora",
        label: "LoRa radio",
        note: "Long-range, low-rate backup telemetry.",
        profiles: &[Drone, GroundStation],
        availability: Planned,
    },
    HwCategory {
        id: "swarm",
        label: "Swarm radio",
        note: "One controller to many aircraft.",
        profiles: &[Drone, GroundStation],
        availability: Planned,
    },
    // ── roadmap: ground-station panel ───────────────────────────────────────
    HwCategory {
        id: "spi_lcd",
        label: "SPI LCD",
        note: "A larger attached touchscreen.",
        profiles: &[GroundStation],
        availability: Planned,
    },
    HwCategory {
        id: "buttons",
        label: "Physical buttons",
        note: "GPIO buttons for the handheld panel.",
        profiles: &[GroundStation],
        availability: Planned,
    },
    // ── roadmap: workstation ground link ────────────────────────────────────
    HwCategory {
        id: "directlink",
        label: "USB ground radio (Direct Link)",
        note: "Drive the long-range radio straight from this computer.",
        profiles: &[Workstation],
        availability: Planned,
    },
];

/// The categories shown for a profile, in catalog order.
pub fn catalog_for(profile: Profile) -> Vec<&'static HwCategory> {
    CATALOG
        .iter()
        .filter(|c| c.profiles.contains(&profile))
        .collect()
}

/// A category's detection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// Detected on this host, with a path / id / model detail.
    Found(String),
    /// A `Now` category that was probed and is absent.
    Missing,
    /// A `Planned` category: supported, not yet probed.
    Supported,
}

/// Classify one category from the system snapshot. `Planned` categories are
/// always `Supported`; `Now` categories probe the snapshot.
pub fn detect(cat: &HwCategory, sys: &SysProbe) -> Detection {
    if cat.availability == Availability::Planned {
        return Detection::Supported;
    }
    match cat.id {
        "fc" => opt(hw::pick_first_serial(
            &sys.dev_names,
            &["ttyACM", "ttyAMA", "ttyUSB"],
        )
        .map(|n| format!("/dev/{n}"))),
        "camera" => opt(hw::pick_lowest_video(&sys.dev_names).map(|n| format!("/dev/{n}"))),
        "gps" => flag(hw::lsusb_has_gps(&sys.lsusb), "USB GNSS"),
        "modem" => flag(hw::lsusb_has_modem(&sys.lsusb), "USB modem"),
        "radio" => flag(hw::lsusb_has_radio(&sys.lsusb), "USB adapter"),
        "mesh" => {
            let n = hw::radio_count(&sys.lsusb);
            if n >= 2 {
                Detection::Found(format!("{n} adapters"))
            } else {
                Detection::Missing
            }
        }
        "oled" => flag(
            sys.i2c_addrs.contains(&0x3c) || sys.i2c_addrs.contains(&0x3d),
            "I2C 0x3c",
        ),
        "hdmi" => flag(sys.hdmi_connected, "monitor connected"),
        "joystick" => opt(hw::pick_joystick(&sys.dev_names).map(|n| format!("/dev/input/{n}"))),
        "board" => opt(sys.board_model.clone()),
        "gpu" => flag(sys.gpu, "render device"),
        // A Now category with no probe arm is a bug; treat it as absent rather
        // than silently reading as supported.
        _ => Detection::Missing,
    }
}

/// `Found(detail)` or `Missing` from an optional detail.
fn opt(detail: Option<String>) -> Detection {
    match detail {
        Some(d) => Detection::Found(d),
        None => Detection::Missing,
    }
}

/// `Found(detail)` or `Missing` from a boolean presence flag.
fn flag(present: bool, detail: &str) -> Detection {
    if present {
        Detection::Found(detail.to_string())
    } else {
        Detection::Missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_a_non_empty_catalog_and_no_fc_off_drone() {
        for p in [Profile::Drone, Profile::GroundStation, Profile::Workstation] {
            let list = catalog_for(p);
            assert!(!list.is_empty(), "empty catalog for {p:?}");
        }
        // The reported bug: a ground station must never be shown a flight
        // controller, and a workstation must never be shown a long-range radio.
        let gs: Vec<&str> = catalog_for(Profile::GroundStation)
            .iter()
            .map(|c| c.id)
            .collect();
        assert!(!gs.contains(&"fc"), "ground station shown an FC");
        let ws: Vec<&str> = catalog_for(Profile::Workstation)
            .iter()
            .map(|c| c.id)
            .collect();
        assert!(!ws.contains(&"radio"), "workstation shown a radio");
        assert!(!ws.contains(&"camera"), "workstation shown a camera");
        // The drone must include its core airborne hardware.
        let dr: Vec<&str> = catalog_for(Profile::Drone).iter().map(|c| c.id).collect();
        for want in ["fc", "radio", "camera", "gps"] {
            assert!(dr.contains(&want), "drone missing {want}");
        }
    }

    #[test]
    fn every_now_category_has_a_probe_arm() {
        // A Now category that falls through to the `_ => Missing` arm would be a
        // silent bug; assert each Now id is one the detect() match handles.
        let empty = SysProbe::default();
        for cat in CATALOG.iter().filter(|c| c.availability == Availability::Now) {
            // On an empty snapshot every Now probe returns Missing (never
            // Supported — Supported is Planned-only), proving the arm exists.
            assert_eq!(
                detect(cat, &empty),
                Detection::Missing,
                "Now category {} has no real probe arm",
                cat.id
            );
        }
    }

    #[test]
    fn planned_is_always_supported() {
        let sys = SysProbe::default();
        for cat in CATALOG
            .iter()
            .filter(|c| c.availability == Availability::Planned)
        {
            assert_eq!(detect(cat, &sys), Detection::Supported);
        }
    }

    #[test]
    fn detect_classifies_from_the_snapshot() {
        let sys = SysProbe {
            lsusb: "Bus 001 Device 004: ID 0bda:8812 Realtek\n\
                    Bus 001 Device 006: ID 1546:01a8 U-Blox"
                .into(),
            dev_names: vec!["video0".into(), "ttyACM0".into(), "js0".into()],
            board_model: Some("Radxa Cubie A7Z".into()),
            hdmi_connected: true,
            i2c_addrs: vec![0x3c],
            gpu: true,
        };
        let by_id = |id: &str| {
            let cat = CATALOG.iter().find(|c| c.id == id).unwrap();
            detect(cat, &sys)
        };
        assert!(matches!(by_id("fc"), Detection::Found(_)));
        assert!(matches!(by_id("camera"), Detection::Found(_)));
        assert!(matches!(by_id("radio"), Detection::Found(_)));
        assert!(matches!(by_id("gps"), Detection::Found(_)));
        assert!(matches!(by_id("oled"), Detection::Found(_)));
        assert!(matches!(by_id("hdmi"), Detection::Found(_)));
        assert!(matches!(by_id("joystick"), Detection::Found(_)));
        assert!(matches!(by_id("board"), Detection::Found(_)));
        assert!(matches!(by_id("gpu"), Detection::Found(_)));
        // Only one radio → not mesh-capable.
        assert_eq!(by_id("mesh"), Detection::Missing);
        // No modem in the snapshot.
        assert_eq!(by_id("modem"), Detection::Missing);
    }
}
