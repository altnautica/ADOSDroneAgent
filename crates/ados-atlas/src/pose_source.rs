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
//! - [`HybridPose`] uses whichever of the two arrived more recently.
//! - [`ReplayPose`] walks a fixed pose list, for the SITL harness.
//!
//! Every sample carries BOTH a wall-clock stamp and a local monotonic arrival
//! instant, and every age or ordering decision uses the monotonic one. The
//! wall-clock stamp is for the keyframe envelope (a consumer on another host
//! needs a common time base); it is never a freshness or a precedence signal,
//! because the drone and a compute node are not clock-synced and a node whose
//! clock runs fast would otherwise win every comparison forever and
//! drift-correct the aircraft's world frame with a wrong pose.

use std::sync::{Arc, LazyLock};

// `parking_lot::Mutex` rather than `std::sync::Mutex`: every one of these
// critical sections is a pointer-sized read or write with no error path, so a
// poisoning result to unwrap adds nothing but noise at the call site. No guard
// is ever held across an `.await` here.
use parking_lot::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ados_protocol::atlas::{
    GlobalAnchor, OffloadedPose, Pose, PoseSource, VioHealth, POSE_COV_LEN,
};
use ados_protocol::ipc::{connect_with_retry, read_length_prefixed};
use ados_protocol::state::{read_state_value, STATE_V2_MAX_FRAME};
use tokio::task::JoinHandle;

use crate::runtime::{AtlasRuntimeConfig, PosePrior, PoseTier};

/// A pose plus the metadata the capture path needs to stamp a keyframe.
#[derive(Debug, Clone, PartialEq)]
pub struct PoseSample {
    pub pose: Pose,
    pub anchor: Option<GlobalAnchor>,
    pub source: PoseSource,
    /// Wall-clock milliseconds this sample refers to. For a local sample this
    /// is the read time; for an offloaded one it is the REMOTE node's stamp. Use
    /// it to derive a keyframe's `ts_unix_ms`, never to compare two samples.
    pub ts_ms: i64,
    /// Local monotonic nanoseconds this sample arrived on THIS host. The only
    /// valid basis for an age or a precedence comparison — see the module docs.
    pub arrival_mono_ns: i64,
    pub health: VioHealth,
    /// Row-major 6x6 covariance of `pose`, `POSE_COV_LEN` elements, or empty
    /// when the producer could not state one. Carried beside the pose rather
    /// than inside it so the ~10 Hz live pose lane does not pay 288 bytes per
    /// message for a value only the reconstructor reads.
    pub cov: Vec<f64>,
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

/// Process-monotonic nanoseconds, anchored on first use.
///
/// `CLOCK_MONOTONIC` cannot be stepped, so this is the only age measurement
/// that survives a wall-clock correction mid-flight (chrony stepping the clock,
/// or a GNSS time fix landing after boot). Every freshness and precedence
/// decision in this module reads it; nothing crossing a process boundary does,
/// because a monotonic instant is meaningless on another host.
pub fn mono_ns() -> i64 {
    static ANCHOR: LazyLock<Instant> = LazyLock::new(Instant::now);
    ANCHOR.elapsed().as_nanos() as i64
}

/// Process-monotonic milliseconds (see [`mono_ns`]).
pub fn mono_ms() -> i64 {
    mono_ns() / 1_000_000
}

/// The current wall-clock-minus-monotonic offset, in nanoseconds, so a
/// consumer can reproduce a keyframe's `ts_unix_ms` from its `ts_monotonic_ns`.
pub fn clock_offset_ns() -> i64 {
    let realtime_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    realtime_ns.saturating_sub(mono_ns())
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
    /// Spawn the reader against the state socket at `socket_path`, stating the
    /// pose uncertainty for this rig via `prior`.
    pub fn spawn(socket_path: String, prior: PosePrior) -> Self {
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
                // Read decoded state snapshots off the socket. The shared reader
                // auto-detects v1 (newline JSON) vs v2 (length-prefixed msgpack)
                // per frame, so a v2 producer is read correctly rather than
                // mis-parsed as v1 (which silently yielded no pose). Clean EOF or
                // an unrecoverable framing/IO error breaks to the outer loop to
                // reconnect.
                // The loop ends on the first non-`Ok(Some)` (clean EOF or an
                // unrecoverable framing/IO error), dropping to the outer
                // reconnect.
                while let Ok(Some(v)) = read_state_value(&mut stream).await {
                    if let Some(sample) = parse_state_pose(&v, &anchor, &prior) {
                        *latest_t.lock() = Some(sample);
                    }
                }
                // The connection dropped. DROP the cached pose: the flight
                // controller link is down, so there is no current pose, and
                // holding the last one forward would let the capture path tag
                // every subsequent frame with a frozen pose and enter it into the
                // reconstruction as truth. An absent pose is the honest reading
                // and the capture loop already drops frames on it.
                //
                // This does NOT replace the consumer-side age gate: a socket that
                // stays up while the producer stops publishing never reaches this
                // line, and only an age check catches that. The two cover
                // different failures and both are needed.
                *latest_t.lock() = None;
                // Pause before reconnecting so an accept-then-EOF flap cannot
                // spin the CPU.
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
        self.latest.lock().clone()
    }
}

/// A diagonal row-major 6x6 covariance from a position and an orientation
/// sigma (metres and radians), variances on the diagonal.
///
/// Diagonal is the honest shape here: the flight controller publishes no
/// cross-covariance terms on the state socket, so inventing off-diagonal
/// correlations would assert structure nobody measured.
pub fn diagonal_cov(position_sigma_m: f64, orientation_sigma_rad: f64) -> Vec<f64> {
    let mut cov = vec![0.0_f64; POSE_COV_LEN];
    let pos_var = position_sigma_m * position_sigma_m;
    let ori_var = orientation_sigma_rad * orientation_sigma_rad;
    for i in 0..3 {
        cov[i * 6 + i] = pos_var;
    }
    for i in 3..6 {
        cov[i * 6 + i] = ori_var;
    }
    cov
}

/// Convert one decoded state snapshot into a local-frame pose, fixing the
/// session anchor on the first valid fix. The caller decodes the wire frame
/// (v1 JSON or v2 msgpack) into a field-addressed value first.
fn parse_state_pose(
    v: &serde_json::Value,
    anchor: &Arc<Mutex<Option<GlobalAnchor>>>,
    prior: &PosePrior,
) -> Option<PoseSample> {
    let pos = v.get("position")?;
    let att = v.get("attitude")?;
    let lat = pos.get("lat")?.as_f64()?;
    let lon = pos.get("lon")?.as_f64()?;
    let alt_rel = pos.get("alt_rel").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let alt_msl = pos.get("alt_msl").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let roll = att.get("roll")?.as_f64()?;
    let pitch = att.get("pitch")?.as_f64()?;
    let yaw = att.get("yaw")?.as_f64()?;
    let gps = v.get("gps");
    let fix = gps
        .and_then(|g| g.get("fix_type"))
        .and_then(|f| f.as_i64())
        .unwrap_or(0);
    // GPS_RAW_INT's eph / epv, as the state socket republishes them: dilution
    // of precision, UNITLESS. The metric h_acc / v_acc extension fields are not
    // on this socket, so a metre sigma is DOP x UERE and the UERE depends on the
    // receiver class, which only the operator knows. See `PosePrior`.
    let hdop = gps
        .and_then(|g| g.get("eph"))
        .and_then(|x| x.as_f64())
        .filter(|d| d.is_finite() && *d > 0.0);
    let vdop = gps
        .and_then(|g| g.get("epv"))
        .and_then(|x| x.as_f64())
        .filter(|d| d.is_finite() && *d > 0.0);

    // Fix the anchor once, on the first 3D fix at a real position.
    let mut anchor_guard = anchor.lock();
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
    // The worse of the two dilution figures drives the sigma: a good HDOP with a
    // poor VDOP is still a poor 3D position, and a prior must not be tighter
    // than the worst axis it covers.
    let dop = match (hdop, vdop) {
        (Some(h), Some(v)) => Some(h.max(v)),
        (Some(h), None) => Some(h),
        (None, Some(v)) => Some(v),
        (None, None) => None,
    };
    let cov = prior.covariance(dop, fix >= 3);
    Some(PoseSample {
        pose: Pose {
            r: euler_to_rotation(roll, pitch, yaw),
            t,
            // Deliberately absent on the live pose lane; the keyframe lane
            // carries `cov` below. See `PoseSample::cov`.
            cov: None,
        },
        anchor: anchor_now,
        source: PoseSource::LocalVio,
        ts_ms: now_ms(),
        arrival_mono_ns: mono_ns(),
        health,
        cov,
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
    /// Spawn the reader against the offload socket at `socket_path`. `prior`
    /// states the uncertainty of an offloaded SLAM pose for this rig.
    pub fn spawn(socket_path: String, prior: PosePrior) -> Self {
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
                        // `ts_ms` is the REMOTE node's clock and is carried
                        // through untouched for the keyframe stamp;
                        // `arrival_mono_ns` is this host's monotonic clock and is
                        // what every age and precedence check reads. A node whose
                        // clock runs fast can no longer win a comparison it did
                        // not earn.
                        let cov = prior.slam_covariance();
                        *latest_t.lock() = Some(PoseSample {
                            pose: op.pose,
                            anchor: None,
                            source: PoseSource::OffloadedSlam,
                            ts_ms: op.ts_ms,
                            arrival_mono_ns: mono_ns(),
                            health: VioHealth::Good,
                            cov,
                        });
                    }
                }
                // The link to the compute node dropped: there is no current
                // offloaded pose. Drop the cache for the same reason the local
                // reader does — a held-forward SLAM pose would be tagged onto
                // every later frame as truth.
                *latest_t.lock() = None;
                // Pause before reconnecting so an accept-then-EOF flap cannot
                // spin the CPU.
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
        self.latest.lock().clone()
    }
}

/// Returns whichever of two providers produced the more recently ARRIVED pose.
/// Local is the control-rate pose; the offloaded pose corrects drift when it is
/// newer.
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
            // Compare LOCAL MONOTONIC ARRIVAL, never the two samples' own
            // timestamps. The local sample is stamped with this host's wall
            // clock and the offloaded one with the compute node's, and the two
            // are not synced — so comparing them let a node whose clock ran even
            // slightly fast win permanently, and drift-correct the drone's world
            // frame with a pose that was actually older. Arrival is measured on
            // one clock, so the comparison is meaningful.
            (Some(l), Some(o)) => Some(if o.arrival_mono_ns > l.arrival_mono_ns {
                o
            } else {
                l
            }),
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
        let mut idx = self.idx.lock();
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
    let prior = config.pose_prior;
    match tier {
        PoseTier::Local => Arc::new(StateSockPose::spawn(config.state_socket_path(), prior)),
        PoseTier::Offload => Arc::new(OffloadPose::spawn(config.offload_socket_path(), prior)),
        PoseTier::Hybrid => Arc::new(HybridPose::new(
            Box::new(StateSockPose::spawn(config.state_socket_path(), prior)),
            Box::new(OffloadPose::spawn(config.offload_socket_path(), prior)),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(arrival_ns: i64, source: PoseSource) -> PoseSample {
        PoseSample {
            pose: Pose {
                r: euler_to_rotation(0.0, 0.0, 0.0),
                t: [0.0, 0.0, 0.0],
                cov: None,
            },
            anchor: None,
            source,
            ts_ms: arrival_ns / 1_000_000,
            arrival_mono_ns: arrival_ns,
            health: VioHealth::Good,
            cov: diagonal_cov(1.0, 0.01),
        }
    }

    /// A sample whose remote wall-clock stamp and local arrival disagree, which
    /// is exactly the shape a clock-skewed compute node produces.
    fn skewed_sample(arrival_ns: i64, ts_ms: i64, source: PoseSource) -> PoseSample {
        PoseSample {
            ts_ms,
            ..sample(arrival_ns, source)
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
        let prior = PosePrior::default();
        let v: serde_json::Value = serde_json::from_str(
            r#"{"position":{"lat":12.97,"lon":77.59,"alt_msl":900.0,"alt_rel":10.0,"heading":0.0},"attitude":{"roll":0.0,"pitch":0.0,"yaw":0.0},"gps":{"fix_type":3,"eph":1.2,"epv":1.8}}"#,
        )
        .unwrap();
        let s = parse_state_pose(&v, &anchor, &prior).expect("a pose");
        assert_eq!(s.source, PoseSource::LocalVio);
        assert_eq!(s.health, VioHealth::Good);
        assert!(s.anchor.is_some(), "anchor fixed on the first 3D fix");
        assert_eq!(s.pose.t, [0.0, 0.0, 10.0], "at the anchor, up = alt_rel");
        assert_eq!(
            s.cov.len(),
            POSE_COV_LEN,
            "a fixed pose states a full 6x6 covariance"
        );
        assert!(
            s.pose.cov.is_none(),
            "the live pose lane does not carry the 288-byte covariance"
        );
        // The position variance tracks the FC's reported DOP: worse DOP, larger
        // variance. This is what makes the reconstructor's prior honest.
        let sharp: serde_json::Value = serde_json::from_str(
            r#"{"position":{"lat":12.97,"lon":77.59,"alt_msl":900.0,"alt_rel":10.0},"attitude":{"roll":0.0,"pitch":0.0,"yaw":0.0},"gps":{"fix_type":3,"eph":0.6,"epv":0.8}}"#,
        )
        .unwrap();
        let s_sharp = parse_state_pose(&sharp, &anchor, &prior).unwrap();
        assert!(
            s_sharp.cov[0] < s.cov[0],
            "a better DOP yields a tighter position prior ({} vs {})",
            s_sharp.cov[0],
            s.cov[0]
        );
        // A second sample moved north reuses the anchor (non-zero north offset).
        let v2: serde_json::Value = serde_json::from_str(
            r#"{"position":{"lat":12.971,"lon":77.59,"alt_msl":900.0,"alt_rel":10.0,"heading":0.0},"attitude":{"roll":0.0,"pitch":0.0,"yaw":0.0},"gps":{"fix_type":3}}"#,
        )
        .unwrap();
        let s2 = parse_state_pose(&v2, &anchor, &prior).unwrap();
        assert!(s2.pose.t[1] > 100.0, "moved north in the same frame");
    }

    #[test]
    fn parse_state_pose_without_fix_is_degraded_origin() {
        let anchor = Arc::new(Mutex::new(None));
        let v: serde_json::Value = serde_json::from_str(
            r#"{"position":{"lat":0.0,"lon":0.0,"alt_msl":0.0,"alt_rel":2.0,"heading":0.0},"attitude":{"roll":0.0,"pitch":0.0,"yaw":0.0},"gps":{"fix_type":0}}"#,
        )
        .unwrap();
        let s = parse_state_pose(&v, &anchor, &PosePrior::default()).unwrap();
        assert_eq!(s.health, VioHealth::Degraded);
        assert!(s.anchor.is_none());
        assert_eq!(s.pose.t, [0.0, 0.0, 2.0]);
        // No fix means no stated position prior, not a fabricated one.
        assert!(
            s.cov.is_empty(),
            "without a 3D fix the producer states no covariance rather than guessing"
        );
    }

    #[test]
    fn hybrid_prefers_the_more_recently_arrived_pose() {
        let local = Box::new(ReplayPose::new(vec![sample(100, PoseSource::LocalVio)]));
        let offload = Box::new(ReplayPose::new(vec![sample(
            200,
            PoseSource::OffloadedSlam,
        )]));
        let h = HybridPose::new(local, offload);
        let got = h.latest().unwrap();
        assert_eq!(
            got.source,
            PoseSource::OffloadedSlam,
            "offload arrived later"
        );
    }

    #[test]
    fn a_clock_skewed_compute_node_cannot_win_the_hybrid_comparison() {
        // The regression this pins: the offloaded sample's wall-clock stamp is
        // 10 s AHEAD (a compute node whose clock runs fast) while it actually
        // arrived EARLIER on this host's monotonic clock. Comparing `ts_ms` made
        // the stale offloaded pose win permanently and drift-correct the drone's
        // world frame with it.
        let local = Box::new(ReplayPose::new(vec![skewed_sample(
            5_000_000_000,
            1_000,
            PoseSource::LocalVio,
        )]));
        let offload = Box::new(ReplayPose::new(vec![skewed_sample(
            1_000_000_000,
            11_000,
            PoseSource::OffloadedSlam,
        )]));
        let h = HybridPose::new(local, offload);
        let got = h.latest().unwrap();
        assert_eq!(
            got.source,
            PoseSource::LocalVio,
            "the locally-fresher pose wins even though the remote stamp is 10 s ahead"
        );
    }

    #[test]
    fn replay_walks_then_holds_last() {
        let r = ReplayPose::new(vec![
            sample(1_000_000, PoseSource::LocalVio),
            sample(2_000_000, PoseSource::LocalVio),
        ]);
        assert_eq!(r.latest().unwrap().ts_ms, 1);
        assert_eq!(r.latest().unwrap().ts_ms, 2);
        assert_eq!(r.latest().unwrap().ts_ms, 2, "holds the last sample");
    }

    #[test]
    fn the_monotonic_clock_advances_and_the_offset_reconstructs_wall_time() {
        let a = mono_ns();
        let b = mono_ns();
        assert!(b >= a, "monotonic never goes backwards");
        // ts_unix_ms is derived as (mono + offset); the derivation must land on
        // the real wall clock, which is what a consumer on another host relies on.
        let derived_ms = (mono_ns() + clock_offset_ns()) / 1_000_000;
        assert!(
            (derived_ms - now_ms()).abs() < 100,
            "derived {derived_ms} vs wall {}",
            now_ms()
        );
    }
}
