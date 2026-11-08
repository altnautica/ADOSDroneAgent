//! Per-firmware, per-class knowledge: step lists, mode numbers, parameter
//! prefixes. VTOL is the plane adapter plus the copter steps on `Q_` params.

pub mod ardupilot_copter;
pub mod ardupilot_plane;
pub mod px4;

use super::fcstate::{Class, Fw};
use super::schema::WizardId;

/// Flight modes the setup engine commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Guided,
    Land,
    Rtl,
    Loiter,
    Autotune,
    Takeoff,
    Fbwa,
    Fbwb,
    Qhover,
    Qloiter,
    Qautotune,
    Offboard,
    Posctl,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adapter {
    pub fw: Fw,
    pub class: Class,
}

impl Adapter {
    pub fn new(fw: Fw, class: Class) -> Self {
        Adapter { fw, class }
    }

    pub fn is_ardupilot(&self) -> bool {
        self.fw == Fw::ArduPilot
    }

    /// Whether the vehicle flies like a copter for this step: a multirotor,
    /// or a VTOL in its Q modes.
    pub fn copter_like(&self) -> bool {
        matches!(self.class, Class::Multirotor | Class::Vtol)
    }

    /// The step ids of a wizard, in order.
    pub fn steps(&self, wizard: WizardId) -> &'static [&'static str] {
        match (self.fw, wizard) {
            (Fw::Px4, WizardId::Ground) => px4::GROUND,
            (Fw::Px4, WizardId::Air) => px4::AIR,
            (_, WizardId::Ground) => match self.class {
                Class::Multirotor => ardupilot_copter::GROUND,
                Class::Fixedwing => ardupilot_plane::GROUND,
                Class::Vtol => ardupilot_plane::GROUND_VTOL,
            },
            (_, WizardId::Air) => match self.class {
                Class::Multirotor => ardupilot_copter::AIR,
                Class::Fixedwing => ardupilot_plane::AIR,
                Class::Vtol => ardupilot_plane::AIR_VTOL,
            },
        }
    }

    /// `(main, sub)` custom mode for DO_SET_MODE.
    pub fn mode(&self, m: Mode) -> Option<(u32, u32)> {
        match self.fw {
            Fw::Px4 => px4::mode(m),
            _ => match self.class {
                Class::Multirotor => ardupilot_copter::mode(m).map(|n| (n, 0)),
                Class::Fixedwing | Class::Vtol => ardupilot_plane::mode(m).map(|n| (n, 0)),
            },
        }
    }

    /// The heartbeat's `custom_mode` is `m`.
    pub fn in_mode(&self, custom_mode: u32, m: Mode) -> bool {
        match self.fw {
            Fw::Px4 => px4::mode(m).is_some_and(|(main, sub)| px4::decode(custom_mode) == (main, sub)),
            _ => self.mode(m).is_some_and(|(n, _)| n == custom_mode),
        }
    }

    /// Prefix of the attitude-controller parameters (`ATC` or `Q_A`).
    pub fn atc(&self) -> &'static str {
        if self.class == Class::Vtol { "Q_A" } else { "ATC" }
    }

    /// Prefix of the motor parameters (`MOT` or `Q_M`).
    pub fn mot(&self) -> &'static str {
        if self.class == Class::Vtol { "Q_M" } else { "MOT" }
    }

    /// Prefix of the copter autotune parameters (`AUTOTUNE` or `Q_AUTOTUNE`).
    pub fn autotune_prefix(&self) -> &'static str {
        if self.class == Class::Vtol { "Q_AUTOTUNE" } else { "AUTOTUNE" }
    }

    /// Size of the air-wizard tuning box (m from the start point): a plane's,
    /// or a VTOL's while it flies (or transitions) as one, else a hover's.
    pub fn tuning_box_m(&self, fw_flight: bool) -> f32 {
        if self.class == Class::Fixedwing || fw_flight { 400.0 } else { 20.0 }
    }

    /// The gains the setup report shows before and after tuning.
    pub fn report_gains(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        if self.fw == Fw::Px4 {
            for n in ["MC_ROLLRATE_P", "MC_ROLLRATE_I", "MC_ROLLRATE_D", "MC_PITCHRATE_P", "MC_PITCHRATE_I", "MC_PITCHRATE_D", "MC_YAWRATE_P", "MC_ROLL_P", "MC_PITCH_P"] {
                v.push(n.into());
            }
            return v;
        }
        if self.copter_like() {
            let a = self.atc();
            for s in ["RAT_RLL_P", "RAT_RLL_I", "RAT_RLL_D", "RAT_PIT_P", "RAT_PIT_I", "RAT_PIT_D", "RAT_YAW_P", "RAT_YAW_I", "ANG_RLL_P", "ANG_PIT_P", "ANG_YAW_P"] {
                v.push(format!("{a}_{s}"));
            }
        }
        if matches!(self.class, Class::Fixedwing | Class::Vtol) {
            for n in ["RLL_RATE_P", "RLL_RATE_I", "RLL_RATE_D", "RLL_RATE_FF", "PTCH_RATE_P", "PTCH_RATE_I", "PTCH_RATE_D", "PTCH_RATE_FF"] {
                v.push(n.into());
            }
        }
        v
    }

    /// The gain parameters an autotune changes, for "Revert gains".
    pub fn is_gain_param(&self, name: &str) -> bool {
        const AP: &[&str] = &[
            "ATC_RAT_", "ATC_ANG_", "ATC_ACCEL_", "ATC_ACC_", "Q_A_RAT_", "Q_A_ANG_", "Q_A_ACCEL_", "Q_A_ACC_",
            "RLL_RATE_", "PTCH_RATE_", "YAW_RATE_", "RLL2SRV_", "PTCH2SRV_",
        ];
        const PX4: &[&str] = &["MC_ROLLRATE_", "MC_PITCHRATE_", "MC_YAWRATE_", "MC_ROLL_P", "MC_PITCH_P", "MC_YAW_P", "FW_RR_", "FW_PR_", "FW_YR_"];
        let list = if self.fw == Fw::Px4 { PX4 } else { AP };
        list.iter().any(|p| name.starts_with(p))
    }
}
