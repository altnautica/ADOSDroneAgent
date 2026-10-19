//! The flight-controller sensor lane: turns the vehicle's sensor messages into
//! sensor-bus records on the host monotonic clock. Free of I/O; the task in
//! [`super::task`] feeds it frames and publishes what it emits.

use ados_protocol::mavlink::ardupilotmega::MavMessage;
use ados_sensorbus::v1::{ImuBatch, ImuSample, TriggerRecord};
use ados_sensorbus::{Baro, SyncObs};

use super::convert::{self, Unwrapper};
use super::timesync::{Reply, TimesyncTracker};

/// Bus stream ids this lane publishes.
pub const IMU_SRC: &str = "fc_imu";
pub const RAW_IMU_SRC: &str = "fc_raw_imu";
pub const BARO_SRC: &str = "fc_baro";
pub const TRIGGER_SRC: &str = "cam0";

/// IMU samples are published in batches spanning about this long.
pub const IMU_BATCH_NS: u64 = 10_000_000;

/// A sample cannot be taken after the router received it; a converted time
/// later than receipt by more than this means the clock relation is stale.
const FUTURE_SLACK_NS: u64 = 5_000_000;
/// Nor does a frame spend this long on the wire; an older converted time also
/// means a stale relation (typically an FC reboot not yet seen by TIMESYNC).
const MAX_LATENCY_NS: u64 = 1_000_000_000;

/// One record for the bus.
#[derive(Debug, Clone, PartialEq)]
pub enum BusOut {
    Imu { src: &'static str, batch: ImuBatch },
    Baro(Baro),
    Trigger(TriggerRecord),
    Sync(SyncObs),
}

#[derive(Debug)]
pub struct LaneCore {
    timesync: TimesyncTracker,
    epoch: u64,
    batch: Vec<ImuSample>,
    batch_src: &'static str,
    batch_start_rx: u64,
    imu_seq: u64,
    raw_seq: u64,
    img_idx: Unwrapper,
    baro_ms: Unwrapper,
    /// Latest `SYSTEM_TIME` as `(time_unix_usec, time_boot_ms)`.
    system_time: Option<(u64, u32)>,
}

impl Default for LaneCore {
    fn default() -> Self {
        Self {
            timesync: TimesyncTracker::default(),
            epoch: 0,
            batch: Vec::new(),
            batch_src: IMU_SRC,
            batch_start_rx: 0,
            imu_seq: 0,
            raw_seq: 0,
            img_idx: Unwrapper::new(16),
            baro_ms: Unwrapper::new(32),
            system_time: None,
        }
    }
}

impl LaneCore {
    /// Record a TIMESYNC request sent at host time `ts1_ns`.
    pub fn timesync_sent(&mut self, ts1_ns: u64) {
        self.timesync.sent(ts1_ns);
    }

    /// Process one vehicle message received at host time `rx_ns`.
    pub fn on_message(&mut self, rx_ns: u64, msg: &MavMessage, out: &mut Vec<BusOut>) {
        match msg {
            MavMessage::HIGHRES_IMU(m) => {
                let t = self.host_time(fc_ns_from_us(m.time_usec), rx_ns);
                self.push_imu(IMU_SRC, convert::highres_sample(m), t, rx_ns, out);
            }
            MavMessage::RAW_IMU(m) => {
                let t = self.host_time(fc_ns_from_us(m.time_usec), rx_ns);
                self.push_imu(RAW_IMU_SRC, convert::raw_imu_sample(m), t, rx_ns, out);
            }
            MavMessage::SCALED_PRESSURE(m) => {
                let boot_ms = self.baro_ms.unwrap(m.time_boot_ms as u64);
                let t = self.host_time(boot_ms as i64 * 1_000_000, rx_ns);
                out.push(BusOut::Baro(convert::baro(m, t)));
            }
            MavMessage::SYSTEM_TIME(m) => {
                self.system_time = Some((m.time_unix_usec, m.time_boot_ms));
            }
            MavMessage::CAMERA_FEEDBACK(m) => {
                let seq = self.img_idx.unwrap(m.img_idx as u64);
                let t = match convert::feedback_boot_us(m, self.system_time) {
                    Some(us) => self.host_time(fc_ns_from_us(us), rx_ns),
                    None => rx_ns,
                };
                out.push(BusOut::Trigger(convert::trigger(seq, t)));
            }
            MavMessage::TIMESYNC(m) => {
                if let Reply::Accepted(obs) = self.timesync.on_timesync(m.tc1, m.ts1, rx_ns) {
                    out.push(BusOut::Sync(obs));
                }
                if self.timesync.epoch() != self.epoch {
                    // The FC rebooted: its counters restarted with its clock.
                    self.epoch = self.timesync.epoch();
                    self.img_idx.reset();
                    self.baro_ms.reset();
                }
            }
            _ => {}
        }
    }

    /// Publish the pending IMU batch once it spans [`IMU_BATCH_NS`] at host
    /// time `now_ns`.
    pub fn flush_due(&mut self, now_ns: u64, out: &mut Vec<BusOut>) {
        if !self.batch.is_empty() && now_ns.saturating_sub(self.batch_start_rx) >= IMU_BATCH_NS {
            self.flush(out);
        }
    }

    /// Host time for FC boot time `fc_ns`: the TIMESYNC relation when one is
    /// fitted and plausible against the receive time, else the receive time.
    fn host_time(&self, fc_ns: i64, rx_ns: u64) -> u64 {
        match self.timesync.to_host(fc_ns) {
            Some(t)
                if t <= rx_ns + FUTURE_SLACK_NS && rx_ns.saturating_sub(t) <= MAX_LATENCY_NS =>
            {
                t
            }
            _ => rx_ns,
        }
    }

    fn push_imu(
        &mut self,
        src: &'static str,
        mut sample: ImuSample,
        t_ns: u64,
        rx_ns: u64,
        out: &mut Vec<BusOut>,
    ) {
        if !self.batch.is_empty() && self.batch_src != src {
            self.flush(out);
        }
        let seq = if src == IMU_SRC {
            &mut self.imu_seq
        } else {
            &mut self.raw_seq
        };
        sample.seq = *seq;
        *seq += 1;
        sample.t_ns = t_ns;
        if self.batch.is_empty() {
            self.batch_src = src;
            self.batch_start_rx = rx_ns;
        }
        self.batch.push(sample);
        self.flush_due(rx_ns, out);
    }

    fn flush(&mut self, out: &mut Vec<BusOut>) {
        out.push(BusOut::Imu {
            src: self.batch_src,
            batch: ImuBatch {
                samples: std::mem::take(&mut self.batch),
            },
        });
    }
}

fn fc_ns_from_us(us: u64) -> i64 {
    (us as i64).saturating_mul(1000)
}
