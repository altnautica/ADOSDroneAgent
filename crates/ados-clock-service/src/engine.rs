//! The clock's consumer of the broker tap: sync pairs feed the clock fits,
//! data frames feed the stream table, and a locked GNSS PPS source feeds
//! chrony.

use std::path::Path;

use ados_clock::chrony::{self, ChronySink};
use ados_sensorbus::{BusFrame, Kind, SyncObs, SyncSource};
use serde::Serialize;

use crate::clock::{ClockState, SyncOutcome};
use crate::fit::LatencyFloor;
use crate::streams::StreamTable;

/// Everything the clock tracks.
#[derive(Debug)]
pub struct Engine {
    pub clock: ClockState,
    pub streams: StreamTable,
    chrony: Option<ChronySink>,
}

impl Engine {
    /// `chrony`: where to send PPS samples, or `None` to never send.
    /// `sim_floor`: the latency floor under the video SEI pairs.
    pub fn new(chrony: Option<ChronySink>, sim_floor: LatencyFloor) -> Self {
        Engine {
            clock: ClockState::with_sim_floor(sim_floor),
            streams: StreamTable::new(),
            chrony,
        }
    }

    /// Account one tapped frame that arrived at host `rx_ns`.
    pub fn ingest(&mut self, frame: &BusFrame, rx_ns: u64) {
        match frame.kind {
            Kind::Sync => match frame.decode_msgpack::<SyncObs>() {
                Ok(obs) => self.on_sync(&obs, rx_ns),
                Err(e) => tracing::debug!(error = %e, src = %frame.src, "bad sync frame"),
            },
            _ => {
                self.streams.ingest(frame, rx_ns);
            }
        }
    }

    fn on_sync(&mut self, obs: &SyncObs, rx_ns: u64) {
        let outcome = self.clock.on_sync(obs);
        if outcome == SyncOutcome::Outlier
            || obs.source != SyncSource::GnssPps
            || !self.clock.locked(SyncSource::GnssPps, rx_ns)
        {
            return;
        }
        let Some(sink) = self.chrony.as_mut() else {
            return;
        };
        let delta = ados_sensorbus::time::realtime_ns() - ados_sensorbus::time::mono_ns() as i64;
        sink.send(&chrony::pulse_sample(obs.host_ns, obs.source_ns, delta));
    }

    /// PPS samples delivered to chrony so far.
    pub fn chrony_sent(&self) -> u64 {
        self.chrony.as_ref().map_or(0, ChronySink::sent)
    }
}

/// Write `doc` as JSON to `path` atomically (tmp + rename), mode 0644.
pub fn write_sidecar<T: Serialize>(path: &Path, doc: &T) -> std::io::Result<()> {
    ados_protocol::sidecar::write_json_atomic(path, doc, 0o644)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_sensorbus::Origin;

    const S: u64 = 1_000_000_000;

    fn sync(source: SyncSource, host_ns: u64, source_ns: i64) -> BusFrame {
        let obs = SyncObs {
            source,
            host_ns,
            source_ns,
            rtt_ns: 0,
            sim_time: false,
        };
        BusFrame::msgpack(Kind::Sync, source.id(), Origin::Gnss, &obs).unwrap()
    }

    #[test]
    fn a_locked_pps_feeds_chrony_once_per_pair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chrony.sock");
        let rx = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        rx.set_nonblocking(true).unwrap();
        let mut e = Engine::new(Some(ChronySink::new(&path)), LatencyFloor::Unknown);
        let utc0 = 1_760_860_000 * S as i64;
        for i in 0..7u64 {
            let h = 50 * S + i * S;
            e.ingest(&sync(SyncSource::GnssPps, h, utc0 + (i * S) as i64), h);
        }
        // Pairs 5, 6 and 7 found the source locked.
        assert_eq!(e.chrony_sent(), 3);
        let mut buf = [0u8; 64];
        assert_eq!(rx.recv(&mut buf).unwrap(), chrony::SAMPLE_LEN);
    }

    #[test]
    fn non_pps_pairs_never_reach_chrony() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chrony.sock");
        let _rx = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
        let mut e = Engine::new(Some(ChronySink::new(&path)), LatencyFloor::Unknown);
        for i in 0..10u64 {
            let h = 50 * S + i * S / 10;
            e.ingest(&sync(SyncSource::Hub, h, h as i64), h);
        }
        assert!(e.clock.locked(SyncSource::Hub, 51 * S));
        assert_eq!(e.chrony_sent(), 0);
    }

    #[test]
    fn sidecars_are_written_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clock.json");
        let e = Engine::new(None, LatencyFloor::Unknown);
        write_sidecar(&path, &e.clock.report(5)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["timebase"], "host_monotonic");
        assert_eq!(v["t_ns"], 5);
    }
}
