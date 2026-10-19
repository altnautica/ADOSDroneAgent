//! Payload encode/decode for [`Packet`], byte-exact per protocol v1.

use crate::packet::*;
use crate::wire::{Reader, Writer};
use crate::Error;

impl<'a> Packet<'a> {
    /// Type byte on the wire.
    pub fn ty(&self) -> u8 {
        match self {
            Self::Hello(_) => ty::HELLO,
            Self::ImuBatch(_) => ty::IMU_BATCH,
            Self::Baro(_) => ty::BARO,
            Self::Mag(_) => ty::MAG,
            Self::Trigger(_) => ty::TRIGGER,
            Self::Pps(_) => ty::PPS,
            Self::GnssRaw(_) => ty::GNSS_RAW,
            Self::RtcmIn(_) => ty::RTCM_IN,
            Self::Config(_) => ty::CONFIG,
            Self::Health(_) => ty::HEALTH,
            Self::Unknown { ty, .. } => *ty,
        }
    }

    /// Payload length in bytes, validated against the per-type limits.
    pub fn payload_len(&self) -> Result<usize, Error> {
        Ok(match self {
            Self::Hello(_) => HELLO_LEN,
            Self::ImuBatch(b) => {
                let n = b.samples.len();
                if n == 0 || n > IMU_MAX_SAMPLES {
                    return Err(Error::ImuCount);
                }
                IMU_HEADER_LEN + IMU_SAMPLE_LEN * n
            }
            Self::Baro(_) => BARO_LEN,
            Self::Mag(_) => MAG_LEN,
            Self::Trigger(_) => TRIGGER_LEN,
            Self::Pps(_) => PPS_LEN,
            Self::GnssRaw(g) if g.bytes.len() > GNSS_MAX_BYTES => return Err(Error::Length),
            Self::GnssRaw(g) => GNSS_HEADER_LEN + g.bytes.len(),
            Self::RtcmIn(r) if r.bytes.len() > RTCM_MAX_BYTES => return Err(Error::Length),
            Self::RtcmIn(r) => RTCM_HEADER_LEN + r.bytes.len(),
            Self::Config(_) => CONFIG_LEN,
            Self::Health(_) => HEALTH_LEN,
            Self::Unknown { payload, .. } if payload.len() > crate::MAX_PAYLOAD => {
                return Err(Error::Length)
            }
            Self::Unknown { payload, .. } => payload.len(),
        })
    }

    /// Write the payload into `out` (exactly `payload_len()` bytes).
    pub(crate) fn write_payload(&self, out: &mut [u8]) {
        let mut w = Writer::new(out);
        match self {
            Self::Hello(h) => {
                w.u16(h.proto_ver);
                w.u8(h.fw_major);
                w.u8(h.fw_minor);
                w.u8(h.fw_patch);
                w.zeros(1);
                w.u16(h.board_id);
                w.u32(h.caps);
                w.bytes(&h.uid);
                w.bytes(&h.board_name);
            }
            Self::ImuBatch(b) => {
                w.u64(b.t0_ns);
                w.u32(b.dt_ns);
                // payload_len() has bounded the count to 1..=64.
                w.u8(b.samples.len() as u8);
                w.u8(b.imu_id);
                w.u8(b.flags);
                w.zeros(1);
                w.u32(b.seq0);
                for s in b.samples.iter() {
                    s.write(&mut w);
                }
            }
            Self::Baro(b) => {
                w.u64(b.t_ns);
                w.f32(b.pressure_pa);
                w.f32(b.temp_c);
                w.u8(b.baro_id);
                w.zeros(3);
            }
            Self::Mag(m) => {
                w.u64(m.t_ns);
                m.field_ut.iter().for_each(|v| w.f32(*v));
                w.u8(m.mag_id);
                w.zeros(3);
            }
            Self::Trigger(t) => {
                w.u32(t.seq);
                w.u8(t.cam_mask);
                w.u8(t.flags);
                w.zeros(2);
                w.u64(t.t_start_ns);
                w.u64(t.t_mid_ns);
                w.u32(t.exposure_ns);
            }
            Self::Pps(p) => {
                w.u64(p.t_ns);
                w.u8(p.locked);
                w.zeros(3);
                w.u32(p.pps_count);
                w.i32(p.phase_err_ns);
                w.i32(p.freq_corr_ppb);
            }
            Self::GnssRaw(g) => {
                w.u64(g.t_ns);
                w.u8(g.port);
                w.zeros(1);
                w.bytes(g.bytes);
            }
            Self::RtcmIn(r) => {
                w.u8(r.port);
                w.zeros(1);
                w.bytes(r.bytes);
            }
            Self::Config(c) => {
                w.u16(c.trigger_hz);
                w.u32(c.exposure_us);
                w.u16(c.imu_odr_hz);
                w.u8(c.cam_mask);
                w.u8(c.trigger_mode);
                w.u8(c.pps_out);
                w.u8(c.gprmc_out);
                w.u8(c.imu_batch);
                w.i8(c.heater_target_c);
                w.zeros(2);
            }
            Self::Health(h) => {
                w.u64(h.t_ns);
                w.u32(h.uptime_s);
                w.i16(h.mcu_temp_c);
                w.i16(h.imu_temp_c);
                w.u16(h.vin_mv);
                w.zeros(2);
                w.u32(h.usb_tx_drops);
                w.u32(h.imu_fifo_overflows);
                w.u32(h.rx_crc_errors);
                w.u32(h.trigger_missed);
                w.u8(h.pps_state);
                w.u8(h.heater_duty_pct);
                w.zeros(2);
            }
            Self::Unknown { payload, .. } => w.bytes(payload),
        }
        debug_assert_eq!(w.pos(), out.len());
    }

    /// Parse a payload of type `ty`.
    pub fn parse(ty: u8, p: &'a [u8]) -> Result<Self, Error> {
        let fixed = |n: usize| {
            if p.len() == n {
                Ok(())
            } else {
                Err(Error::PayloadSize { ty })
            }
        };
        let mut r = Reader::new(p);
        Ok(match ty {
            ty::HELLO => {
                fixed(HELLO_LEN)?;
                let proto_ver = r.u16();
                let (fw_major, fw_minor, fw_patch) = (r.u8(), r.u8(), r.u8());
                r.skip(1);
                Self::Hello(Hello {
                    proto_ver,
                    fw_major,
                    fw_minor,
                    fw_patch,
                    board_id: r.u16(),
                    caps: r.u32(),
                    uid: r.take(),
                    board_name: r.take(),
                })
            }
            ty::IMU_BATCH => {
                if p.len() < IMU_HEADER_LEN {
                    return Err(Error::PayloadSize { ty });
                }
                let t0_ns = r.u64();
                let dt_ns = r.u32();
                let n = usize::from(r.u8());
                if n == 0 || n > IMU_MAX_SAMPLES {
                    return Err(Error::ImuCount);
                }
                fixed(IMU_HEADER_LEN + IMU_SAMPLE_LEN * n)?;
                let (imu_id, flags) = (r.u8(), r.u8());
                r.skip(1);
                let seq0 = r.u32();
                Self::ImuBatch(ImuBatch {
                    t0_ns,
                    dt_ns,
                    imu_id,
                    flags,
                    seq0,
                    samples: ImuSamples::Raw(r.rest()),
                })
            }
            ty::BARO => {
                fixed(BARO_LEN)?;
                Self::Baro(Baro {
                    t_ns: r.u64(),
                    pressure_pa: r.f32(),
                    temp_c: r.f32(),
                    baro_id: r.u8(),
                })
            }
            ty::MAG => {
                fixed(MAG_LEN)?;
                Self::Mag(Mag {
                    t_ns: r.u64(),
                    field_ut: [r.f32(), r.f32(), r.f32()],
                    mag_id: r.u8(),
                })
            }
            ty::TRIGGER => {
                fixed(TRIGGER_LEN)?;
                let (seq, cam_mask, flags) = (r.u32(), r.u8(), r.u8());
                r.skip(2);
                Self::Trigger(Trigger {
                    seq,
                    cam_mask,
                    flags,
                    t_start_ns: r.u64(),
                    t_mid_ns: r.u64(),
                    exposure_ns: r.u32(),
                })
            }
            ty::PPS => {
                fixed(PPS_LEN)?;
                let (t_ns, locked) = (r.u64(), r.u8());
                r.skip(3);
                Self::Pps(Pps {
                    t_ns,
                    locked,
                    pps_count: r.u32(),
                    phase_err_ns: r.i32(),
                    freq_corr_ppb: r.i32(),
                })
            }
            ty::GNSS_RAW => {
                if p.len() < GNSS_HEADER_LEN || p.len() > GNSS_HEADER_LEN + GNSS_MAX_BYTES {
                    return Err(Error::PayloadSize { ty });
                }
                let (t_ns, port) = (r.u64(), r.u8());
                r.skip(1);
                Self::GnssRaw(GnssRaw {
                    t_ns,
                    port,
                    bytes: r.rest(),
                })
            }
            ty::RTCM_IN => {
                if p.len() < RTCM_HEADER_LEN {
                    return Err(Error::PayloadSize { ty });
                }
                let port = r.u8();
                r.skip(1);
                Self::RtcmIn(RtcmIn {
                    port,
                    bytes: r.rest(),
                })
            }
            ty::CONFIG => {
                fixed(CONFIG_LEN)?;
                Self::Config(Config {
                    trigger_hz: r.u16(),
                    exposure_us: r.u32(),
                    imu_odr_hz: r.u16(),
                    cam_mask: r.u8(),
                    trigger_mode: r.u8(),
                    pps_out: r.u8(),
                    gprmc_out: r.u8(),
                    imu_batch: r.u8(),
                    heater_target_c: r.i8(),
                })
            }
            ty::HEALTH => {
                fixed(HEALTH_LEN)?;
                let (t_ns, uptime_s, mcu_temp_c, imu_temp_c, vin_mv) =
                    (r.u64(), r.u32(), r.i16(), r.i16(), r.u16());
                r.skip(2);
                Self::Health(Health {
                    t_ns,
                    uptime_s,
                    mcu_temp_c,
                    imu_temp_c,
                    vin_mv,
                    usb_tx_drops: r.u32(),
                    imu_fifo_overflows: r.u32(),
                    rx_crc_errors: r.u32(),
                    trigger_missed: r.u32(),
                    pps_state: r.u8(),
                    heater_duty_pct: r.u8(),
                })
            }
            _ => Self::Unknown { ty, payload: p },
        })
    }
}
