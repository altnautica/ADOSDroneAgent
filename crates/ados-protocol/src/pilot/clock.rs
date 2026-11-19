//! The clock stamp on the pilot frame (`simclock.v1`).

use serde::{Deserialize, Serialize};

/// Which clock produced a state message.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ClockStamp {
    /// `wall`, or `sim` on a node following the simulator's clock.
    pub source: String,
    /// Simulator time of the message; omitted on wall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_ns: Option<u64>,
    /// Wall milliseconds since the latest sim tick arrived; omitted on wall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag_ms: Option<u32>,
    /// The simulator's pacing speed; omitted on wall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
}

impl ClockStamp {
    pub fn new(source: &str, sim_ns: Option<u64>, lag_ms: Option<u32>, speed: Option<f64>) -> Self {
        ClockStamp {
            source: source.to_owned(),
            sim_ns,
            lag_ms,
            speed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wall_stamp_carries_only_its_source() {
        let s = serde_json::to_string(&ClockStamp::new("wall", None, None, None)).unwrap();
        assert_eq!(s, r#"{"source":"wall"}"#);
        let sim = ClockStamp::new("sim", Some(5), Some(3), Some(2.0));
        let back: ClockStamp = serde_json::from_str(&serde_json::to_string(&sim).unwrap()).unwrap();
        assert_eq!(back, sim);
    }
}
