//! The FC parameters the pilot reads, requested until the FC has sent them.
//!
//! ArduPilot queues by-name reads in a 20-deep request queue shared by every
//! link and drops a read that does not fit. The pilot once sent every missing
//! name in one burst every 15 s, so most were dropped, and a parameter could
//! stay unread for a minute. It now asks for at most [`PARAM_BURST`] names
//! per FC heartbeat, fewest-asked first, one name no oftener than every
//! [`PARAM_ASK_GAP_MS`]. A name the FC answers `PARAM_ERROR`
//! DOES_NOT_EXIST, or asked [`crate::fc::PARAM_ABSENT_ASKS`] times without an
//! answer, is absent from the firmware; those are asked again every
//! [`PARAM_RETRY_MS`].

use super::Pilot;
use crate::fc::PARAM_ABSENT_ASKS;

/// Names asked per FC heartbeat: half the FC's request queue, which other
/// links share.
const PARAM_BURST: usize = 10;
/// Names taken as absent are asked again this often.
const PARAM_RETRY_MS: u64 = 15_000;
/// One name is asked again no sooner than this, so the reads that make it
/// absent span seconds: a full reply queue makes the FC's answers wait.
const PARAM_ASK_GAP_MS: u64 = 2_000;

/// FC parameters the pilot reads.
const EXTRA_PARAMS: &[&str] = &[
    "Q_ENABLE",
    "FLTMODE_CH",
    "BATT_LOW_MAH",
    "BATT_CAPACITY",
    "TECS_CLMB_MAX",
    "TECS_SINK_MAX",
    "AIRSPEED_MIN",
    "AIRSPEED_MAX",
    "AIRSPEED_CRUISE",
    "TKOFF_ALT",
    // The landing speed: `LAND_SPD_MS` (m/s), `LAND_SPEED` (cm/s) before it.
    "LAND_SPD_MS",
    "LAND_SPEED",
    // The default waypoint speed the confined cap restores in AUTO: `WP_SPD`
    // (m/s), `WPNAV_SPEED` (cm/s) before it.
    "WP_SPD",
    "WPNAV_SPEED",
    // The EKF's horizontal position source per source set (3 = GPS): the
    // GNSS-denied speed cap.
    "EK3_SRC1_POSXY",
    "EK3_SRC2_POSXY",
    "EK3_SRC3_POSXY",
    // Visual odometry enabled: with it the EKF's relative position counts for
    // the `position` readiness item, as with `FLOW_TYPE`.
    "VISO_TYPE",
    // The FC's guidance limits the stick profile is held to
    // (`crate::model::fc_capped`), with their older names.
    "WP_ACC",
    "WPNAV_ACCEL",
    "PSC_NE_JERK",
    "PSC_JERK_XY",
    "WP_ACC_Z",
    "WPNAV_ACCEL_Z",
    "PSC_D_JERK",
    "PSC_JERK_Z",
    "ATC_ACC_Y_MAX",
    "ATC_ACCEL_Y_MAX",
];

impl Pilot {
    /// Once per FC heartbeat: ask for the next missing parameters.
    pub(super) fn request_params(&mut self, now_ms: u64) {
        let Some(fw) = self.fc.firmware else { return };
        let class = self.fc.vehicle_class();
        // The flow, rangefinder and battery-failsafe rules read ArduPilot
        // parameters only.
        let ardupilot = fw == crate::fc_profile::Firmware::Ardupilot;
        let sensors = ardupilot
            .then(crate::fc_sensor_profile::param_names)
            .unwrap_or_default();
        let batt = if ardupilot {
            crate::batt_failsafe::PARAMS
        } else {
            &[]
        };
        let batt_legacy = if ardupilot {
            crate::batt_failsafe::LEGACY_PARAMS
        } else {
            &[]
        };
        // The steady take-off's and airframe health's parameters.
        let liftoff: &[&[&str]] = if ardupilot {
            &[crate::liftoff::PARAMS, crate::airframe::PARAMS]
        } else {
            &[]
        };
        // Under either RC declaration: the fence rows are read in advance.
        let mut names: Vec<&str> = crate::no_rc::all_names(fw, class)
            .chain(EXTRA_PARAMS.iter().copied())
            .chain(liftoff.iter().flat_map(|p| p.iter().copied()))
            .chain(batt.iter().chain(batt_legacy).copied())
            .chain(sensors.iter().map(String::as_str))
            .collect();
        names.sort_unstable();
        names.dedup();
        let asks = |n: &str| self.fc.param_asks.get(n).copied().unwrap_or(0);
        let mut missing: Vec<(u32, &str)> = names
            .into_iter()
            .filter(|n| !self.fc.params.contains_key(*n))
            .map(|n| (asks(n), n))
            .collect();
        // Still being read first; the absent ones only every PARAM_RETRY_MS.
        let reading = missing.iter().any(|&(a, _)| a < PARAM_ABSENT_ASKS);
        if !reading {
            if now_ms.saturating_sub(self.params_ms) < PARAM_RETRY_MS {
                return;
            }
            self.params_ms = now_ms;
        }
        missing.retain(|&(a, n)| {
            !reading
                || (a < PARAM_ABSENT_ASKS
                    && self
                        .param_asked_ms
                        .get(n)
                        .is_none_or(|&t| now_ms.saturating_sub(t) >= PARAM_ASK_GAP_MS))
        });
        missing.sort_unstable();
        let ask: Vec<String> = missing
            .iter()
            .take(PARAM_BURST)
            .map(|&(_, n)| n.to_owned())
            .collect();
        for n in ask {
            let m = self.tx().param_request(&n);
            self.mav(m);
            *self.fc.param_asks.entry(n.clone()).or_insert(0) += 1;
            self.param_asked_ms.insert(n, now_ms);
        }
    }
}
