//! Packet payload structs, byte-exact per protocol v1. Every `t_ns` is hub
//! time. Reserved fields are written as zero and ignored on receive.

use crate::wire::{Reader, Writer};

/// Packet type bytes.
pub mod ty {
    /// Hub identification, sent on connect.
    pub const HELLO: u8 = 0x01;
    /// A batch of evenly spaced IMU samples.
    pub const IMU_BATCH: u8 = 0x10;
    /// One barometer reading.
    pub const BARO: u8 = 0x11;
    /// One magnetometer reading.
    pub const MAG: u8 = 0x12;
    /// One camera trigger event.
    pub const TRIGGER: u8 = 0x20;
    /// One PPS edge and the hub clock discipline state.
    pub const PPS: u8 = 0x30;
    /// Raw GNSS receiver bytes passed through the hub.
    pub const GNSS_RAW: u8 = 0x40;
    /// RTCM correction bytes for the hub to forward to a GNSS port.
    pub const RTCM_IN: u8 = 0x41;
    /// Hub configuration.
    pub const CONFIG: u8 = 0x50;
    /// Periodic hub health counters.
    pub const HEALTH: u8 = 0x60;
}

/// Payload sizes of the fixed-length packets.
pub const HELLO_LEN: usize = 40;
/// Fixed header size of an [`ImuBatch`] payload.
pub const IMU_HEADER_LEN: usize = 20;
/// Size of one [`ImuSample`] on the wire.
pub const IMU_SAMPLE_LEN: usize = 14;
/// Most samples one [`ImuBatch`] may carry.
pub const IMU_MAX_SAMPLES: usize = 64;
/// Payload size of [`Baro`].
pub const BARO_LEN: usize = 20;
/// Payload size of [`Mag`].
pub const MAG_LEN: usize = 24;
/// Payload size of [`Trigger`].
pub const TRIGGER_LEN: usize = 28;
/// Payload size of [`Pps`].
pub const PPS_LEN: usize = 24;
/// Fixed header size of a [`GnssRaw`] payload.
pub const GNSS_HEADER_LEN: usize = 10;
/// Most receiver bytes one [`GnssRaw`] may carry.
pub const GNSS_MAX_BYTES: usize = 1014;
/// Fixed header size of an [`RtcmIn`] payload.
pub const RTCM_HEADER_LEN: usize = 2;
/// Most correction bytes one [`RtcmIn`] may carry.
pub const RTCM_MAX_BYTES: usize = 1022;
/// Payload size of [`Config`].
pub const CONFIG_LEN: usize = 16;
/// Payload size of [`Health`].
pub const HEALTH_LEN: usize = 40;

/// `Hello.board_id` of the simulator. Its hub clock is the simulator's time.
pub const BOARD_ID_SIM: u16 = 0xFFFF;
/// `Hello.board_id` of the Matek H743-SLIM V3.
pub const BOARD_ID_MATEK_H743_SLIM_V3: u16 = 1;
/// Protocol version this crate implements.
pub const PROTO_VER: u16 = 1;

/// Gyro LSB in rad/s (±2000 dps full scale).
pub const GYRO_LSB_RAD_S: f64 = 2000.0 / 32768.0 * core::f64::consts::PI / 180.0;
/// Accel LSB in m/s² (±16 g full scale, g = 9.80665).
pub const ACCEL_LSB_M_S2: f64 = 16.0 / 32768.0 * 9.80665;

/// Hub identification and capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Hello {
    /// Protocol version the hub speaks (see [`PROTO_VER`]).
    pub proto_ver: u16,
    /// Firmware major version.
    pub fw_major: u8,
    /// Firmware minor version.
    pub fw_minor: u8,
    /// Firmware patch version.
    pub fw_patch: u8,
    /// Board identifier.
    pub board_id: u16,
    /// Capability bits, see [`caps`].
    pub caps: u32,
    /// MCU unique ID.
    pub uid: [u8; 12],
    /// Board name, NUL-padded ASCII.
    pub board_name: [u8; 16],
}

/// `Hello.caps` bits.
pub mod caps {
    /// First IMU present.
    pub const IMU0: u32 = 1 << 0;
    /// Second IMU present.
    pub const IMU1: u32 = 1 << 1;
    /// Barometer present.
    pub const BARO: u32 = 1 << 2;
    /// Magnetometer present.
    pub const MAG: u32 = 1 << 3;
    /// Camera trigger output.
    pub const TRIGGER_OUT: u32 = 1 << 4;
    /// Camera strobe (exposure) input.
    pub const STROBE_IN: u32 = 1 << 5;
    /// PPS input.
    pub const PPS_IN: u32 = 1 << 6;
    /// GNSS byte passthrough.
    pub const GNSS_PASSTHROUGH: u32 = 1 << 7;
    /// RTCM correction input.
    pub const RTCM_IN: u32 = 1 << 8;
    /// PPS output.
    pub const PPS_OUT: u32 = 1 << 9;
    /// GPRMC sentence output.
    pub const GPRMC_OUT: u32 = 1 << 10;
    /// IMU heater.
    pub const HEATER: u32 = 1 << 11;
}

/// One raw IMU sample in the chip frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImuSample {
    /// Gyro X, Y, Z in raw counts (see [`GYRO_LSB_RAD_S`]).
    pub gyro: [i16; 3],
    /// Accel X, Y, Z in raw counts (see [`ACCEL_LSB_M_S2`]).
    pub accel: [i16; 3],
    /// °C × 100.
    pub temp: i16,
}

impl ImuSample {
    pub(crate) fn write(&self, w: &mut Writer<'_>) {
        for v in self.gyro.iter().chain(self.accel.iter()) {
            w.i16(*v);
        }
        w.i16(self.temp);
    }
    pub(crate) fn read(r: &mut Reader<'_>) -> Self {
        let gyro = [r.i16(), r.i16(), r.i16()];
        let accel = [r.i16(), r.i16(), r.i16()];
        Self {
            gyro,
            accel,
            temp: r.i16(),
        }
    }
}

/// Samples of an `ImuBatch`: typed on the encode side, raw wire bytes on the
/// decode side (no copy, no allocation).
#[derive(Debug, Clone, Copy)]
pub enum ImuSamples<'a> {
    /// Decoded samples, used when encoding.
    Typed(&'a [ImuSample]),
    /// Wire bytes, [`IMU_SAMPLE_LEN`] per sample, as decoded.
    Raw(&'a [u8]),
}

impl<'a> ImuSamples<'a> {
    /// Number of samples.
    pub fn len(&self) -> usize {
        match self {
            Self::Typed(s) => s.len(),
            Self::Raw(b) => b.len() / IMU_SAMPLE_LEN,
        }
    }
    /// Whether there are no samples.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Sample `k`, or `None` past the end.
    pub fn get(&self, k: usize) -> Option<ImuSample> {
        match self {
            Self::Typed(s) => s.get(k).copied(),
            Self::Raw(b) => b
                .get(k * IMU_SAMPLE_LEN..(k + 1) * IMU_SAMPLE_LEN)
                .map(|c| ImuSample::read(&mut Reader::new(c))),
        }
    }
    /// Iterate over the samples in order.
    pub fn iter(&self) -> impl Iterator<Item = ImuSample> + '_ {
        (0..self.len()).filter_map(move |k| self.get(k))
    }
}

impl PartialEq for ImuSamples<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

/// A batch of IMU samples taken every `dt_ns` from `t0_ns`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImuBatch<'a> {
    /// Hub time of the first sample.
    pub t0_ns: u64,
    /// Sample spacing.
    pub dt_ns: u32,
    /// Which IMU produced the batch.
    pub imu_id: u8,
    /// Flag bits.
    pub flags: u8,
    /// Sequence number of the first sample.
    pub seq0: u32,
    /// The samples, 1 to [`IMU_MAX_SAMPLES`].
    pub samples: ImuSamples<'a>,
}

/// `ImuBatch.flags` bits.
pub mod imu_flags {
    /// A gyro axis hit full scale.
    pub const GYRO_CLIPPED: u8 = 1 << 0;
    /// An accel axis hit full scale.
    pub const ACCEL_CLIPPED: u8 = 1 << 1;
    /// The hub clock was PPS locked.
    pub const PPS_LOCKED: u8 = 1 << 2;
    /// The IMU FIFO overflowed before this batch.
    pub const FIFO_OVERFLOW: u8 = 1 << 3;
}

impl ImuBatch<'_> {
    /// Hub time of sample `k`. Wraps on overflow, like the hub timebase, so a
    /// hostile `t0_ns` cannot panic a debug build.
    pub fn sample_t_ns(&self, k: u32) -> u64 {
        self.t0_ns
            .wrapping_add(u64::from(k).wrapping_mul(u64::from(self.dt_ns)))
    }
}

/// One barometer reading.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Baro {
    /// Hub time of the reading.
    pub t_ns: u64,
    /// Static pressure in pascals.
    pub pressure_pa: f32,
    /// Sensor temperature in degrees Celsius.
    pub temp_c: f32,
    /// Which barometer.
    pub baro_id: u8,
}

/// One magnetometer reading.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Mag {
    /// Hub time of the reading.
    pub t_ns: u64,
    /// Field X, Y, Z in microtesla.
    pub field_ut: [f32; 3],
    /// Which magnetometer.
    pub mag_id: u8,
}

/// One camera trigger event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Trigger {
    /// Trigger sequence number.
    pub seq: u32,
    /// Bit mask of the cameras triggered.
    pub cam_mask: u8,
    /// Flag bits.
    pub flags: u8,
    /// Hub time the exposure started.
    pub t_start_ns: u64,
    /// Hub time of mid-exposure.
    pub t_mid_ns: u64,
    /// Exposure length.
    pub exposure_ns: u32,
}

/// `Trigger.flags` bits.
pub mod trigger_flags {
    /// Timing comes from a measured strobe, not the commanded trigger.
    pub const STROBE_MEASURED: u8 = 1 << 0;
    /// The hub clock was PPS locked.
    pub const PPS_LOCKED: u8 = 1 << 1;
    /// The trigger is phase locked to PPS.
    pub const PHASE_LOCKED: u8 = 1 << 2;
}

/// PPS lock state (`Pps.locked`, `Health.pps_state`).
pub mod pps_state {
    /// No PPS seen.
    pub const NONE: u8 = 0;
    /// PPS seen, discipline not settled.
    pub const ACQUIRING: u8 = 1;
    /// Hub clock locked to PPS.
    pub const LOCKED: u8 = 2;
    /// PPS lost, hub clock free-running on the last correction.
    pub const HOLDOVER: u8 = 3;
}

/// One PPS edge and the hub clock discipline state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Pps {
    /// Hub time of the reading.
    pub t_ns: u64,
    /// Lock state, see [`pps_state`].
    pub locked: u8,
    /// PPS edges seen since boot.
    pub pps_count: u32,
    /// Phase error of this edge.
    pub phase_err_ns: i32,
    /// Frequency correction applied to the hub clock, in parts per billion.
    pub freq_corr_ppb: i32,
}

/// Raw GNSS receiver bytes passed through the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GnssRaw<'a> {
    /// Hub time of the reading.
    pub t_ns: u64,
    /// Hub serial port.
    pub port: u8,
    /// The bytes, unparsed.
    pub bytes: &'a [u8],
}

/// RTCM correction bytes for the hub to forward to a GNSS port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtcmIn<'a> {
    /// Hub serial port.
    pub port: u8,
    /// The bytes, unparsed.
    pub bytes: &'a [u8],
}

/// Hub configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Config {
    /// Camera trigger rate.
    pub trigger_hz: u16,
    /// Commanded exposure, 0 for camera-controlled.
    pub exposure_us: u32,
    /// IMU output data rate.
    pub imu_odr_hz: u16,
    /// Bit mask of the cameras triggered.
    pub cam_mask: u8,
    /// Trigger mode, see [`trigger_mode`].
    pub trigger_mode: u8,
    /// Nonzero enables the PPS output.
    pub pps_out: u8,
    /// Nonzero enables the GPRMC output.
    pub gprmc_out: u8,
    /// IMU samples per [`ImuBatch`].
    pub imu_batch: u8,
    /// IMU heater target in degrees Celsius, or [`HEATER_OFF`].
    pub heater_target_c: i8,
}

/// `Config.trigger_mode` values.
pub mod trigger_mode {
    /// No triggering.
    pub const OFF: u8 = 0;
    /// Trigger on the hub clock.
    pub const FREE_RUN: u8 = 1;
    /// Trigger phase locked to PPS.
    pub const PPS_LOCKED: u8 = 2;
}

/// `Config.heater_target_c` value meaning heater off.
pub const HEATER_OFF: i8 = -128;

impl Default for Config {
    fn default() -> Self {
        Self {
            trigger_hz: 20,
            exposure_us: 0,
            imu_odr_hz: 1000,
            cam_mask: 0,
            trigger_mode: trigger_mode::FREE_RUN,
            pps_out: 0,
            gprmc_out: 0,
            imu_batch: 10,
            heater_target_c: 45,
        }
    }
}

/// Periodic hub health counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Health {
    /// Hub time of the reading.
    pub t_ns: u64,
    /// Seconds since boot.
    pub uptime_s: u32,
    /// MCU temperature in degrees Celsius.
    pub mcu_temp_c: i16,
    /// IMU temperature in degrees Celsius.
    pub imu_temp_c: i16,
    /// Input voltage in millivolts.
    pub vin_mv: u16,
    /// Frames dropped on the host link.
    pub usb_tx_drops: u32,
    /// IMU FIFO overflows since boot.
    pub imu_fifo_overflows: u32,
    /// Received frames rejected for CRC.
    pub rx_crc_errors: u32,
    /// Triggers missed since boot.
    pub trigger_missed: u32,
    /// PPS state, see [`pps_state`].
    pub pps_state: u8,
    /// Heater duty cycle in percent.
    pub heater_duty_pct: u8,
}

/// One decoded packet. Variable-length payloads borrow from the decode
/// buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Packet<'a> {
    /// [`Hello`].
    Hello(Hello),
    /// [`ImuBatch`].
    ImuBatch(ImuBatch<'a>),
    /// [`Baro`].
    Baro(Baro),
    /// [`Mag`].
    Mag(Mag),
    /// [`Trigger`].
    Trigger(Trigger),
    /// [`Pps`].
    Pps(Pps),
    /// [`GnssRaw`].
    GnssRaw(GnssRaw<'a>),
    /// [`RtcmIn`].
    RtcmIn(RtcmIn<'a>),
    /// [`Config`].
    Config(Config),
    /// [`Health`].
    Health(Health),
    /// A type this version does not know; receivers count and skip it.
    Unknown {
        /// The type byte.
        ty: u8,
        /// The raw payload.
        payload: &'a [u8],
    },
}
