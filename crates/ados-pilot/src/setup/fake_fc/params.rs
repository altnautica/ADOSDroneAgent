//! The parameter sets the fake FC boots with.

use std::collections::BTreeMap;

pub fn ap_copter_params() -> BTreeMap<String, f32> {
    let mut p = BTreeMap::new();
    for (k, v) in [
        ("FRAME_CLASS", 1.0),
        ("FRAME_TYPE", 1.0),
        ("BATT_MONITOR", 4.0),
        ("BATT_CAPACITY", 3300.0),
        ("BATT_VOLT_MULT", 10.1),
        ("ACRO_YAW_P", 4.5),
        ("ATC_ACCEL_P_MAX", 110_000.0),
        ("ATC_ACCEL_R_MAX", 110_000.0),
        ("ATC_ACCEL_Y_MAX", 27_000.0),
        ("ATC_RAT_PIT_FLTD", 20.0),
        ("ATC_RAT_PIT_FLTE", 0.0),
        ("ATC_RAT_PIT_FLTT", 20.0),
        ("ATC_RAT_RLL_FLTD", 20.0),
        ("ATC_RAT_RLL_FLTE", 0.0),
        ("ATC_RAT_RLL_FLTT", 20.0),
        ("ATC_RAT_YAW_FLTD", 0.0),
        ("ATC_RAT_YAW_FLTE", 2.0),
        ("ATC_RAT_YAW_FLTT", 20.0),
        ("ATC_THR_MIX_MAN", 0.1),
        ("ATC_RAT_RLL_P", 0.135),
        ("ATC_RAT_RLL_I", 0.135),
        ("ATC_RAT_RLL_D", 0.0036),
        ("ATC_RAT_PIT_P", 0.135),
        ("ATC_RAT_PIT_I", 0.135),
        ("ATC_RAT_PIT_D", 0.0036),
        ("ATC_RAT_YAW_P", 0.18),
        ("ATC_RAT_YAW_I", 0.018),
        ("ATC_ANG_RLL_P", 4.5),
        ("ATC_ANG_PIT_P", 4.5),
        ("ATC_ANG_YAW_P", 4.5),
        ("INS_ACCEL_FILTER", 20.0),
        ("INS_GYRO_FILTER", 20.0),
        ("INS_HNTCH_ENABLE", 0.0),
        ("MOT_THST_EXPO", 0.65),
        ("MOT_THST_HOVER", 0.35),
        ("MOT_HOVER_LEARN", 2.0),
        ("MOT_BAT_VOLT_MAX", 0.0),
        ("MOT_BAT_VOLT_MIN", 0.0),
        ("MOT_PWM_TYPE", 0.0),
        ("BATT_ARM_VOLT", 0.0),
        ("BATT_CRT_VOLT", 0.0),
        ("BATT_LOW_VOLT", 10.5),
        ("SERVO1_FUNCTION", 33.0),
        ("SERVO2_FUNCTION", 34.0),
        ("SERVO3_FUNCTION", 35.0),
        ("SERVO4_FUNCTION", 36.0),
        ("SERVO_BLH_RVMASK", 0.0),
        ("SERVO_BLH_AUTO", 0.0),
        ("SERVO_BLH_MASK", 0.0),
        ("FLTMODE_CH", 5.0),
        ("FLTMODE1", 0.0),
        ("FLTMODE2", 2.0),
        ("FLTMODE3", 5.0),
        ("FLTMODE4", 5.0),
        ("FLTMODE5", 6.0),
        ("FLTMODE6", 9.0),
        ("MAV_GCS_SYSID", 255.0),
        ("FS_GCS_ENABLE", 0.0),
        ("FS_GCS_TIMEOUT", 5.0),
        ("RC_OVERRIDE_TIME", 3.0),
        ("BATT_FS_LOW_ACT", 0.0),
        ("BATT_FS_CRT_ACT", 0.0),
        ("AUTOTUNE_AXES", 7.0),
        ("AUTOTUNE_AGGR", 0.1),
        ("RC1_MIN", 1100.0),
        ("RC1_MAX", 1900.0),
        ("RC1_TRIM", 1500.0),
    ] {
        p.insert(k.to_string(), v);
    }
    p
}

pub fn ap_plane_params() -> BTreeMap<String, f32> {
    let mut p = BTreeMap::new();
    for (k, v) in [
        ("SERVO1_FUNCTION", 4.0),
        ("SERVO2_FUNCTION", 19.0),
        ("SERVO3_FUNCTION", 70.0),
        ("SERVO4_FUNCTION", 21.0),
        ("SERVO1_REVERSED", 0.0),
        ("SERVO2_REVERSED", 0.0),
        ("SERVO4_REVERSED", 0.0),
        ("SERVO_AUTO_TRIM", 0.0),
        ("Q_ENABLE", 0.0),
        ("ARSPD_TYPE", 0.0),
        ("BATT_MONITOR", 4.0),
        ("BATT_CAPACITY", 5000.0),
        ("BATT_VOLT_MULT", 10.1),
        ("MAV_GCS_SYSID", 255.0),
        ("FS_GCS_ENABL", 0.0),
        ("FS_LONG_TIMEOUT", 5.0),
        ("FS_LONG_ACTN", 0.0),
        ("BATT_FS_LOW_ACT", 0.0),
        ("RC_OVERRIDE_TIME", 3.0),
        ("RLL_RATE_P", 0.08),
        ("PTCH_RATE_P", 0.08),
    ] {
        p.insert(k.to_string(), v);
    }
    p
}
