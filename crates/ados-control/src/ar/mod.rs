//! The AR scene producer: projects the mission route, home, the flight-path
//! vector and the follow geometry into each watched camera and publishes the
//! result as the nav-stream kind `ar` (see [`scene`] for the items).
//!
//! It computes only what someone is watching: each tick it reads the hub's
//! view demand and builds items only for the cameras and layers leased there.
//! With no demand it computes nothing. A frame is computed once per tick for
//! every viewer of a camera, at 10 Hz (`ar.rate_hz`), raised to 20 Hz while the
//! view moves fast (yaw or gimbal rate above 20 °/s, groundspeed above 2 m/s).
//! A frame that draws the same as the last one is not republished; the last
//! one is re-sent once a second with its original `ts_ms` as a keep-alive.
//! `ar_frames_built` is logged every 10 s.
//!
//! Configuration: `ar.enabled` (default true) and `ar.rate_hz` (default 10) in
//! the agent config file.

pub mod geo;
mod inputs;
pub mod project;
pub mod scene;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;

use crate::ipc::state_client::StateIpcClient;
use crate::routes::nav::{NavHub, NavKind, ViewDemand};
use geo::Home;
use project::{Aim, Pose};
use scene::{CameraIn, MissionIn, MissionItem, MotionIn, Orbit, SceneIn};

use inputs::*;
pub use inputs::{cameras_from, scene_from};

/// The fast rate, Hz.
const FAST_HZ: f64 = 20.0;
/// Rates above which the view counts as moving fast.
const FAST_YAW_DPS: f64 = 20.0;
const FAST_GROUNDSPEED_MPS: f64 = 2.0;
/// The keep-alive period for an unchanged frame.
const KEEPALIVE: Duration = Duration::from_secs(1);
/// Two frames closer than this (normalised units) count as unchanged.
const SAME_TOL: f64 = 0.002;
/// How often the camera roster is re-read.
const ROSTER_EVERY: Duration = Duration::from_secs(5);
/// How often the build counter is logged.
const LOG_EVERY: Duration = Duration::from_secs(10);
/// The base tick.
const TICK: Duration = Duration::from_millis(50);
/// A tick this close before a build is due counts as on time, so wake-up
/// jitter on the 50 ms tick never pushes a build to the following tick.
const DUE_SLACK: Duration = Duration::from_millis(25);
/// A camera model angle change smaller than this (degrees) is not news to the
/// pilot: well under one obstacle cell.
const MODEL_ANGLE_TOL_DEG: f64 = 0.1;
/// Data older than this does not drive the scene.
const FRESH_MS: u64 = 1500;

/// The `ar:` block of the agent config.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ArConfig {
    pub enabled: bool,
    pub rate_hz: f64,
}

impl Default for ArConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rate_hz: 10.0,
        }
    }
}

impl ArConfig {
    /// Read `ar:` from the config file; defaults on absence or a parse error.
    pub fn load_from(path: &Path) -> Self {
        #[derive(Deserialize, Default)]
        struct Root {
            #[serde(default)]
            ar: ArConfig,
        }
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut cfg = serde_norway::from_str::<Root>(&text)
            .map(|r| r.ar)
            .unwrap_or_default();
        if !(1.0..=FAST_HZ).contains(&cfg.rate_hz) {
            cfg.rate_hz = ArConfig::default().rate_hz;
        }
        cfg
    }
}

/// One served camera's image model, as the AR builder last computed it. The
/// pilot's per-camera obstacle projection uses the same models.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraModel {
    pub frame_w: u32,
    pub frame_h: u32,
    pub hfov_deg: f64,
    pub vfov_deg: f64,
    /// Degrees below the horizon the optical axis points (+ looks down): the
    /// fresh gimbal pitch for the primary camera, otherwise the mount pitch.
    pub tilt_down_deg: f64,
    /// Degrees of camera yaw from the vehicle nose (an earth-frame gimbal yaw
    /// is converted with the current vehicle heading).
    pub yaw_deg: f64,
    /// A stabilised gimbal: its tilt is held against the horizon, so vehicle
    /// roll and pitch do not turn it. A body-mounted camera turns with them.
    pub stabilised: bool,
}

impl CameraModel {
    /// Whether `other` draws the same within [`MODEL_ANGLE_TOL_DEG`].
    fn same_as(&self, other: &Self) -> bool {
        let close = |a: f64, b: f64| (a - b).abs() < MODEL_ANGLE_TOL_DEG;
        self.frame_w == other.frame_w
            && self.frame_h == other.frame_h
            && self.stabilised == other.stabilised
            && close(self.hfov_deg, other.hfov_deg)
            && close(self.vfov_deg, other.vfov_deg)
            && close(self.tilt_down_deg, other.tilt_down_deg)
            && close(self.yaw_deg, other.yaw_deg)
    }
}

type Models = BTreeMap<String, CameraModel>;

static MODELS: LazyLock<watch::Sender<Models>> = LazyLock::new(|| watch::channel(Models::new()).0);

/// The latest camera models by served camera id (`main` for the primary leg).
/// Empty before the builder's first tick; cameras with no `fov_deg` are absent.
pub fn camera_models() -> Models {
    MODELS.borrow().clone()
}

/// Publish the builder's camera models. Subscribers of [`model_changes`] wake
/// only when a camera appears, goes, or moves by more than
/// [`MODEL_ANGLE_TOL_DEG`].
pub fn publish_models(models: Models) {
    MODELS.send_if_modified(|cur| {
        let same = cur.len() == models.len()
            && cur
                .iter()
                .zip(models.iter())
                .all(|((a, ma), (b, mb))| a == b && ma.same_as(mb));
        if !same {
            *cur = models;
        }
        !same
    });
}

/// A receiver that wakes when the camera models change, so the pilot's
/// obstacle grid follows a gimbal move at once.
pub fn model_changes() -> watch::Receiver<Models> {
    MODELS.subscribe()
}

/// One roster row, reduced to what the scene needs.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterCam {
    pub served_id: String,
    pub primary: bool,
    pub width: u32,
    pub height: u32,
    pub fov_deg: Option<f64>,
    pub mount_pitch_deg: Option<f64>,
}

/// The declared legs of a roster (`GET /api/video/roster` rows) with their
/// served ids: the primary leg (role `primary`, else the first declared leg)
/// is served as `main`, every other leg under its own id. Discovered but
/// unassigned devices are not cameras anyone can watch.
pub fn roster_cams(rows: &[Value]) -> Vec<RosterCam> {
    let declared: Vec<&Value> = rows
        .iter()
        .filter(|r| r.get("state").and_then(Value::as_str) != Some("discovered_unassigned"))
        .collect();
    let primary = declared
        .iter()
        .position(|r| r.get("role").and_then(Value::as_str) == Some("primary"))
        .unwrap_or(0);
    declared
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let id = r.get("id")?.as_str()?;
            Some(RosterCam {
                served_id: if i == primary {
                    "main".to_owned()
                } else {
                    id.to_owned()
                },
                primary: i == primary,
                width: r.get("width").and_then(Value::as_u64).unwrap_or(1280) as u32,
                height: r.get("height").and_then(Value::as_u64).unwrap_or(720) as u32,
                fov_deg: r.get("fov_deg").and_then(Value::as_f64),
                mount_pitch_deg: r.get("mount_pitch_deg").and_then(Value::as_f64),
            })
        })
        .collect()
}

/// The last vehicle yaw (degrees) and gimbal `(pitch, yaw)` and when they were
/// read, for the fast-motion rate.
type YawSample = (Instant, f64, Option<(f64, f64)>);

/// The publish decision and rate state, clock-injected for the tests.
#[derive(Debug)]
pub struct Builder {
    period: Duration,
    /// The scheduled time of the last build. The next one is due a period
    /// later, at the period of the tick that checks it (so a switch to the
    /// fast rate takes effect at once), and the rate holds whatever the tick
    /// jitter because each slot follows the last slot, not the late tick.
    last_slot: Option<Instant>,
    last_sent: Option<Instant>,
    prev: Option<Value>,
    prev_yaw: Option<YawSample>,
    /// Frames computed since start.
    pub frames_built: u64,
    /// Frames handed to the hub (changes plus keep-alives).
    pub frames_published: u64,
}

impl Builder {
    pub fn new(rate_hz: f64) -> Self {
        Self {
            period: Duration::from_secs_f64(1.0 / rate_hz),
            last_slot: None,
            last_sent: None,
            prev: None,
            prev_yaw: None,
            frames_built: 0,
            frames_published: 0,
        }
    }

    /// Whether the view is moving fast enough for the 20 Hz rate.
    fn fast(&mut self, now: Instant, scene: &SceneIn, gimbal: Option<(f64, f64, bool)>) -> bool {
        let yaw = scene.pose.map(|p| p.yaw.to_degrees());
        let gim = gimbal.map(|(p, y, _)| (p, y));
        let mut fast = scene.groundspeed > FAST_GROUNDSPEED_MPS;
        if let (Some((t0, y0, g0)), Some(y1)) = (self.prev_yaw, yaw) {
            let dt = now.saturating_duration_since(t0).as_secs_f64();
            if dt > 0.0 {
                let wrap = |d: f64| (d + 540.0).rem_euclid(360.0) - 180.0;
                fast |= wrap(y1 - y0).abs() / dt > FAST_YAW_DPS;
                if let (Some((p0, gy0)), Some((p1, gy1))) = (g0, gim) {
                    fast |= (p1 - p0).abs() / dt > FAST_YAW_DPS
                        || wrap(gy1 - gy0).abs() / dt > FAST_YAW_DPS;
                }
            }
        }
        if let Some(y) = yaw {
            self.prev_yaw = Some((now, y, gim));
        }
        fast
    }

    /// Whether a build is due at `now` at this `period`, recording its slot
    /// when it is. A builder more than a period behind (a stall, or a return
    /// from no demand) restarts its schedule from `now`.
    fn due(&mut self, now: Instant, period: Duration) -> bool {
        let base = match self.last_slot.map(|slot| slot + period) {
            Some(due) if now + DUE_SLACK < due => return false,
            Some(due) if now.saturating_duration_since(due) < period => due,
            _ => now,
        };
        self.last_slot = Some(base);
        true
    }

    /// One tick. Returns the frame to publish, if any.
    pub fn step(
        &mut self,
        now: Instant,
        demand: &ViewDemand,
        scene: &SceneIn,
        cameras: &[CameraIn],
        gimbal: Option<(f64, f64, bool)>,
    ) -> Option<Value> {
        let wanted = ar_demand(demand);
        if wanted.is_empty() {
            // Nobody watches: compute nothing, and start fresh next time.
            self.prev = None;
            self.prev_yaw = None;
            return None;
        }
        let fast = self.fast(now, scene, gimbal);
        let period = if fast {
            Duration::from_secs_f64(1.0 / FAST_HZ)
        } else {
            self.period
        };
        if !self.due(now, period) {
            return None;
        }
        let frame = scene::build(scene, cameras, &wanted);
        self.frames_built += 1;
        let unchanged = self
            .prev
            .as_ref()
            .is_some_and(|p| scene::same_scene(p, &frame, SAME_TOL));
        if unchanged {
            if self
                .last_sent
                .is_some_and(|t| now.saturating_duration_since(t) >= KEEPALIVE)
            {
                self.last_sent = Some(now);
                self.frames_published += 1;
                return self.prev.clone();
            }
            return None;
        }
        self.prev = Some(frame.clone());
        self.last_sent = Some(now);
        self.frames_published += 1;
        Some(frame)
    }
}

/// Unix milliseconds of an `Instant` in the past.
fn unix_ms_of(at: Instant) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    now - at.elapsed().as_millis() as i64
}

/// Spawn the builder. Returns `None` when `ar.enabled` is false.
pub fn spawn(
    config_path: &Path,
    state: StateIpcClient,
    hub: NavHub,
) -> Option<tokio::task::JoinHandle<()>> {
    let cfg = ArConfig::load_from(config_path);
    if !cfg.enabled {
        tracing::info!("ar_builder_disabled");
        return None;
    }
    Some(tokio::spawn(run(cfg, state, hub)))
}

/// The 10 s build counter record.
#[derive(Debug, Clone, PartialEq)]
pub struct BuildLog {
    pub built: u64,
    pub published: u64,
    pub built_last_10s: u64,
    pub cameras: Vec<String>,
}

/// The builder loop's state between ticks.
struct Runner {
    state: StateIpcClient,
    hub: NavHub,
    builder: Builder,
    roster: Vec<RosterCam>,
    roster_at: Option<Instant>,
    log_at: Instant,
    logged_built: u64,
}

impl Runner {
    fn new(cfg: &ArConfig, state: StateIpcClient, hub: NavHub, now: Instant) -> Self {
        Self {
            state,
            hub,
            builder: Builder::new(cfg.rate_hz),
            roster: Vec::new(),
            roster_at: None,
            log_at: now,
            logged_built: 0,
        }
    }

    /// One tick: refresh the roster, build for the current demand when the
    /// vehicle state is fresh, and return the counter record every
    /// [`LOG_EVERY`] whether or not there was state to build from.
    async fn tick(&mut self, now: Instant) -> Option<BuildLog> {
        if self
            .roster_at
            .is_none_or(|t| now.saturating_duration_since(t) >= ROSTER_EVERY)
        {
            self.roster_at = Some(now);
            if let Ok(rows) =
                tokio::task::spawn_blocking(crate::routes::camera_config::load_roster).await
            {
                self.roster = roster_cams(&rows);
            }
        }
        let demand = self.hub.prune_views();
        if let Some((at, snapshot)) = self.state.snapshot_at() {
            self.build(now, &demand, at, &snapshot);
        }
        if now.saturating_duration_since(self.log_at) < LOG_EVERY {
            return None;
        }
        self.log_at = now;
        let built = self.builder.frames_built;
        let log = BuildLog {
            built,
            published: self.builder.frames_published,
            built_last_10s: built - self.logged_built,
            cameras: demand.cameras.keys().cloned().collect(),
        };
        self.logged_built = built;
        Some(log)
    }

    fn build(&mut self, now: Instant, demand: &ViewDemand, at: Instant, snapshot: &Value) {
        let gimbal = gimbal_of(snapshot);
        let yaw_deg = snapshot
            .get("attitude")
            .and_then(|a| f(a, "yaw"))
            .unwrap_or(0.0)
            .to_degrees();
        let (cams, models) = cameras_from(&self.roster, gimbal, yaw_deg);
        publish_models(models);
        if !demand
            .cameras
            .values()
            .any(|l| l.contains("ar_route") || l.contains("ar_motion"))
        {
            self.builder
                .step(now, demand, &SceneIn::default(), &cams, None);
            return;
        }
        let fresh = |k| {
            self.hub
                .latest(k)
                .filter(|(_, age)| *age <= FRESH_MS)
                .map(|(v, _)| v)
        };
        let pilot = fresh(NavKind::Pilot);
        let follow = fresh(NavKind::Follow);
        let scene = scene_from(snapshot, unix_ms_of(at), pilot.as_ref(), follow.as_ref());
        if let Some(frame) = self.builder.step(now, demand, &scene, &cams, gimbal) {
            let ts = frame
                .get("ts_ms")
                .and_then(Value::as_i64)
                .unwrap_or(scene.ts_ms);
            self.hub.ingest_host(NavKind::Ar, ts, frame);
        }
    }
}

async fn run(cfg: ArConfig, state: StateIpcClient, hub: NavHub) {
    let mut runner = Runner::new(&cfg, state, hub, Instant::now());
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Some(log) = runner.tick(Instant::now()).await {
            tracing::info!(
                ar_frames_built = log.built,
                ar_frames_published = log.published,
                built_last_10s = log.built_last_10s,
                cameras = ?log.cameras,
                "ar_builder"
            );
        }
    }
}

#[cfg(test)]
mod tests;
