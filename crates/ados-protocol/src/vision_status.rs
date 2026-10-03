//! The vision engine's work sidecar: proof that the engine is consuming frames.
//!
//! `ados-vision` takes every frame over a Unix socket (the video pipeline's
//! tap) and hands it on over shared memory and more Unix sockets. None of that
//! moves the process's `/proc/<pid>/io` `rchar`/`wchar` counters, which follow
//! only the `read(2)`/`write(2)` file path, so a fully fed engine reads as
//! flat there. The engine instead publishes the cumulative count of frames it
//! has consumed, plus how many of its inputs are delivering right now, and the
//! supervisor judges it on that count.
//!
//! The count only means something while an input is delivering: an engine
//! whose camera is absent, or whose tap has gone silent, is idle, not wedged.
//! [`VisionStatus::work_counter`] therefore answers only while at least one
//! input is live. A source that delivers nothing for a while is closed by the
//! engine and stops counting as live, so a silent upstream never ages into a
//! stall verdict; an engine wedged anywhere else keeps its inputs live with a
//! frozen count, which is exactly a stall.
//!
//! Producer: `ados-vision`, rewritten about once a second under its socket
//! directory. Consumer: the supervisor's work proof.

use serde::{Deserialize, Serialize};

/// The sidecar's file name under the run directory (`/run/ados` by default).
pub const VISION_STATUS_FILE: &str = "vision-status.json";

/// The sidecar schema version, stamped on write.
pub const VISION_STATUS_VERSION: u16 = 1;

/// The engine's work state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisionStatus {
    #[serde(default)]
    pub version: u16,
    /// Frames consumed from every input since the engine started.
    pub frames_consumed: u64,
    /// Inputs that have delivered a frame since they were last (re)opened and
    /// have not since failed or gone silent.
    pub live_inputs: u32,
}

impl VisionStatus {
    /// The work counter to judge the engine on: the frames consumed, but only
    /// while an input is delivering. `None` (no verdict) otherwise.
    pub fn work_counter(&self) -> Option<u64> {
        (self.live_inputs > 0).then_some(self.frames_consumed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_is_a_verdict_only_while_an_input_is_delivering() {
        let fed = VisionStatus {
            version: VISION_STATUS_VERSION,
            frames_consumed: 42,
            live_inputs: 1,
        };
        assert_eq!(fed.work_counter(), Some(42));
        // No camera, or a silent tap: idle, never a stall.
        let idle = VisionStatus {
            live_inputs: 0,
            ..fed
        };
        assert_eq!(idle.work_counter(), None);
    }

    #[test]
    fn the_wire_shape_round_trips_and_tolerates_a_missing_version() {
        let s = VisionStatus {
            version: VISION_STATUS_VERSION,
            frames_consumed: 7,
            live_inputs: 2,
        };
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<VisionStatus>(&text).unwrap(), s);
        let old: VisionStatus =
            serde_json::from_str(r#"{"frames_consumed":3,"live_inputs":1}"#).unwrap();
        assert_eq!(old.version, 0);
        assert_eq!(old.work_counter(), Some(3));
    }
}
