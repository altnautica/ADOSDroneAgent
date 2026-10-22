//! The `clock` slice of the agent config.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fit::LatencyFloor;

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct SimSection {
    latency_floor_ns: Option<u64>,
}

/// The `clock` slice as it sits in the config; `Default` is the schema default.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct ClockSection {
    sim: SimSection,
}

/// The latency floor under the video SEI pairs, from the agent config at
/// `path`. A malformed file or slice is reported through the shared loader
/// and the config-status sidecar, and the floor stays unknown.
pub fn sim_floor(path: &Path) -> LatencyFloor {
    let (sec, err) = match ados_config::schema::load_slice::<ClockSection>(path, "/clock") {
        Ok(s) => (s, None),
        Err(e) => (ClockSection::default(), Some(e)),
    };
    ados_config::write_config_status("clock", err.as_deref());
    floor_of(&sec)
}

fn floor_of(sec: &ClockSection) -> LatencyFloor {
    sec.sim
        .latency_floor_ns
        .map_or(LatencyFloor::Unknown, LatencyFloor::Known)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_slice_conforms() {
        ados_config::schema::assert_slice_conforms::<ClockSection>("/clock");
    }

    #[test]
    fn a_configured_floor_is_known() {
        assert_eq!(floor_of(&ClockSection::default()), LatencyFloor::Unknown);
        let sec: ClockSection =
            serde_json::from_value(serde_json::json!({"sim": {"latency_floor_ns": 12_000_000}}))
                .unwrap();
        assert_eq!(floor_of(&sec), LatencyFloor::Known(12_000_000));
    }
}
