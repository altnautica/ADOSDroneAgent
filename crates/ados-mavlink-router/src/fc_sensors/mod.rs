//! The flight-controller sensor lane on the sensor bus (`sensors.sock`).
//!
//! Out: the vehicle's `HIGHRES_IMU` (or `RAW_IMU`), `SCALED_PRESSURE` and
//! `CAMERA_FEEDBACK` as `imu` (`fc_imu` / `fc_raw_imu`), `baro` (`fc_baro`)
//! and `trigger` (`cam0`) records, every time on the host monotonic clock
//! through a TIMESYNC fit, plus the accepted TIMESYNC pairs as `sync` (`fc`)
//! so the clock service fits the same relation. In: `rtcm` frames, injected
//! into the FC as `GPS_RTCM_DATA`.
//!
//! The stream rates that make these messages flow are requested by the send
//! cadence ([`crate::connection`]), which reads the capture marker this lane
//! polls and the `HIGHRES_IMU` sightings it reports.

pub mod convert;
pub mod lane;
pub mod rtcm;
pub mod task;
pub mod timesync;

#[cfg(test)]
mod tests;
