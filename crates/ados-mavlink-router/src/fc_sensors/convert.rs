//! Pure conversions from flight-controller MAVLink sensor messages to the
//! sensor-bus payloads: axes, units, and source-clock unwrapping.

use ados_protocol::mavlink::ardupilotmega::{
    CAMERA_FEEDBACK_DATA, HIGHRES_IMU_DATA, RAW_IMU_DATA, SCALED_PRESSURE_DATA,
};
use ados_sensorbus::v1::{ImuSample, TriggerRecord};
use ados_sensorbus::Baro;

/// Standard gravity, m/s², the constant ArduPilot scales `RAW_IMU` by.
pub const GRAVITY_MSS: f64 = 9.80665;

/// A source timestamp in microseconds at or above this is Unix epoch time
/// (after 2001-09), not time since boot: no flight controller runs that long.
pub const EPOCH_US_THRESHOLD: u64 = 1_000_000_000_000_000;

/// Body FRD (MAVLink, x forward, y right, z down) to body FLU (bus contract,
/// x forward, y left, z up).
pub fn frd_to_flu(v: [f64; 3]) -> [f64; 3] {
    [v[0], -v[1], -v[2]]
}

/// One inertial sample from `HIGHRES_IMU` (SI units already), in FLU, before
/// its time and sequence are assigned.
pub fn highres_sample(m: &HIGHRES_IMU_DATA) -> ImuSample {
    let gyro = frd_to_flu([m.xgyro as f64, m.ygyro as f64, m.zgyro as f64]);
    let accel = frd_to_flu([m.xacc as f64, m.yacc as f64, m.zacc as f64]);
    ImuSample {
        t_ns: 0,
        seq: 0,
        gyro: gyro.to_vec(),
        accel: accel.to_vec(),
        temp_c: m.temperature.is_finite().then_some(m.temperature),
    }
}

/// One inertial sample from `RAW_IMU` as ArduPilot scales it (accel in
/// milli-g, gyro in mrad/s), converted to SI and FLU. The dialect's base
/// message carries no temperature.
pub fn raw_imu_sample(m: &RAW_IMU_DATA) -> ImuSample {
    let g = GRAVITY_MSS / 1000.0;
    let gyro = frd_to_flu([
        m.xgyro as f64 / 1000.0,
        m.ygyro as f64 / 1000.0,
        m.zgyro as f64 / 1000.0,
    ]);
    let accel = frd_to_flu([m.xacc as f64 * g, m.yacc as f64 * g, m.zacc as f64 * g]);
    ImuSample {
        t_ns: 0,
        seq: 0,
        gyro: gyro.to_vec(),
        accel: accel.to_vec(),
        temp_c: None,
    }
}

/// The bus barometer record for `SCALED_PRESSURE` (hPa to Pa, centi-degrees
/// to degrees) at host time `t_ns`.
pub fn baro(m: &SCALED_PRESSURE_DATA, t_ns: u64) -> Baro {
    Baro {
        t_ns,
        pressure_pa: m.press_abs as f64 * 100.0,
        temp_c: m.temperature as f32 / 100.0,
    }
}

/// The trigger record for one `CAMERA_FEEDBACK`. The message carries neither
/// exposure nor gain, so both are 0 (unknown).
pub fn trigger(seq: u64, t_mid_ns: u64) -> TriggerRecord {
    TriggerRecord {
        seq,
        t_mid_ns,
        exposure_ns: 0,
        gain: 0.0,
    }
}

/// The `CAMERA_FEEDBACK` time in FC boot microseconds. ArduPilot stamps the
/// feedback with GPS epoch time when it has it and boot time otherwise; an
/// epoch stamp is mapped back to boot time through the latest `SYSTEM_TIME`
/// pair `(time_unix_usec, time_boot_ms)`. `None` for an epoch stamp with no
/// pair to map it.
pub fn feedback_boot_us(m: &CAMERA_FEEDBACK_DATA, system_time: Option<(u64, u32)>) -> Option<u64> {
    if m.time_usec < EPOCH_US_THRESHOLD {
        return Some(m.time_usec);
    }
    let (unix_us, boot_ms) = system_time.filter(|(unix, _)| *unix >= EPOCH_US_THRESHOLD)?;
    let boot_at_epoch_zero = unix_us as i128 - boot_ms as i128 * 1000;
    u64::try_from(m.time_usec as i128 - boot_at_epoch_zero).ok()
}

/// Extends a wrapping counter of `bits` width to 64 bits. A step backwards of
/// more than half the range is a wrap; a smaller one is a reordering and
/// keeps the current epoch.
#[derive(Debug, Clone)]
pub struct Unwrapper {
    bits: u32,
    last: Option<u64>,
    epoch: u64,
}

impl Unwrapper {
    pub fn new(bits: u32) -> Self {
        Self {
            bits,
            last: None,
            epoch: 0,
        }
    }

    pub fn unwrap(&mut self, raw: u64) -> u64 {
        let range = 1u64 << self.bits;
        let raw = raw & (range - 1);
        if let Some(last) = self.last {
            if raw < last && last - raw > range / 2 {
                self.epoch += range;
            }
        }
        self.last = Some(raw);
        self.epoch + raw
    }

    /// Forget the history (the source restarted its counter).
    pub fn reset(&mut self) {
        self.last = None;
        self.epoch = 0;
    }
}
