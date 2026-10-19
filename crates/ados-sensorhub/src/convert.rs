//! Hub packets → sensor-bus frames.
//!
//! Every hub stamp is mapped onto the host monotonic clock through
//! [`HubClock`]; IMU and magnetometer vectors are scaled to SI units and
//! rotated from the chip frame into body FLU with the rig extrinsics.

use std::collections::{HashMap, HashSet};

use ados_sensorbus::{v1, Baro, BusFrame, Kind, Mag, Origin};
use ados_sensorhub_proto::{self as proto, Packet};

use crate::rig::{rotate, Extrinsics, Mat3, IDENTITY};
use crate::state::{HelloInfo, HubState};
use crate::timemap::HubClock;

/// Stream id of the hub's `sync` frames.
pub const SYNC_SRC: &str = "hub";

pub struct Converter {
    clock: HubClock,
    ext: Extrinsics,
    warned: HashSet<String>,
    gnss_forward: bool,
    imu_next: HashMap<u8, u32>,
    trigger_next: Option<u32>,
    /// Hub UART ports in first-seen order; index i publishes as `gnss<i>`.
    gnss_ports: Vec<u8>,
    /// Set when a `Hello` reports a protocol version this build cannot read.
    pub unsupported_proto: Option<u16>,
}

impl Converter {
    pub fn new(ext: Extrinsics, gnss_forward: bool) -> Self {
        Self {
            clock: HubClock::new(),
            ext,
            warned: HashSet::new(),
            gnss_forward,
            imu_next: HashMap::new(),
            trigger_next: None,
            gnss_ports: Vec::new(),
            unsupported_proto: None,
        }
    }

    pub fn clock(&self) -> &HubClock {
        &self.clock
    }

    /// The hub UART RTCM corrections go to: the first GNSS port seen, else 0.
    pub fn rtcm_port(&self) -> u8 {
        self.gnss_ports.first().copied().unwrap_or(0)
    }

    /// Convert one packet that arrived at host time `arrival_ns`, folding link
    /// facts into `st` and emitting bus frames. Once the hub has announced a
    /// protocol version this build cannot read, nothing more is interpreted.
    pub fn handle(
        &mut self,
        pkt: &Packet<'_>,
        arrival_ns: u64,
        st: &mut HubState,
        emit: &mut dyn FnMut(BusFrame),
    ) {
        if self.unsupported_proto.is_some() {
            return;
        }
        if let Some(hub_ns) = newest_stamp(pkt) {
            let obs = self.clock.observe(arrival_ns, hub_ns);
            if obs.reset {
                tracing::info!("hub timebase restarted; clock fit reset");
                self.imu_next.clear();
                self.trigger_next = None;
            }
            if let Some(sync) = obs.sync {
                if let Ok(f) = BusFrame::msgpack(Kind::Sync, SYNC_SRC, Origin::Hub, &sync) {
                    emit(f);
                }
            }
        }
        match *pkt {
            Packet::Hello(h) => self.hello(&h, st),
            Packet::ImuBatch(b) => self.imu(&b, arrival_ns, st, emit),
            Packet::Trigger(t) => {
                st.dropped += seq_gap(self.trigger_next, t.seq);
                self.trigger_next = Some(t.seq.wrapping_add(1));
                st.triggers += 1;
                let t_mid_ns = self.clock.host_time(t.t_mid_ns, arrival_ns, t.t_mid_ns);
                for cam in (0..8u8).filter(|i| t.cam_mask & (1 << i) != 0) {
                    let rec = v1::TriggerRecord {
                        seq: u64::from(t.seq),
                        t_mid_ns,
                        exposure_ns: t.exposure_ns,
                        gain: 0.0,
                    };
                    emit(BusFrame::proto(
                        Kind::Trigger,
                        format!("cam{cam}"),
                        Origin::Hub,
                        &rec,
                    ));
                }
            }
            Packet::Baro(b) => {
                let body = Baro {
                    t_ns: self.clock.host_time(b.t_ns, arrival_ns, b.t_ns),
                    pressure_pa: f64::from(b.pressure_pa),
                    temp_c: b.temp_c,
                };
                let src = format!("baro{}", b.baro_id);
                if let Ok(f) = BusFrame::msgpack(Kind::Baro, src, Origin::Hub, &body) {
                    emit(f);
                }
            }
            Packet::Mag(m) => {
                let src = format!("mag{}", m.mag_id);
                let r = self.rotation(&src);
                let v = rotate(&r, m.field_ut.map(f64::from));
                let body = Mag {
                    t_ns: self.clock.host_time(m.t_ns, arrival_ns, m.t_ns),
                    field_ut: v.map(|x| x as f32),
                };
                if let Ok(f) = BusFrame::msgpack(Kind::Mag, src, Origin::Hub, &body) {
                    emit(f);
                }
            }
            Packet::Pps(p) => self.pps_state(p.locked, st),
            Packet::GnssRaw(g) => {
                let index = match self.gnss_ports.iter().position(|p| *p == g.port) {
                    Some(i) => i,
                    None => {
                        self.gnss_ports.push(g.port);
                        self.gnss_ports.len() - 1
                    }
                };
                if self.gnss_forward {
                    let msg = v1::GnssRaw {
                        t_ns: self.clock.host_time(g.t_ns, arrival_ns, g.t_ns),
                        bytes: g.bytes.to_vec(),
                    };
                    emit(BusFrame::proto(
                        Kind::Gnss,
                        format!("gnss{index}"),
                        Origin::Hub,
                        &msg,
                    ));
                }
            }
            Packet::Health(h) => {
                st.uptime_s = Some(h.uptime_s);
                self.pps_state(h.pps_state, st);
            }
            // Host → hub packets echoed back, and unknown types, carry
            // nothing to publish.
            Packet::RtcmIn(_) | Packet::Config(_) | Packet::Unknown { .. } => {}
        }
    }

    fn hello(&mut self, h: &proto::Hello, st: &mut HubState) {
        let info = HelloInfo {
            fw: format!(
                "ados-sensorhub {}.{}.{}",
                h.fw_major, h.fw_minor, h.fw_patch
            ),
            board: board_name(h),
            proto_ver: h.proto_ver,
            caps: h.caps,
        };
        if st.hello.as_ref() != Some(&info) {
            tracing::info!(fw = %info.fw, board = %info.board, proto_ver = info.proto_ver,
                caps = info.caps, "sensor hub hello");
        }
        if h.proto_ver != proto::PROTO_VER {
            self.unsupported_proto = Some(h.proto_ver);
        }
        st.hello = Some(info);
    }

    fn imu(
        &mut self,
        b: &proto::ImuBatch<'_>,
        arrival_ns: u64,
        st: &mut HubState,
        emit: &mut dyn FnMut(BusFrame),
    ) {
        let n = b.samples.len() as u32;
        if n == 0 {
            return;
        }
        let gap = seq_gap(self.imu_next.get(&b.imu_id).copied(), b.seq0);
        st.dropped += gap.div_ceil(u64::from(n));
        self.imu_next.insert(b.imu_id, b.seq0.wrapping_add(n));
        st.imu_samples += u64::from(n);

        let src = format!("imu{}", b.imu_id);
        let r = self.rotation(&src);
        let newest = b.sample_t_ns(n - 1);
        let samples = b
            .samples
            .iter()
            .zip(0u32..)
            .map(|(s, k)| v1::ImuSample {
                t_ns: self.clock.host_time(b.sample_t_ns(k), arrival_ns, newest),
                seq: u64::from(b.seq0.wrapping_add(k)),
                gyro: rotate(&r, s.gyro.map(|v| f64::from(v) * proto::GYRO_LSB_RAD_S)).to_vec(),
                accel: rotate(&r, s.accel.map(|v| f64::from(v) * proto::ACCEL_LSB_M_S2)).to_vec(),
                temp_c: Some(f32::from(s.temp) / 100.0),
            })
            .collect();
        emit(BusFrame::proto(
            Kind::Imu,
            src,
            Origin::Hub,
            &v1::ImuBatch { samples },
        ));
    }

    fn pps_state(&mut self, state: u8, st: &mut HubState) {
        if st.pps_state != Some(state) {
            tracing::info!(
                pps_state = state,
                locked = state == proto::pps_state::LOCKED,
                "hub pps state"
            );
            st.pps_state = Some(state);
        }
    }

    /// The chip → body rotation of rig sensor `id`; identity (reported once)
    /// when the rig does not carry it.
    fn rotation(&mut self, id: &str) -> Mat3 {
        match self.ext.rotation(id) {
            Some(r) => *r,
            None => {
                if self.warned.insert(id.to_string()) {
                    tracing::warn!(
                        sensor = id,
                        "no rig extrinsics; publishing the chip frame unrotated"
                    );
                }
                IDENTITY
            }
        }
    }
}

/// The newest hub stamp a packet carries.
fn newest_stamp(pkt: &Packet<'_>) -> Option<u64> {
    match *pkt {
        Packet::ImuBatch(b) if !b.samples.is_empty() => {
            Some(b.sample_t_ns(b.samples.len() as u32 - 1))
        }
        Packet::Baro(b) => Some(b.t_ns),
        Packet::Mag(m) => Some(m.t_ns),
        Packet::Trigger(t) => Some(t.t_mid_ns),
        Packet::Pps(p) => Some(p.t_ns),
        Packet::GnssRaw(g) => Some(g.t_ns),
        Packet::Health(h) => Some(h.t_ns),
        _ => None,
    }
}

/// Sequence numbers skipped between the expected and the received one. A
/// backwards step (reorder or restart) counts nothing.
fn seq_gap(expected: Option<u32>, got: u32) -> u64 {
    expected.map_or(0, |e| {
        let d = got.wrapping_sub(e);
        if d < 1 << 31 {
            u64::from(d)
        } else {
            0
        }
    })
}

fn board_name(h: &proto::Hello) -> String {
    let end = h
        .board_name
        .iter()
        .position(|b| *b == 0)
        .unwrap_or(h.board_name.len());
    let name = String::from_utf8_lossy(&h.board_name[..end])
        .trim()
        .to_string();
    if !name.is_empty() {
        return name;
    }
    match h.board_id {
        proto::BOARD_ID_SIM => "sim".to_string(),
        proto::BOARD_ID_MATEK_H743_SLIM_V3 => "matek_h743_slim_v3".to_string(),
        id => format!("board-{id}"),
    }
}

#[cfg(test)]
#[path = "convert_tests.rs"]
mod tests;
