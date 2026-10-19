//! Exposure-trigger records from the sensor bus, joined to captured frames.
//!
//! A triggered camera's exposures are timed by the sensor hub or the flight
//! controller, which publish one `trigger` frame per exposure on
//! `sensors.sock` (`v1::TriggerRecord {seq, t_mid_ns, exposure_ns}`, `t_mid_ns`
//! on the host monotonic clock). The hub names the stream after its trigger
//! output (`cam<channel>`); the flight controller has one camera trigger and
//! names it `cam0`. [`TriggerFeed`] keeps the recent records of every stream;
//! a [`TriggerJoiner`] per camera matches each frame to the record of the
//! exposure that produced it, by sequence number.
//!
//! The frame and trigger counters are independent (a V4L2 buffer sequence
//! starts at stream-on, the hub's at its boot), so the joiner learns the offset
//! between them from the record nearest in time to a frame, then follows the
//! sequence and re-learns it whenever a match stops agreeing in time. A
//! simulated camera's frame sequence is the render sequence the simulated hub
//! stamps on its triggers, so that joiner runs with the offset fixed at 0.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_config::cameras::CameraTrigger;
use ados_sensorbus::v1::TriggerRecord;
use ados_sensorbus::{Kind, Origin, Subscriber};

/// Records kept per trigger stream: several seconds at capture rates.
pub const TRIGGER_HISTORY: usize = 256;

const RECONNECT: Duration = Duration::from_millis(500);

/// The bus stream a camera's triggers arrive on, or `None` when free-running.
pub fn trigger_stream(trigger: CameraTrigger, channel: u8) -> Option<(Origin, String)> {
    match trigger {
        CameraTrigger::None => None,
        CameraTrigger::Hub => Some((Origin::Hub, format!("cam{channel}"))),
        CameraTrigger::Fc => Some((Origin::Fc, "cam0".to_string())),
    }
}

type Streams = HashMap<(Origin, String), VecDeque<TriggerRecord>>;

/// The recent trigger records of every stream on the bus. Cheap to clone.
#[derive(Clone, Default)]
pub struct TriggerFeed {
    streams: Arc<Mutex<Streams>>,
}

impl TriggerFeed {
    /// Record one trigger frame's payload.
    pub fn ingest(&self, origin: Origin, src: &str, rec: TriggerRecord) {
        let mut streams = self.streams.lock().expect("trigger feed lock");
        let ring = streams.entry((origin, src.to_string())).or_default();
        if ring.len() == TRIGGER_HISTORY {
            ring.pop_front();
        }
        ring.push_back(rec);
    }

    /// Run `f` over the records of one stream (empty when none arrived yet).
    pub fn with_stream<R>(&self, stream: &(Origin, String), f: impl FnOnce(&VecDeque<TriggerRecord>) -> R) -> R {
        let streams = self.streams.lock().expect("trigger feed lock");
        match streams.get(stream) {
            Some(ring) => f(ring),
            None => f(&VecDeque::new()),
        }
    }

    /// Subscribe to the bus at `path` and keep feeding records until the task
    /// is dropped, reconnecting whenever the bus goes away.
    pub fn spawn_subscriber(&self, path: PathBuf) -> tokio::task::JoinHandle<()> {
        let feed = self.clone();
        tokio::spawn(async move {
            loop {
                let Ok(mut sub) = Subscriber::connect(&path, &[Kind::Trigger]).await else {
                    tokio::time::sleep(RECONNECT).await;
                    continue;
                };
                loop {
                    match sub.recv().await {
                        Ok(frame) if frame.kind == Kind::Trigger => {
                            if let Ok(rec) = frame.decode_proto::<TriggerRecord>() {
                                feed.ingest(frame.origin, &frame.src, rec);
                            }
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
                        Err(_) => break,
                    }
                }
                tokio::time::sleep(RECONNECT).await;
            }
        })
    }
}

/// Matches one camera's frames to their trigger records.
#[derive(Debug, Clone)]
pub struct TriggerJoiner {
    /// `trigger seq − frame seq`, once learned.
    offset: Option<i128>,
    /// Whether the offset is fixed by construction (never re-learned).
    fixed: bool,
    /// The largest `|t_mid − frame time|` a match may show; `None` trusts the
    /// sequence alone.
    tolerance_ns: Option<u64>,
}

impl TriggerJoiner {
    /// A joiner that learns the frame-to-trigger offset, accepting a match
    /// whose mid-exposure time lies within `tolerance_ns` of the frame's.
    pub fn learning(tolerance_ns: u64) -> Self {
        Self {
            offset: None,
            fixed: false,
            tolerance_ns: Some(tolerance_ns),
        }
    }

    /// A joiner whose frame sequence IS the trigger sequence (a simulated
    /// camera): matched by sequence only.
    pub fn same_sequence() -> Self {
        Self {
            offset: Some(0),
            fixed: true,
            tolerance_ns: None,
        }
    }

    fn agrees(&self, rec: &TriggerRecord, frame_ns: u64) -> bool {
        self.tolerance_ns
            .is_none_or(|tol| rec.t_mid_ns.abs_diff(frame_ns) <= tol)
    }

    /// The record of the exposure that produced frame `frame_seq`, captured
    /// around host time `frame_ns`, among `records` (oldest first).
    pub fn join(
        &mut self,
        records: &VecDeque<TriggerRecord>,
        frame_seq: u64,
        frame_ns: u64,
    ) -> Option<TriggerRecord> {
        if let Some(off) = self.offset {
            let want = i128::from(frame_seq) + off;
            if let Some(rec) = records.iter().rev().find(|r| i128::from(r.seq) == want) {
                if self.agrees(rec, frame_ns) {
                    return Some(*rec);
                }
            }
            if self.fixed {
                return None;
            }
            self.offset = None;
        }
        let tol = self.tolerance_ns?;
        let best = records
            .iter()
            .min_by_key(|r| r.t_mid_ns.abs_diff(frame_ns))?;
        if best.t_mid_ns.abs_diff(frame_ns) > tol {
            return None;
        }
        self.offset = Some(i128::from(best.seq) - i128::from(frame_seq));
        Some(*best)
    }
}

/// A camera's joiner bound to its stream in the shared feed.
#[derive(Clone)]
pub struct CameraTriggers {
    feed: TriggerFeed,
    stream: (Origin, String),
    joiner: TriggerJoiner,
}

impl CameraTriggers {
    pub fn new(feed: TriggerFeed, stream: (Origin, String), joiner: TriggerJoiner) -> Self {
        Self {
            feed,
            stream,
            joiner,
        }
    }

    /// Join frame `frame_seq` captured around `frame_ns` to its trigger record.
    pub fn join(&mut self, frame_seq: u64, frame_ns: u64) -> Option<TriggerRecord> {
        let joiner = &mut self.joiner;
        self.feed
            .with_stream(&self.stream, |records| joiner.join(records, frame_seq, frame_ns))
    }
}

#[cfg(test)]
#[path = "trigger_tests.rs"]
mod tests;
