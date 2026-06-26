//! Where the capture service gets the pose to tag each frame with.
//!
//! A [`PoseProvider`] hands the loop the latest known pose synchronously; the
//! work of reading a socket runs in a background task that updates a shared
//! cache. Three real providers plus a replay provider for SITL:
//!
//! - [`StateSockPose`] reads the flight controller's fused state from the state
//!   socket and converts it to a local-frame pose (on-board "local VIO").
//! - [`OffloadPose`] reads SLAM poses a compute node returns for an NPU-less
//!   board.
//! - [`HybridPose`] uses whichever of the two is fresher.
//! - [`ReplayPose`] walks a fixed pose list, for the SITL harness.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_protocol::atlas::{GlobalAnchor, OffloadedPose, Pose, PoseSource, VioHealth};
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed, read_newline_line};
use ados_protocol::state::STATE_V2_MAX_FRAME;
use tokio::task::JoinHandle;

use crate::runtime::{AtlasRuntimeConfig, PoseTier};

/// A pose plus the metadata the capture path needs to stamp a keyframe.
#[derive(Debug, Clone, PartialEq)]
pub struct PoseSample {
    pub pose: Pose,
    pub anchor: Option<GlobalAnchor>,
    pub source: PoseSource,
    pub ts_ms: i64,
    pub health: VioHealth,
}

/// Hands the capture loop the latest known pose. Synchronous and object-safe;
/// any socket reading happens off-thread and updates a shared cache.
pub trait PoseProvider: Send + Sync {
    fn latest(&self) -> Option<PoseSample>;
}

/// Wall-clock milliseconds, for stamping a freshly-read pose.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Row-major 3x3 rotation from aerospace euler angles (radians): yaw about Z,
/// pitch about Y, roll about X, composed `Rz(yaw) * Ry(pitch) * Rx(roll)`. This
/// is the world-from-body rotation the keyframe pose carries; the compute node
/// refines it during reconstruction.
pub fn euler_to_rotation(roll: f64, pitch: f64, yaw: f64) -> [f64; 9] {
    let (sr, cr) = roll.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let (sy, cy) = yaw.sin_cos();
    [
        cy * cp,
        cy * sp * sr - sy * cr,
        cy * sp * cr + sy * sr,
        sy * cp,
        sy * sp * sr + cy * cr,
        sy * sp * cr - cy * sr,
        -sp,
        cp * sr,
        cp * cr,
    ]
}

/// Local east-north-up offset (metres) of a geodetic point from the session
/// anchor, via the equirectangular approximation (accurate over the hundreds of
/// metres a single capture spans). Up is the home-relative altitude directly.
pub fn geodetic_to_enu(lat: f64, lon: f64, alt_rel: f64, anchor: &GlobalAnchor) -> [f64; 3] {
    const R_EARTH: f64 = 6_378_137.0;
    let dlat = (lat - anchor.lat).to_radians();
    let dlon = (lon - anchor.lon).to_radians();
    let east = dlon * anchor.lat.to_radians().cos() * R_EARTH;
    let north = dlat * R_EARTH;
    [east, north, alt_rel]
}

type Shared = Arc<Mutex<Option<PoseSample>>>;

/// Reads the flight-controller state socket and converts each snapshot to a
/// local-frame pose. The session anchor is fixed on the first fix and reused so
/// every pose is in one consistent local frame.
pub struct StateSockPose {
    latest: Shared,
    task: JoinHandle<()>,
}

impl StateSockPose {
    /// Spawn the reader against the state socket at `socket_path`.
    pub fn spawn(socket_path: String) -> Self {
        let latest: Shared = Arc::new(Mutex::new(None));
        let anchor: Arc<Mutex<Option<GlobalAnchor>>> = Arc::new(Mutex::new(None));
        let latest_t = latest.clone();
        let task = tokio::spawn(async move {
            loop {
                let mut stream =
                    match connect_with_retry(&socket_path, 5, Duration::from_millis(300)).await {
                        Ok(s) => s,
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            continue;
                        }
                    };
                // EOF / read error exits the while-let, dropping to the outer
                // loop to reconnect.
                while let Ok(Some(line)) = read_newline_line(&mut stream, STATE_V2_MAX_FRAME).await
                {
                    if let Some(sample) = parse_state_pose(&line, &anchor) {
                        *latest_t.lock().unwrap() = Some(sample);
                    }
                }
                // The connection dropped (clean EOF or error); pause before
                // reconnecting so an accept-then-EOF flap cannot spin the CPU.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        Self { latest, task }
    }
}

impl Drop for StateSockPose {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PoseProvider for StateSockPose {
    fn latest(&self) -> Option<PoseSample> {
        self.latest.lock().unwrap().clone()
    }
}

/// Parse one state-socket JSON line into a local-frame pose, fixing the session
/// anchor on the first valid fix.
fn parse_state_pose(line: &[u8], anchor: &Arc<Mutex<Option<GlobalAnchor>>>) -> Option<PoseSample> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    let pos = v.get("position")?;
    let att = v.get("attitude")?;
    let lat = pos.get("lat")?.as_f64()?;
    let lon = pos.get("lon")?.as_f64()?;
    let alt_rel = pos.get("alt_rel").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let alt_msl = pos.get("alt_msl").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let roll = att.get("roll")?.as_f64()?;
    let pitch = att.get("pitch")?.as_f64()?;
    let yaw = att.get("yaw")?.as_f64()?;
    let fix = v
        .get("gps")
        .and_then(|g| g.get("fix_type"))
        .and_then(|f| f.as_i64())
        .unwrap_or(0);

    // Fix the anchor once, on the first 3D fix at a real position.
    let mut anchor_guard = anchor.lock().unwrap();
    if anchor_guard.is_none() && fix >= 3 && (lat != 0.0 || lon != 0.0) {
        *anchor_guard = Some(GlobalAnchor {
            lat,
            lon,
            alt_m: alt_msl,
            yaw_rad: yaw,
        });
    }
    let anchor_now = *anchor_guard;
    drop(anchor_guard);

    let t = match &anchor_now {
        Some(a) => geodetic_to_enu(lat, lon, alt_rel, a),
        // No anchor yet (no fix): the pose is rotation-only at the origin.
        None => [0.0, 0.0, alt_rel],
    };
    let health = if fix >= 3 {
        VioHealth::Good
    } else {
        VioHealth::Degraded
    };
    Some(PoseSample {
        pose: Pose {
            r: euler_to_rotation(roll, pitch, yaw),
            t,
            cov: None,
        },
        anchor: anchor_now,
        source: PoseSource::LocalVio,
        ts_ms: now_ms(),
        health,
    })
}

/// Reads SLAM poses a compute node returns on the offload socket (for an
/// NPU-less board). Inert until a compute node produces poses; the reader simply
/// waits and reconnects.
pub struct OffloadPose {
    latest: Shared,
    task: JoinHandle<()>,
}

impl OffloadPose {
    pub fn spawn(socket_path: String) -> Self {
        let latest: Shared = Arc::new(Mutex::new(None));
        let latest_t = latest.clone();
        let task = tokio::spawn(async move {
            loop {
                let mut stream =
                    match connect_with_retry(&socket_path, 5, Duration::from_millis(500)).await {
                        Ok(s) => s,
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(1000)).await;
                            continue;
                        }
                    };
                // EOF / read error exits the while-let, dropping to the outer
                // loop to reconnect.
                while let Ok(Some(payload)) =
                    read_length_prefixed(&mut stream, STATE_V2_MAX_FRAME, true).await
                {
                    if let Ok(op) = OffloadedPose::from_msgpack(&payload) {
                        *latest_t.lock().unwrap() = Some(PoseSample {
                            pose: op.pose,
                            anchor: None,
                            source: PoseSource::OffloadedSlam,
                            ts_ms: op.ts_ms,
                            health: VioHealth::Good,
                        });
                    }
                }
                // The connection dropped; pause before reconnecting so an
                // accept-then-EOF flap cannot spin the CPU.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
        Self { latest, task }
    }
}

impl Drop for OffloadPose {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl PoseProvider for OffloadPose {
    fn latest(&self) -> Option<PoseSample> {
        self.latest.lock().unwrap().clone()
    }
}

/// Returns whichever of two providers has the fresher pose. Local is the
/// control-rate pose; the offloaded pose corrects drift when it is newer.
pub struct HybridPose {
    local: Box<dyn PoseProvider>,
    offload: Box<dyn PoseProvider>,
}

impl HybridPose {
    pub fn new(local: Box<dyn PoseProvider>, offload: Box<dyn PoseProvider>) -> Self {
        Self { local, offload }
    }
}

impl PoseProvider for HybridPose {
    fn latest(&self) -> Option<PoseSample> {
        match (self.local.latest(), self.offload.latest()) {
            (Some(l), Some(o)) => Some(if o.ts_ms > l.ts_ms { o } else { l }),
            (Some(l), None) => Some(l),
            (None, Some(o)) => Some(o),
            (None, None) => None,
        }
    }
}

/// A fixed pose list for the SITL harness and replay: each `latest()` returns
/// the next sample, holding the last once exhausted.
pub struct ReplayPose {
    samples: Vec<PoseSample>,
    idx: Mutex<usize>,
}

impl ReplayPose {
    pub fn new(samples: Vec<PoseSample>) -> Self {
        Self {
            samples,
            idx: Mutex::new(0),
        }
    }
}

impl PoseProvider for ReplayPose {
    fn latest(&self) -> Option<PoseSample> {
        if self.samples.is_empty() {
            return None;
        }
        let mut idx = self.idx.lock().unwrap();
        let i = (*idx).min(self.samples.len() - 1);
        if *idx < self.samples.len() {
            *idx += 1;
        }
        Some(self.samples[i].clone())
    }
}

/// Build the pose provider for a resolved tier, spawning the reader task(s) the
/// tier needs.
pub fn build_pose_provider(tier: PoseTier, config: &AtlasRuntimeConfig) -> Arc<dyn PoseProvider> {
    match tier {
        PoseTier::Local => Arc::new(StateSockPose::spawn(config.state_socket_path())),
        PoseTier::Offload => Arc::new(OffloadPose::spawn(config.offload_socket_path())),
        PoseTier::Hybrid => Arc::new(HybridPose::new(
            Box::new(StateSockPose::spawn(config.state_socket_path())),
            Box::new(OffloadPose::spawn(config.offload_socket_path())),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: i64, source: PoseSource) -> PoseSample {
        PoseSample {
            pose: Pose {
                r: euler_to_rotation(0.0, 0.0, 0.0),
                t: [0.0, 0.0, 0.0],
                cov: None,
            },
            anchor: None,
            source,
            ts_ms: ts,
            health: VioHealth::Good,
        }
    }

    #[test]
    fn euler_identity_is_identity_matrix() {
        let r = euler_to_rotation(0.0, 0.0, 0.0);
        assert_eq!(r, [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn enu_offset_is_metric_and_zero_at_anchor() {
        let a = GlobalAnchor {
            lat: 12.97,
            lon: 77.59,
            alt_m: 900.0,
            yaw_rad: 0.0,
        };
        assert_eq!(geodetic_to_enu(12.97, 77.59, 5.0, &a), [0.0, 0.0, 5.0]);
        // ~0.001 deg north is ~111 m.
        let enu = geodetic_to_enu(12.971, 77.59, 0.0, &a);
        assert!((enu[1] - 111.0).abs() < 2.0, "north ~111 m, got {}", enu[1]);
        assert!(enu[0].abs() < 1.0, "no east movement");
    }

    #[test]
    fn parse_state_pose_fixes_anchor_and_builds_pose() {
        let anchor = Arc::new(Mutex::new(None));
        let line = br#"{"position":{"lat":12.97,"lon":77.59,"alt_msl":900.0,"alt_rel":10.0,"heading":0.0},"attitude":{"roll":0.0,"pitch":0.0,"yaw":0.0},"gps":{"fix_type":3}}"#;
        let s = parse_state_pose(line, &anchor).expect("a pose");
        assert_eq!(s.source, PoseSource::LocalVio);
        assert_eq!(s.health, VioHealth::Good);
        assert!(s.anchor.is_some(), "anchor fixed on the first 3D fix");
        assert_eq!(s.pose.t, [0.0, 0.0, 10.0], "at the anchor, up = alt_rel");
        // A second sample moved north reuses the anchor (non-zero north offset).
        let line2 = br#"{"position":{"lat":12.971,"lon":77.59,"alt_msl":900.0,"alt_rel":10.0,"heading":0.0},"attitude":{"roll":0.0,"pitch":0.0,"yaw":0.0},"gps":{"fix_type":3}}"#;
        let s2 = parse_state_pose(line2, &anchor).unwrap();
        assert!(s2.pose.t[1] > 100.0, "moved north in the same frame");
    }

    #[test]
    fn parse_state_pose_without_fix_is_degraded_origin() {
        let anchor = Arc::new(Mutex::new(None));
        let line = br#"{"position":{"lat":0.0,"lon":0.0,"alt_msl":0.0,"alt_rel":2.0,"heading":0.0},"attitude":{"roll":0.0,"pitch":0.0,"yaw":0.0},"gps":{"fix_type":0}}"#;
        let s = parse_state_pose(line, &anchor).unwrap();
        assert_eq!(s.health, VioHealth::Degraded);
        assert!(s.anchor.is_none());
        assert_eq!(s.pose.t, [0.0, 0.0, 2.0]);
    }

    #[test]
    fn hybrid_returns_the_fresher_pose() {
        let local = Box::new(ReplayPose::new(vec![sample(100, PoseSource::LocalVio)]));
        let offload = Box::new(ReplayPose::new(vec![sample(
            200,
            PoseSource::OffloadedSlam,
        )]));
        let h = HybridPose::new(local, offload);
        let got = h.latest().unwrap();
        assert_eq!(got.source, PoseSource::OffloadedSlam, "offload is fresher");
    }

    #[test]
    fn replay_walks_then_holds_last() {
        let r = ReplayPose::new(vec![
            sample(1, PoseSource::LocalVio),
            sample(2, PoseSource::LocalVio),
        ]);
        assert_eq!(r.latest().unwrap().ts_ms, 1);
        assert_eq!(r.latest().unwrap().ts_ms, 2);
        assert_eq!(r.latest().unwrap().ts_ms, 2, "holds the last sample");
    }
}
