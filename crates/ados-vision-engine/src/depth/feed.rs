//! The ranging the depth fit and the landing-zone search use: the planar
//! LiDAR scan, the downward rangefinder and the vehicle attitude.
//!
//! - The scan is the world-engine extension's `plugin.world-engine.scan`
//!   topic (body FRD, one nearest horizontal range per bin, clockwise from the
//!   nose), read off the plugin host's event export socket. Only events the
//!   host stamped as published by that extension are taken.
//! - The rangefinder is the flight controller's `DISTANCE_SENSOR` facing down
//!   (`MAV_SENSOR_ROTATION_PITCH_270`), and the attitude its `ATTITUDE`, both
//!   read off the MAVLink broadcast socket.
//!
//! The scan and the rangefinder keep only the latest value with its arrival
//! time on the engine's monotonic clock; [`RangeFeed::snapshot`] hands out
//! what is fresh. The attitude keeps a few seconds of samples on the flight
//! controller's own clock (`time_boot_ms`), so the tracker can ask how the
//! vehicle was turned when a given frame was exposed
//! ([`RangeFeed::attitude_at`]). A missing socket is normal (no plugin host,
//! no flight controller yet): the reader retries quietly, the depth layer
//! runs unscaled and the tracker does without the camera's turn.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ados_protocol::framebus::mono_ns;
use ados_protocol::ipc::read_newline_line;
use ados_protocol::mavlink::ardupilotmega::MavSensorOrientation;
use ados_protocol::mavlink::{parse_any, MavMessage};
use serde::Deserialize;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// The topic the planar scan arrives on, and the only publisher trusted for it.
pub const SCAN_TOPIC: &str = "plugin.world-engine.scan";
pub const SCAN_PUBLISHER: &str = "com.altnautica.world-engine";

/// A value older than this is not used, milliseconds.
pub const MAX_AGE_MS: u64 = 500;

/// How far back the attitude history reaches, on the flight controller's
/// clock, ms. A detector batch is a few hundred ms behind its frame.
pub const ATTITUDE_HISTORY_MS: u64 = 4_000;

/// The longest gap between two attitude samples interpolated across, ms
/// (the stream runs at 10 Hz or faster); and the longest a frame may be
/// newer than the newest sample and still take it, ms.
const ATTITUDE_MAX_GAP_MS: f64 = 250.0;
const ATTITUDE_MAX_HOLD_MS: f64 = 100.0;

/// `MAV_COMP_ID_AUTOPILOT1`.
const AUTOPILOT_COMPONENT: u8 = 1;

/// Longest export line taken (a 360-bin scan is a few KiB of JSON).
const MAX_LINE: usize = 512 * 1024;

/// Pause between reconnect attempts.
const RETRY: Duration = Duration::from_secs(2);

/// One planar scan in the body frame.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Scan {
    /// Bin 0's azimuth, radians, clockwise from the nose.
    pub angle_start_rad: f32,
    pub angle_increment_rad: f32,
    /// Nearest horizontal range per bin, metres; `0` is no return.
    pub ranges_m: Vec<f32>,
}

/// The fresh ranging at one instant.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ranging {
    pub scan: Option<Scan>,
    /// Downward rangefinder distance, metres.
    pub down_m: Option<f32>,
    /// Roll and pitch, radians.
    pub attitude: Option<(f32, f32)>,
}

/// The vehicle's attitude, radians: MAVLink `ATTITUDE`, the body frame (FRD)
/// in the local NED frame, yaw then pitch then roll.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Attitude {
    pub roll: f32,
    pub pitch: f32,
    pub yaw: f32,
}

/// When a frame was exposed, on one of the two clocks the history can read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameTime {
    /// The engine's monotonic clock, ms. The history maps it onto the flight
    /// controller's clock with the smallest arrival delay it has seen.
    Host(f64),
    /// The flight controller's own clock (`time_boot_ms`), ms: a simulated
    /// frame stamped with simulation time, when the flight controller runs
    /// on that clock too.
    FlightController(f64),
}

#[derive(Debug, Clone, Copy)]
struct AttitudeSample {
    /// `time_boot_ms`.
    fc_ms: u64,
    /// Arrival on the engine's monotonic clock.
    rx_ms: u64,
    attitude: Attitude,
}

#[derive(Debug, Default)]
struct Latest {
    scan: Option<(Arc<Scan>, u64)>,
    down: Option<(f32, u64)>,
    attitudes: VecDeque<AttitudeSample>,
}

/// The latest ranging, shared by the readers and the depth runs.
#[derive(Debug, Default)]
pub struct RangeFeed {
    latest: Mutex<Latest>,
}

fn now_ms() -> u64 {
    mono_ns() / 1_000_000
}

impl RangeFeed {
    fn with<T>(&self, f: impl FnOnce(&mut Latest) -> T) -> T {
        f(&mut self.latest.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn set_scan(&self, scan: Scan, at_ms: u64) {
        self.with(|l| l.scan = Some((Arc::new(scan), at_ms)));
    }

    pub fn set_down(&self, metres: f32, at_ms: u64) {
        self.with(|l| l.down = Some((metres, at_ms)));
    }

    /// One `ATTITUDE` sample: the flight controller's `time_boot_ms`, and its
    /// arrival on the engine's clock. A repeated sample is skipped; one older
    /// than the newest means the flight controller restarted, and the
    /// history starts over.
    pub fn push_attitude(&self, fc_ms: u64, attitude: Attitude, rx_ms: u64) {
        self.with(|l| {
            match l.attitudes.back() {
                Some(last) if last.fc_ms == fc_ms => return,
                Some(last) if last.fc_ms > fc_ms => l.attitudes.clear(),
                _ => {}
            }
            l.attitudes.push_back(AttitudeSample {
                fc_ms,
                rx_ms,
                attitude,
            });
            while l
                .attitudes
                .front()
                .is_some_and(|s| s.fc_ms + ATTITUDE_HISTORY_MS < fc_ms)
            {
                l.attitudes.pop_front();
            }
        });
    }

    /// The attitude when a frame was exposed at `at`, interpolated between
    /// the two samples around it. `None` when the history does not cover
    /// that instant: older than its first sample, newer than its last by
    /// more than [`ATTITUDE_MAX_HOLD_MS`], or inside a gap of more than
    /// [`ATTITUDE_MAX_GAP_MS`].
    pub fn attitude_at(&self, at: FrameTime) -> Option<Attitude> {
        self.with(|l| {
            let s = &l.attitudes;
            let fc = match at {
                FrameTime::FlightController(ms) => ms,
                // The sample that arrived soonest after its stamp fixes the
                // offset between the clocks; later arrivals only queued.
                FrameTime::Host(ms) => {
                    let offset = s
                        .iter()
                        .map(|x| x.rx_ms as f64 - x.fc_ms as f64)
                        .min_by(f64::total_cmp)?;
                    ms - offset
                }
            };
            let first = s.front()?;
            let last = s.back()?;
            if fc < first.fc_ms as f64 || fc > last.fc_ms as f64 + ATTITUDE_MAX_HOLD_MS {
                return None;
            }
            if fc >= last.fc_ms as f64 {
                return Some(last.attitude);
            }
            let i = s.partition_point(|x| (x.fc_ms as f64) <= fc);
            let (a, b) = (s[i - 1], s[i]);
            let span = (b.fc_ms - a.fc_ms) as f64;
            if span > ATTITUDE_MAX_GAP_MS {
                return None;
            }
            let f = ((fc - a.fc_ms as f64) / span) as f32;
            let lerp = |x: f32, y: f32| x + f * (y - x);
            let dyaw = (b.attitude.yaw - a.attitude.yaw + std::f32::consts::PI)
                .rem_euclid(std::f32::consts::TAU)
                - std::f32::consts::PI;
            Some(Attitude {
                roll: lerp(a.attitude.roll, b.attitude.roll),
                pitch: lerp(a.attitude.pitch, b.attitude.pitch),
                yaw: a.attitude.yaw + f * dyaw,
            })
        })
    }

    /// What is at most [`MAX_AGE_MS`] old at `now_ms`.
    pub fn snapshot(&self, now_ms: u64) -> Ranging {
        let fresh = |at: u64| now_ms.saturating_sub(at) <= MAX_AGE_MS;
        self.with(|l| Ranging {
            scan: l
                .scan
                .as_ref()
                .filter(|(_, at)| fresh(*at))
                .map(|(s, _)| Scan::clone(s)),
            down_m: l.down.filter(|(_, at)| fresh(*at)).map(|(d, _)| d),
            attitude: l
                .attitudes
                .back()
                .filter(|a| fresh(a.rx_ms))
                .map(|a| (a.attitude.roll, a.attitude.pitch)),
        })
    }

    /// Apply one export event line. Returns whether it was a usable scan.
    pub fn apply_event_line(&self, line: &[u8], at_ms: u64) -> bool {
        #[derive(Deserialize)]
        struct Event {
            topic: Option<String>,
            publisher: Option<String>,
            payload: Option<serde_json::Value>,
        }
        let Ok(ev) = serde_json::from_slice::<Event>(line) else {
            return false;
        };
        if ev.topic.as_deref() != Some(SCAN_TOPIC)
            || ev.publisher.as_deref() != Some(SCAN_PUBLISHER)
        {
            return false;
        }
        let Some(payload) = ev.payload else {
            return false;
        };
        if payload.get("frame").and_then(|f| f.as_str()) != Some("body_frd") {
            return false;
        }
        match serde_json::from_value::<Scan>(payload) {
            Ok(scan) if !scan.ranges_m.is_empty() && scan.angle_increment_rad.is_finite() => {
                self.set_scan(scan, at_ms);
                true
            }
            _ => false,
        }
    }

    /// Apply one raw MAVLink frame. Returns whether it updated the feed. The
    /// attitude is taken from the autopilot component only: another
    /// component's `ATTITUDE` (a gimbal, a companion estimator) runs on its
    /// own clock and describes something else.
    pub fn apply_mavlink(&self, frame: &[u8], at_ms: u64) -> bool {
        let Ok((header, msg)) = parse_any(frame) else {
            return false;
        };
        match msg {
            MavMessage::ATTITUDE(a) if header.component_id == AUTOPILOT_COMPONENT => {
                let attitude = Attitude {
                    roll: a.roll,
                    pitch: a.pitch,
                    yaw: a.yaw,
                };
                if !(attitude.roll.is_finite()
                    && attitude.pitch.is_finite()
                    && attitude.yaw.is_finite())
                {
                    return false;
                }
                self.push_attitude(u64::from(a.time_boot_ms), attitude, at_ms);
                true
            }
            MavMessage::DISTANCE_SENSOR(d)
                if d.orientation == MavSensorOrientation::MAV_SENSOR_ROTATION_PITCH_270
                    && d.current_distance >= d.min_distance
                    && d.current_distance <= d.max_distance
                    && d.current_distance > 0 =>
            {
                self.set_down(f32::from(d.current_distance) / 100.0, at_ms);
                true
            }
            _ => false,
        }
    }
}

/// The export socket: `$ADOS_PLUGIN_HOST_DIR/_events.sock`, default
/// `/run/ados/plugin-host/_events.sock`.
pub fn default_events_socket() -> PathBuf {
    let dir = std::env::var("ADOS_PLUGIN_HOST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/run/ados/plugin-host"));
    dir.join(ados_protocol::paths::PLUGIN_EVENTS_SOCK)
}

async fn scan_session(path: &Path, feed: &RangeFeed) -> std::io::Result<()> {
    let stream = UnixStream::connect(path).await?;
    let (read, mut write) = stream.into_split();
    let mut req = serde_json::to_vec(&serde_json::json!({
        "v": 1, "op": "subscribe", "topics": [SCAN_TOPIC],
    }))
    .unwrap_or_default();
    req.push(b'\n');
    write.write_all(&req).await?;
    let mut reader = BufReader::new(read);
    let ack = read_newline_line(&mut reader, MAX_LINE).await?;
    let ok = ack
        .as_deref()
        .and_then(|l| serde_json::from_slice::<serde_json::Value>(l).ok())
        .and_then(|v| v.get("ok").and_then(|o| o.as_bool()))
        .unwrap_or(false);
    if !ok {
        return Err(std::io::Error::other(
            "event export refused the subscription",
        ));
    }
    tracing::info!(path = %path.display(), "depth_scan_feed_connected");
    while let Some(line) = read_newline_line(&mut reader, MAX_LINE).await? {
        feed.apply_event_line(&line, now_ms());
    }
    Ok(())
}

/// Keep the scan current from the export socket until `cancel`.
pub async fn run_scan_reader(
    path: PathBuf,
    feed: Arc<RangeFeed>,
    cancel: ados_protocol::shutdown::Shutdown,
) {
    loop {
        tokio::select! {
            r = scan_session(&path, &feed) => {
                if let Err(e) = r {
                    tracing::debug!(error = %e, path = %path.display(), "depth_scan_feed_unavailable");
                }
            }
            _ = cancel.wait() => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(RETRY) => {}
            _ = cancel.wait() => return,
        }
    }
}

async fn mavlink_session(path: &Path, feed: &RangeFeed) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(path).await?;
    tracing::info!(path = %path.display(), "depth_mavlink_feed_connected");
    while let Some(rec) = ados_protocol::mavlink_socket::read_frame(&mut stream).await? {
        feed.apply_mavlink(&rec.frame, rec.rx_mono_ns / 1_000_000);
    }
    Ok(())
}

/// Keep the attitude and the rangefinder current from the MAVLink socket
/// until `cancel`.
pub async fn run_mavlink_reader(
    path: PathBuf,
    feed: Arc<RangeFeed>,
    cancel: ados_protocol::shutdown::Shutdown,
) {
    loop {
        tokio::select! {
            r = mavlink_session(&path, &feed) => {
                if let Err(e) = r {
                    tracing::debug!(error = %e, path = %path.display(), "depth_mavlink_feed_unavailable");
                }
            }
            _ = cancel.wait() => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(RETRY) => {}
            _ = cancel.wait() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::mavlink::ardupilotmega::{
        MavDistanceSensor, ATTITUDE_DATA, DISTANCE_SENSOR_DATA,
    };
    use ados_protocol::mavlink::{serialize_v2, MavHeader};

    fn line(topic: &str, publisher: &str, frame: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "v": 1, "topic": topic, "publisher": publisher, "ts_ms": 5,
            "payload": {"t_ns": 1, "frame": frame, "angle_start_rad": 0.0,
                        "angle_increment_rad": 0.0174533, "ranges_m": [3.0, 0.0, 4.5]},
        }))
        .unwrap()
    }

    #[test]
    fn only_the_world_engine_body_frame_scan_is_taken() {
        let feed = RangeFeed::default();
        assert!(!feed.apply_event_line(&line(SCAN_TOPIC, "com.example.spoof", "body_frd"), 0));
        assert!(!feed.apply_event_line(&line(SCAN_TOPIC, SCAN_PUBLISHER, "map_ned"), 0));
        assert!(!feed.apply_event_line(&line("follow.state", SCAN_PUBLISHER, "body_frd"), 0));
        assert!(!feed.apply_event_line(b"{not json", 0));
        assert!(feed.apply_event_line(&line(SCAN_TOPIC, SCAN_PUBLISHER, "body_frd"), 1_000));
        let snap = feed.snapshot(1_400);
        assert_eq!(snap.scan.unwrap().ranges_m, vec![3.0, 0.0, 4.5]);
        // Stale after half a second.
        assert!(feed.snapshot(1_600).scan.is_none());
    }

    fn frame(msg: &MavMessage) -> Vec<u8> {
        serialize_v2(
            MavHeader {
                system_id: 1,
                component_id: 1,
                sequence: 0,
            },
            msg,
        )
        .unwrap()
    }

    #[test]
    fn the_downward_rangefinder_and_the_attitude_are_taken() {
        let feed = RangeFeed::default();
        let down = |orientation, cm| {
            MavMessage::DISTANCE_SENSOR(DISTANCE_SENSOR_DATA {
                min_distance: 20,
                max_distance: 1_200,
                current_distance: cm,
                mavtype: MavDistanceSensor::MAV_DISTANCE_SENSOR_LASER,
                orientation,
                ..DISTANCE_SENSOR_DATA::default()
            })
        };
        // A forward sensor, and a reading outside the sensor's span, are not.
        assert!(!feed.apply_mavlink(
            &frame(&down(MavSensorOrientation::MAV_SENSOR_ROTATION_NONE, 400)),
            0
        ));
        assert!(!feed.apply_mavlink(
            &frame(&down(
                MavSensorOrientation::MAV_SENSOR_ROTATION_PITCH_270,
                5_000
            )),
            0
        ));
        assert!(feed.apply_mavlink(
            &frame(&down(
                MavSensorOrientation::MAV_SENSOR_ROTATION_PITCH_270,
                412
            )),
            10
        ));
        assert!(feed.apply_mavlink(
            &frame(&MavMessage::ATTITUDE(ATTITUDE_DATA {
                roll: 0.1,
                pitch: -0.2,
                ..ATTITUDE_DATA::default()
            })),
            10
        ));
        let s = feed.snapshot(100);
        assert_eq!(s.down_m, Some(4.12));
        assert_eq!(s.attitude, Some((0.1, -0.2)));
        assert!(!feed.apply_mavlink(b"garbage", 0));
    }

    fn att(roll: f32, pitch: f32, yaw: f32) -> Attitude {
        Attitude { roll, pitch, yaw }
    }

    fn close(a: Option<Attitude>, b: Attitude) -> bool {
        a.is_some_and(|a| {
            (a.roll - b.roll).abs() < 1e-5
                && (a.pitch - b.pitch).abs() < 1e-5
                && (a.yaw - b.yaw).abs() < 1e-5
        })
    }

    #[test]
    fn the_attitude_history_interpolates_on_the_flight_controller_clock() {
        let feed = RangeFeed::default();
        assert_eq!(feed.attitude_at(FrameTime::FlightController(0.0)), None);
        // 20 Hz, arriving 30 ms after the stamp, one sample queued 40 ms more.
        feed.push_attitude(1_000, att(0.0, -0.1, 3.10), 5_030);
        feed.push_attitude(1_050, att(0.1, -0.2, -3.12), 5_120);
        feed.push_attitude(1_050, att(9.0, 9.0, 9.0), 5_121);
        feed.push_attitude(1_100, att(0.2, -0.2, -3.08), 5_130);
        // Between samples, yaw across the ±π seam the short way.
        let mid = feed
            .attitude_at(FrameTime::FlightController(1_025.0))
            .unwrap();
        assert!((mid.roll - 0.05).abs() < 1e-5 && (mid.pitch + 0.15).abs() < 1e-5);
        let seam = 3.10 + 0.5 * (std::f32::consts::TAU - 6.22);
        assert!((mid.yaw - seam).abs() < 1e-4, "{}", mid.yaw);
        // The engine's clock maps through the quickest arrival (30 ms).
        assert!(close(
            feed.attitude_at(FrameTime::Host(5_105.0)),
            att(0.15, -0.2, -3.10)
        ));
        // Held a little past the newest sample, never before the oldest.
        assert!(close(
            feed.attitude_at(FrameTime::FlightController(1_180.0)),
            att(0.2, -0.2, -3.08)
        ));
        assert_eq!(feed.attitude_at(FrameTime::FlightController(1_250.0)), None);
        assert_eq!(feed.attitude_at(FrameTime::FlightController(990.0)), None);
    }

    #[test]
    fn the_attitude_history_forgets_gaps_old_samples_and_a_restart() {
        let feed = RangeFeed::default();
        feed.push_attitude(1_000, att(0.0, 0.0, 0.0), 1_000);
        feed.push_attitude(1_400, att(0.4, 0.0, 0.0), 1_400);
        assert_eq!(feed.attitude_at(FrameTime::FlightController(1_200.0)), None);
        feed.push_attitude(1_000 + ATTITUDE_HISTORY_MS + 100, att(0.5, 0.0, 0.0), 6_000);
        assert_eq!(feed.attitude_at(FrameTime::FlightController(1_000.0)), None);
        assert!(feed
            .attitude_at(FrameTime::FlightController(5_100.0))
            .is_some());
        // The flight controller restarted: its clock went back.
        feed.push_attitude(20, att(0.7, 0.0, 0.0), 7_000);
        assert_eq!(feed.attitude_at(FrameTime::FlightController(1_400.0)), None);
        assert!(close(
            feed.attitude_at(FrameTime::FlightController(20.0)),
            att(0.7, 0.0, 0.0)
        ));
    }

    #[test]
    fn only_the_autopilot_attitude_is_taken() {
        let feed = RangeFeed::default();
        let gimbal = serialize_v2(
            MavHeader {
                system_id: 1,
                component_id: 154,
                sequence: 0,
            },
            &MavMessage::ATTITUDE(ATTITUDE_DATA {
                time_boot_ms: 10,
                pitch: -1.0,
                ..ATTITUDE_DATA::default()
            }),
        )
        .unwrap();
        assert!(!feed.apply_mavlink(&gimbal, 10));
        assert_eq!(feed.attitude_at(FrameTime::FlightController(10.0)), None);
        assert!(feed.apply_mavlink(
            &frame(&MavMessage::ATTITUDE(ATTITUDE_DATA {
                time_boot_ms: 10,
                yaw: 1.5,
                ..ATTITUDE_DATA::default()
            })),
            12
        ));
        assert!(close(
            feed.attitude_at(FrameTime::FlightController(10.0)),
            att(0.0, 0.0, 1.5)
        ));
    }
}
