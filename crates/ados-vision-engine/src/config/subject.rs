//! `vision.subject`: the designated subject's world estimate
//! (`vision.subject.v1`).

use ados_vision::subject::SubjectConfig;
use serde::Deserialize;

fn default_beacon_only_max_s() -> f32 {
    60.0
}

/// The upper bound a configured beacon-only time is held to, s.
const MAX_BEACON_ONLY_S: f32 = 3_600.0;

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SubjectSettings {
    /// How long, s, the estimate may run on the subject's phone beacon alone
    /// after vision last saw the subject. `0` keeps it from running on the
    /// beacon alone; beside vision the beacon still fuses.
    #[serde(default = "default_beacon_only_max_s")]
    pub beacon_only_max_s: f32,
}

impl Default for SubjectSettings {
    fn default() -> Self {
        Self {
            beacon_only_max_s: default_beacon_only_max_s(),
        }
    }
}

impl SubjectSettings {
    /// The estimator's tuning. A beacon-only time that is not a finite,
    /// non-negative number is the default; one past an hour is an hour.
    pub fn estimator(&self) -> SubjectConfig {
        let max_s = if self.beacon_only_max_s.is_finite() && self.beacon_only_max_s >= 0.0 {
            self.beacon_only_max_s.min(MAX_BEACON_ONLY_S)
        } else {
            default_beacon_only_max_s()
        };
        SubjectConfig {
            beacon_only: max_s > 0.0,
            beacon_only_max_s: max_s,
            ..SubjectConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_beacon_only_time_is_on_by_default_and_zero_turns_it_off() {
        let on = SubjectSettings::default().estimator();
        assert!(on.beacon_only && on.beacon_only_max_s == 60.0);
        let off = SubjectSettings {
            beacon_only_max_s: 0.0,
        }
        .estimator();
        assert!(!off.beacon_only);
        for bad in [f32::NAN, -5.0, f32::INFINITY] {
            let c = SubjectSettings {
                beacon_only_max_s: bad,
            }
            .estimator();
            assert_eq!(c.beacon_only_max_s, 60.0, "{bad}");
        }
        assert_eq!(
            SubjectSettings {
                beacon_only_max_s: 1e6
            }
            .estimator()
            .beacon_only_max_s,
            3_600.0
        );
    }
}
