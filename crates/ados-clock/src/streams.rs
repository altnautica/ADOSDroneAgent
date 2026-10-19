//! The per-stream table behind `/run/ados/sensors.json`: one row per
//! `(kind, src, origin)` data stream on the bus with its sample count, recent
//! rate, newest sample time and newest arrival time.

use std::collections::{BTreeMap, VecDeque};

use ados_sensorbus::{BusFrame, Kind};
use prost::Message;
use serde::{Deserialize, Serialize};

/// Window the rate is measured over.
pub const RATE_WINDOW_NS: u64 = 2_000_000_000;
/// Arrivals kept per stream; bounds memory for a frame-per-sample producer.
const MAX_ARRIVALS: usize = 8192;

// Timestamp-only views of the capture messages: prost skips the fields these
// do not declare, so a frame costs one pass over its bytes and no sample
// vectors.
#[derive(Clone, PartialEq, Message)]
struct ImuTimes {
    #[prost(message, repeated, tag = "1")]
    samples: Vec<SampleTime>,
}

#[derive(Clone, PartialEq, Message)]
struct SampleTime {
    #[prost(uint64, tag = "1")]
    t_ns: u64,
}

#[derive(Clone, PartialEq, Message)]
struct TriggerTime {
    #[prost(uint64, tag = "2")]
    t_mid_ns: u64,
}

/// The `t_ns` of a msgpack `baro` / `mag` body.
#[derive(Deserialize)]
struct MsgpackTime {
    t_ns: u64,
}

/// Samples in a data frame and the newest sample time (`None` when the frame
/// carries no sample), or `None` for a frame that is not a data stream or
/// does not decode.
fn sample_times(frame: &BusFrame) -> Option<(u64, Option<u64>)> {
    let p = frame.payload.as_slice();
    match frame.kind {
        Kind::Imu => {
            let b = ImuTimes::decode(p).ok()?;
            Some((
                b.samples.len() as u64,
                b.samples.iter().map(|s| s.t_ns).max(),
            ))
        }
        Kind::Gnss => Some((1, Some(SampleTime::decode(p).ok()?.t_ns))),
        Kind::Trigger => Some((1, Some(TriggerTime::decode(p).ok()?.t_mid_ns))),
        Kind::Baro | Kind::Mag => {
            let t: MsgpackTime = rmp_serde::from_slice(p).ok()?;
            Some((1, Some(t.t_ns)))
        }
        Kind::Clock | Kind::Rtcm | Kind::Sync | Kind::Subscribe => None,
    }
}

#[derive(Debug, Default, Clone)]
struct Stream {
    count: u64,
    last_t_ns: u64,
    last_rx_ns: u64,
    /// `(rx host ns, samples)` of the recent frames.
    arrivals: VecDeque<(u64, u64)>,
}

impl Stream {
    fn record(&mut self, rx_ns: u64, samples: u64, last_t: Option<u64>) {
        self.count += samples;
        if let Some(t) = last_t {
            self.last_t_ns = t;
        }
        self.last_rx_ns = rx_ns;
        self.arrivals.push_back((rx_ns, samples));
        while let Some(&(rx, _)) = self.arrivals.front() {
            if rx_ns.saturating_sub(rx) > RATE_WINDOW_NS || self.arrivals.len() > MAX_ARRIVALS {
                self.arrivals.pop_front();
            } else {
                break;
            }
        }
    }

    /// Samples per second over the frames that arrived in the window ending
    /// at `now_ns`: the samples after the first frame divided by the span
    /// from the first frame to the last. Zero with fewer than two frames.
    fn rate_hz(&self, now_ns: u64) -> f64 {
        let start = now_ns.saturating_sub(RATE_WINDOW_NS);
        let mut recent = self.arrivals.iter().filter(|(rx, _)| *rx >= start);
        let Some(&(first, _)) = recent.next() else {
            return 0.0;
        };
        let (last, samples) = recent.fold((first, 0u64), |(_, n), &(rx, s)| (rx, n + s));
        if last <= first {
            return 0.0;
        }
        samples as f64 * 1e9 / (last - first) as f64
    }
}

/// One `streams[]` row of the sensors document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamReport {
    pub kind: &'static str,
    pub src: String,
    pub origin: &'static str,
    pub rate_hz: f64,
    pub last_t_ns: u64,
    pub last_rx_ns: u64,
    pub count: u64,
}

/// The `/run/ados/sensors.json` document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SensorsReport {
    pub version: u16,
    pub updated_ns: u64,
    pub streams: Vec<StreamReport>,
}

/// Every data stream seen on the bus, ordered by `(kind, src, origin)`.
#[derive(Debug, Default, Clone)]
pub struct StreamTable {
    streams: BTreeMap<(&'static str, String, &'static str), Stream>,
}

impl StreamTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Account one frame that arrived at host `rx_ns`. Returns false for a
    /// frame that is not a data stream or whose payload does not decode.
    pub fn ingest(&mut self, frame: &BusFrame, rx_ns: u64) -> bool {
        let Some((samples, last_t)) = sample_times(frame) else {
            return false;
        };
        let key = (
            frame.kind.as_str(),
            frame.src.clone(),
            frame.origin.as_str(),
        );
        self.streams
            .entry(key)
            .or_default()
            .record(rx_ns, samples, last_t);
        true
    }

    pub fn len(&self) -> usize {
        self.streams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    /// The sensors document at host time `now_ns`.
    pub fn report(&self, now_ns: u64) -> SensorsReport {
        let streams = self
            .streams
            .iter()
            .map(|(&(kind, ref src, origin), s)| StreamReport {
                kind,
                src: src.clone(),
                origin,
                rate_hz: s.rate_hz(now_ns),
                last_t_ns: s.last_t_ns,
                last_rx_ns: s.last_rx_ns,
                count: s.count,
            })
            .collect();
        SensorsReport {
            version: 1,
            updated_ns: now_ns,
            streams,
        }
    }
}

#[cfg(test)]
#[path = "streams_tests.rs"]
mod tests;
