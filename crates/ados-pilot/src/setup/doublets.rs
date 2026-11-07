//! Plane AUTOTUNE input generator: alternating roll and pitch doublets,
//! steering back toward the tuning-box centre between them.

/// Roll doublet: ±45° bank demand for 2 s each way.
pub const ROLL_BANK_DEG: f32 = 45.0;
pub const ROLL_HALF_MS: u64 = 2000;
/// Pitch doublet: ±15° for 1.5 s each way.
pub const PITCH_DEG: f32 = 15.0;
pub const PITCH_HALF_MS: u64 = 1500;
/// Level flight between doublets.
pub const GAP_MS: u64 = 2000;
/// Beyond this fraction of the box radius the generator steers home first.
pub const RETURN_FRACTION: f32 = 0.6;
/// Bank used to steer back (deg).
pub const RETURN_BANK_DEG: f32 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Seg {
    RollPos,
    RollNeg,
    PitchUp,
    PitchDown,
    Gap,
    Return,
}

/// Stick demand `(roll, pitch)` in -1..1 for MANUAL_CONTROL `y`/`x`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Demand {
    pub roll: f32,
    pub pitch: f32,
}

#[derive(Debug, Clone)]
pub struct Doublets {
    seg: Seg,
    seg_start: u64,
    next_is_roll: bool,
    /// Full-stick bank and pitch (deg), from `ROLL_LIMIT_DEG`/`LIM_ROLL_CD`
    /// and `PTCH_LIM_MAX_DEG`/`LIM_PITCH_MAX`.
    roll_limit_deg: f32,
    pitch_limit_deg: f32,
    box_m: f32,
}

impl Doublets {
    pub fn new(now_ms: u64, roll_limit_deg: f32, pitch_limit_deg: f32, box_m: f32) -> Self {
        Doublets {
            seg: Seg::Gap,
            seg_start: now_ms,
            next_is_roll: true,
            roll_limit_deg: roll_limit_deg.max(1.0),
            pitch_limit_deg: pitch_limit_deg.max(1.0),
            box_m,
        }
    }

    fn seg_len(seg: Seg) -> u64 {
        match seg {
            Seg::RollPos | Seg::RollNeg => ROLL_HALF_MS,
            Seg::PitchUp | Seg::PitchDown => PITCH_HALF_MS,
            Seg::Gap => GAP_MS,
            Seg::Return => 1000,
        }
    }

    /// The demand at `now_ms`. `offset_ne` is the position relative to the
    /// box centre, `heading_rad` the current heading.
    pub fn demand(&mut self, now_ms: u64, offset_ne: [f32; 2], heading_rad: f32) -> Demand {
        let dist = (offset_ne[0].powi(2) + offset_ne[1].powi(2)).sqrt();
        if now_ms.saturating_sub(self.seg_start) >= Self::seg_len(self.seg) {
            self.seg_start = now_ms;
            self.seg = match self.seg {
                Seg::RollPos => Seg::RollNeg,
                Seg::PitchUp => Seg::PitchDown,
                Seg::RollNeg | Seg::PitchDown => Seg::Gap,
                Seg::Gap | Seg::Return => {
                    if dist > RETURN_FRACTION * self.box_m {
                        Seg::Return
                    } else if self.next_is_roll {
                        self.next_is_roll = false;
                        Seg::RollPos
                    } else {
                        self.next_is_roll = true;
                        Seg::PitchUp
                    }
                }
            };
        }
        let roll_k = ROLL_BANK_DEG / self.roll_limit_deg;
        let pitch_k = PITCH_DEG / self.pitch_limit_deg;
        match self.seg {
            Seg::RollPos => Demand { roll: roll_k.min(1.0), pitch: 0.0 },
            Seg::RollNeg => Demand { roll: -roll_k.min(1.0), pitch: 0.0 },
            Seg::PitchUp => Demand { roll: 0.0, pitch: pitch_k.min(1.0) },
            Seg::PitchDown => Demand { roll: 0.0, pitch: -pitch_k.min(1.0) },
            Seg::Gap => Demand { roll: 0.0, pitch: 0.0 },
            Seg::Return => {
                // Bearing to the centre against heading: turn toward it.
                let to_center = (-offset_ne[1]).atan2(-offset_ne[0]);
                let mut err = to_center - heading_rad;
                while err > std::f32::consts::PI {
                    err -= 2.0 * std::f32::consts::PI;
                }
                while err < -std::f32::consts::PI {
                    err += 2.0 * std::f32::consts::PI;
                }
                let bank = if err.abs() < 0.2 { 0.0 } else { RETURN_BANK_DEG * err.signum() };
                Demand { roll: (bank / self.roll_limit_deg).clamp(-1.0, 1.0), pitch: 0.0 }
            }
        }
    }
}
