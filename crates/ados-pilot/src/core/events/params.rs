//! The FC parameters the pilot reads, requested until the FC has sent them.

use super::Pilot;

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
    pub(super) fn request_params(&mut self, now_ms: u64) {
        self.params_ms = now_ms;
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
        for n in names {
            if !self.fc.params.contains_key(n) {
                let m = self.tx().param_request(n);
                self.mav(m);
            }
        }
    }
}
