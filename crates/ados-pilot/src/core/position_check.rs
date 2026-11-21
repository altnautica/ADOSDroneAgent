//! Whether the EKF's horizontal position can be trusted.
//!
//! Two sources say it cannot:
//! - Navigation's `plugin.navigation.state` reports the external position degraded or lost
//!   while the active EKF source set takes its horizontal position from it;
//! - the FC's own optical flow (rate × range) and the EKF's velocity disagree
//!   by more than [`TRIP_M`] over [`WINDOW_MS`]. A frozen external position
//!   pulls the EKF velocity to rest while the aircraft flies on; the flow
//!   still measures the ground going by, whatever its quality.
//!
//! A third, graded: Navigation's pose sigma (`pose_sigma_m`). Over
//! [`UNCERTAIN_SIGMA_M`] and up to `pilot.max_pose_sigma_m` the position is
//! uncertain, not unverified: everything keeps flying under a speed cap
//! that tightens with the sigma ([`super::nav_cap`]); past the limit it is
//! unverified.
//!
//! While the position is unverified the pilot never holds a position on it:
//! a hold is a zero-velocity brake, stick flight is held to
//! [`UNVERIFIED_MAX_MPS`], a goto, skill or swarm goal stops, and nothing new
//! that flies on the estimate (a goto, a lease, a launch, a mission resume)
//! starts.

use std::collections::{BTreeMap, VecDeque};
use std::sync::LazyLock;

use ados_protocol::pilot::{ControlMode, PlaceClass, PositionUnverified, Readiness, Severity};

use super::Pilot;
use crate::geo::V2;

/// The window the flow and the estimate are compared over.
pub const WINDOW_MS: u64 = 3_000;
/// The displacement gap over the window that marks the position unverified.
pub const TRIP_M: f32 = 1.5;
/// The gap under which a tripped check clears.
pub const CLEAR_M: f32 = 0.75;
/// A flow gap longer than this is not bridged (the comparison restarts).
const MAX_STEP_MS: u64 = 200;
/// A flow check whose last flow is older than this no longer speaks.
const FLOW_STALE_MS: u64 = 1_000;
/// A `plugin.navigation.state` older than this no longer speaks for Navigation.
pub const EXTNAV_STALE_MS: u64 = 3_000;
/// `EK3_SRCn_POSXY` value for external navigation.
const POSXY_EXTNAV: f32 = 6.0;
/// Horizontal stick speed while the position is unverified.
pub const UNVERIFIED_MAX_MPS: f32 = 1.0;
/// The flow-against-estimate gap a tripped check brakes on is averaged over
/// this, ms: five 10 Hz flow samples smooth a dusty floor's flow, and the
/// lag stays short of the FC's velocity response (a 1 s average let the
/// brake overshoot into a 0.3-0.9 m/s backward slide in simulation).
const BRAKE_WINDOW_MS: u64 = 500;
/// The most the flow brake asks, m/s: the frozen-pose drifts it stops run
/// to 3 m/s, and an ask held under the gap leaves the vehicle sliding at
/// the rest.
pub const FLOW_BRAKE_MAX_MPS: f32 = 3.0;
/// `hold_reason` while the position is unverified.
pub const HOLD_POSITION_UNVERIFIED: &str = "position-unverified";

/// `PositionUnverified.reason` values.
pub const REASON_FLOW_DISAGREES: &str = "flow_disagrees";
pub const REASON_EXTNAV_LOST: &str = "extnav_lost";
pub const REASON_EXTNAV_DEGRADED: &str = "extnav_degraded";
pub const REASON_EXTNAV_STALE: &str = "extnav_stale";

/// Readiness id of the confined external-navigation launch settle.
pub const EXTNAV_SETTLE_ID: &str = "extnav_settle";
/// How long the FC's local velocity must stay within
/// [`crate::fc::extnav::STILL_MPS`] after its EKF starts using external
/// navigation before a confined launch, ms. That start makes the EKF ring
/// in velocity: in a simulated tunnel flight the ring reached 0.19 m/s with
/// a period near 6 s, and 4.6 s on, at lift-off, it still read −0.027 m/s
/// against a still vehicle; the climb then drifted 0.09 m. On a flight
/// whose start rang at 0.02 m/s and read −0.005 at lift-off the climb
/// drifted 0.013 m: the climb carries about 3 s of the error, so 0.01 m/s
/// keeps it near 0.03 m. A ring of that period stays within 0.01 m/s for
/// 2 s around a zero crossing only below 0.012 m/s, so 2 s still tells a
/// settled estimate from a ring passing through zero; a room start that
/// settled at once passed 2.4 s after it.
pub const EXTNAV_SETTLE_MS: u64 = 2000;

/// Navigation `health_reason`s that make a `degraded` external position
/// unverified. A `degraded` without one of these (a corridor flown on good
/// flow, a lidar or VIO still initialising) is still flown.
/// [`POSE_SIGMA_WIDE`] is graded on the sigma instead
/// ([`Pilot::position_unverified`]).
const UNVERIFYING_DEGRADED_REASONS: [&str; 4] = [
    "corridor_unmeasured",
    "flow_disagrees",
    "ekf_flow_disagree",
    "heading_unconfirmed",
];

/// Navigation's `health_reason` while its pose's one-sigma uncertainty is
/// over [`UNCERTAIN_SIGMA_M`] (a flow reading a few percent off down a long
/// corridor grows it with the distance flown).
pub const POSE_SIGMA_WIDE: &str = "pose_sigma_wide";
/// Over this one-sigma uncertainty (m) the position is uncertain: flown,
/// slowed ([`Pilot::pose_uncertainty_m`]).
pub const UNCERTAIN_SIGMA_M: f32 = 1.0;

fn degraded_reason_unverifies(reason: Option<&str>) -> bool {
    reason.is_some_and(|r| UNVERIFYING_DEGRADED_REASONS.contains(&r) || r.starts_with("imu_feed"))
}

/// The flow-against-estimate comparison.
#[derive(Debug, Clone, Default)]
pub struct FlowCheck {
    /// (time, (flow − estimate) × dt, dt ms) per flow sample inside the
    /// window.
    steps: VecDeque<(u64, V2, u64)>,
    last_ms: Option<u64>,
    tripped: bool,
    flow_mps: f32,
    estimate_mps: f32,
}

impl FlowCheck {
    /// Fold one flow sample in: the ground velocity it measures and the
    /// EKF's, both north-east, m/s.
    pub fn step(&mut self, flow_ne: V2, est_ne: V2, now_ms: u64) {
        let dt_ms = match self.last_ms {
            Some(t) if now_ms.saturating_sub(t) <= MAX_STEP_MS => now_ms.saturating_sub(t),
            Some(_) => {
                self.steps.clear();
                0
            }
            None => 0,
        };
        self.last_ms = Some(now_ms);
        let dt = dt_ms as f32 / 1000.0;
        self.steps.push_back((
            now_ms,
            [(flow_ne[0] - est_ne[0]) * dt, (flow_ne[1] - est_ne[1]) * dt],
            dt_ms,
        ));
        while self
            .steps
            .front()
            .is_some_and(|(t, _, _)| now_ms.saturating_sub(*t) >= WINDOW_MS)
        {
            self.steps.pop_front();
        }
        self.flow_mps = flow_ne[0].hypot(flow_ne[1]);
        self.estimate_mps = est_ne[0].hypot(est_ne[1]);
        let gap = self.gap_m();
        if gap > TRIP_M {
            self.tripped = true;
        } else if gap < CLEAR_M {
            self.tripped = false;
        }
    }

    /// A sample with no flow measurement: the comparison restarts.
    pub fn gap(&mut self) {
        self.last_ms = None;
        self.steps.clear();
    }

    /// The displacement gap over the window, metres.
    pub fn gap_m(&self) -> f32 {
        let s = self
            .steps
            .iter()
            .fold([0.0f32; 2], |a, (_, d, _)| [a[0] + d[0], a[1] + d[1]]);
        s[0].hypot(s[1])
    }

    /// Tripped, with the speeds it last compared, while the flow is fresh.
    fn verdict(&self, now_ms: u64) -> Option<(f32, f32)> {
        let fresh = self
            .last_ms
            .is_some_and(|t| now_ms.saturating_sub(t) <= FLOW_STALE_MS);
        (self.tripped && fresh).then_some((self.flow_mps, self.estimate_mps))
    }

    /// While tripped: the velocity, north-east m/s, to ask the flight
    /// controller for so that the ground velocity the flow measures goes to
    /// zero. The FC drives its EKF velocity to the ask, and the ground
    /// velocity is the EKF's plus the gap (flow − estimate), averaged over
    /// [`BRAKE_WINDOW_MS`]; so the ask is minus that gap, held to
    /// [`FLOW_BRAKE_MAX_MPS`]. `None` while not tripped (or the flow is
    /// stale) or with under 0.2 s of flow in the window: a quality-0 sample
    /// or a missing range restarts the comparison, and the hold then asks
    /// zero on the estimate as before.
    fn brake_ne(&self, now_ms: u64) -> Option<V2> {
        self.verdict(now_ms)?;
        let (mut sum, mut span_ms) = ([0.0f32; 2], 0u64);
        for (_, d, dt) in self
            .steps
            .iter()
            .filter(|(t, _, _)| now_ms.saturating_sub(*t) < BRAKE_WINDOW_MS)
        {
            sum = [sum[0] + d[0], sum[1] + d[1]];
            span_ms += dt;
        }
        if span_ms < 200 {
            return None;
        }
        let s = span_ms as f32 / 1000.0;
        Some(crate::geo::clamp_len(
            [-sum[0] / s, -sum[1] / s],
            FLOW_BRAKE_MAX_MPS,
        ))
    }
}

/// Navigation's last `plugin.navigation.state`, as far as the position check reads it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExtNav {
    pub mode: String,
    /// `good`, `degraded` or `lost`; `None` while Navigation named none.
    pub health: Option<String>,
    pub reason: Option<String>,
    /// The pose's one-sigma horizontal uncertainty (`pose_sigma_m`, m; the
    /// along-track sigma in a corridor), `None` while Navigation gives none.
    pub sigma_m: Option<f32>,
    pub at_ms: u64,
}

impl ExtNav {
    pub fn parse(payload: &serde_json::Value, now_ms: u64) -> Option<Self> {
        let s = |k: &str| payload.get(k).and_then(serde_json::Value::as_str);
        Some(Self {
            mode: s("mode")?.to_owned(),
            health: s("extnav_health").map(str::to_owned),
            reason: s("health_reason").map(str::to_owned),
            sigma_m: payload
                .get("pose_sigma_m")
                .and_then(serde_json::Value::as_f64)
                .map(|v| v as f32)
                .filter(|v| v.is_finite() && *v >= 0.0),
            at_ms: now_ms,
        })
    }
}

/// The body-frame (FRD) ground velocity of a flow rate over `range_m`:
/// forward motion reads as +y flow, rightward as −x (ArduPilot's
/// OPTICAL_FLOW convention).
pub fn flow_body_velocity(comp: [f32; 2], range_m: f32) -> V2 {
    [comp[1] * range_m, -comp[0] * range_m]
}

/// Rotate a body-frame horizontal velocity by `yaw` into north-east.
pub fn body_to_ne(v: V2, yaw: f32) -> V2 {
    let (s, c) = yaw.sin_cos();
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c]
}

impl Pilot {
    /// Fold the FC's newest OPTICAL_FLOW into the flow check.
    pub(super) fn on_flow(&mut self, now_ms: u64) {
        let Some(flow) = self.fc.flow.map(|f| f.v) else {
            return;
        };
        let range = if flow.ground_m > 0.0 {
            Some(flow.ground_m)
        } else {
            self.fc.rangefinder_agl(now_ms)
        };
        let (Some(range), Some(local), Some(att)) = (
            range,
            self.fc.fresh_local(now_ms),
            self.fc.fresh_attitude(now_ms),
        ) else {
            self.flow_check.gap();
            return;
        };
        // Quality 0 is no measurement; any other quality is used, as the
        // EKF itself does.
        if flow.quality == 0 || !flow.comp.iter().all(|c| c.is_finite()) {
            self.flow_check.gap();
            return;
        }
        let ne = body_to_ne(flow_body_velocity(flow.comp, range), att.yaw);
        self.flow_check.step(ne, [local.vn, local.ve], now_ms);
    }

    /// While the flow check is tripped, the velocity to ask so the vehicle
    /// stops over the ground ([`FlowCheck::brake_ne`]); `None` otherwise (a
    /// hold on an unverified position then asks zero).
    pub(super) fn flow_brake_ne(&self, now_ms: u64) -> Option<V2> {
        self.flow_check.brake_ne(now_ms)
    }

    /// The active set's `EK3_SRCn_POSXY`, when the parameter has been read.
    fn posxy_param(&self) -> Option<f32> {
        let name = match self.nav_set {
            1 => "EK3_SRC1_POSXY",
            2 => "EK3_SRC2_POSXY",
            3 => "EK3_SRC3_POSXY",
            _ => return None,
        };
        self.fc.params.get(name).copied()
    }

    /// Whether the active EKF source set takes its horizontal position from
    /// external navigation: its `EK3_SRCn_POSXY`, or Navigation's mode while
    /// that parameter is unread.
    fn on_extnav(&self, nav: &ExtNav) -> bool {
        if !(1..=3).contains(&self.nav_set) {
            return false;
        }
        match self.posxy_param() {
            Some(v) => (v - POSXY_EXTNAV).abs() < 0.5,
            None => nav.mode == "external_nav",
        }
    }

    /// Why the horizontal position is not trusted, `None` while it is.
    pub fn position_unverified(&self, now_ms: u64) -> Option<PositionUnverified> {
        if let Some((flow, est)) = self.flow_check.verdict(now_ms) {
            return Some(
                PositionUnverified::new(REASON_FLOW_DISAGREES)
                    .with_speeds(round1(flow), round1(est)),
            );
        }
        let fresh = self
            .extnav
            .as_ref()
            .filter(|n| now_ms.saturating_sub(n.at_ms) <= EXTNAV_STALE_MS);
        let Some(nav) = fresh else {
            // A source set known to take its position from Navigation, with
            // Navigation gone silent: nobody vouches for that position.
            let extnav_set = self
                .posxy_param()
                .is_some_and(|v| (v - POSXY_EXTNAV).abs() < 0.5);
            return (self.extnav.is_some() && extnav_set)
                .then(|| PositionUnverified::new(REASON_EXTNAV_STALE));
        };
        if !self.on_extnav(nav) {
            return None;
        }
        let reason = match nav.health.as_deref()? {
            "lost" => REASON_EXTNAV_LOST,
            _ if self.sigma_past_limit(nav) => {
                return Some(
                    PositionUnverified::new(REASON_EXTNAV_DEGRADED)
                        .with_detail(Some(POSE_SIGMA_WIDE.to_owned())),
                );
            }
            "degraded" if degraded_reason_unverifies(nav.reason.as_deref()) => {
                REASON_EXTNAV_DEGRADED
            }
            _ => return None,
        };
        Some(PositionUnverified::new(reason).with_detail(nav.reason.clone()))
    }

    /// The pose's sigma is past `max_pose_sigma_m`. A `pose_sigma_wide`
    /// report with no sigma counts as past it: an uncertainty the pilot
    /// cannot size is not flown on, as before the limit existed.
    fn sigma_past_limit(&self, nav: &ExtNav) -> bool {
        match nav.sigma_m {
            Some(s) => s > self.settings.pilot.max_pose_sigma_m,
            None => {
                nav.health.as_deref() == Some("degraded")
                    && nav.reason.as_deref() == Some(POSE_SIGMA_WIDE)
            }
        }
    }

    /// The pose's one-sigma uncertainty (m, one decimal) while it is over
    /// [`UNCERTAIN_SIGMA_M`] on a fresh external position the active source
    /// set flies on and nothing unverifies the position: the pilot flies
    /// on, slowed ([`Pilot::uncertain_cap_mps`]). `None` otherwise.
    pub fn pose_uncertainty_m(&self, now_ms: u64) -> Option<f32> {
        let nav = self
            .extnav
            .as_ref()
            .filter(|n| now_ms.saturating_sub(n.at_ms) <= EXTNAV_STALE_MS)
            .filter(|n| self.on_extnav(n))?;
        let s = nav.sigma_m.filter(|s| *s > UNCERTAIN_SIGMA_M)?;
        self.position_unverified(now_ms)
            .is_none()
            .then_some(round1(s))
    }

    /// While the position is unverified, a goal flown on it stops: the skill
    /// loses its lease, a goto or swarm goal ends, and the pilot brakes.
    pub(super) fn enforce_position_check(&mut self, now_ms: u64) {
        if self.position_unverified(now_ms).is_none() {
            return;
        }
        match self.owner() {
            ados_protocol::pilot::Owner::Skill => {
                self.revoke("position_unverified");
                self.brake_hold(Some(HOLD_POSITION_UNVERIFIED));
                self.skill_ended(now_ms);
            }
            ados_protocol::pilot::Owner::Goto | ados_protocol::pilot::Owner::Swarm => {
                self.brake_hold(Some(HOLD_POSITION_UNVERIFIED));
            }
            _ => {}
        }
    }

    /// `extnav_settle`, in a confined place whose active EKF source set takes
    /// its horizontal position from external navigation, on the ground: a
    /// launch waits until the FC's local velocity has stayed within
    /// [`crate::fc::extnav::STILL_MPS`] for [`EXTNAV_SETTLE_MS`] since the EKF
    /// began using external navigation. Blocks in `ados` mode, warns
    /// otherwise.
    pub(super) fn extnav_settle_item(&self, now_ms: u64) -> Option<Readiness> {
        if self.settings.pilot.place_class != PlaceClass::Confined
            || (self.fc.armed && !self.fc.landed())
        {
            return None;
        }
        let from_extnav = match self.posxy_param() {
            Some(v) => (v - POSXY_EXTNAV).abs() < 0.5,
            None => self
                .extnav
                .as_ref()
                .is_some_and(|n| n.mode == "external_nav"),
        };
        if !from_extnav {
            return None;
        }
        let ok = self
            .fc
            .extnav
            .still_for_ms(now_ms)
            .is_some_and(|ms| ms >= EXTNAV_SETTLE_MS);
        let severity = if self.control_mode() == ControlMode::Ados {
            Severity::Block
        } else {
            Severity::Warn
        };
        let msg = match self.fc.fresh_local(now_ms) {
            _ if ok => "External navigation settled".to_owned(),
            Some(l) => format!(
                "External navigation settling: {:.2} m/s, needs {} m/s for {} s",
                l.vn.hypot(l.ve),
                crate::fc::extnav::STILL_MPS,
                EXTNAV_SETTLE_MS / 1000
            ),
            None => "External navigation settling: no velocity from the FC".to_owned(),
        };
        Some(Readiness {
            id: EXTNAV_SETTLE_ID.into(),
            ok,
            severity,
            msg,
        })
    }
}

/// The readiness line for an unverified position. "(nav)" keeps it apart
/// from the EKF's own `position` item.
pub fn readiness_msg(p: &PositionUnverified) -> String {
    match (p.reason.as_str(), p.flow_mps, p.estimate_mps) {
        (REASON_FLOW_DISAGREES, Some(f), Some(e)) => {
            format!("Position unverified (nav): flow {f:.1} m/s, estimate {e:.1} m/s")
        }
        (REASON_EXTNAV_LOST, ..) => {
            with_detail("Position unverified (nav): external position lost", p)
        }
        (REASON_EXTNAV_STALE, ..) => {
            "Position unverified (nav): no report from vision navigation".to_owned()
        }
        _ => with_detail("Position unverified (nav): external position degraded", p),
    }
}

fn with_detail(base: &str, p: &PositionUnverified) -> String {
    match p.detail.as_deref() {
        Some(d) => format!("{base} ({})", detail_label(d)),
        None => base.to_owned(),
    }
}

/// The shared spec's operator text for a Navigation reason
/// (`flight-state.json` `position_unverified_labels`), else the key in words.
fn detail_label(key: &str) -> String {
    static LABELS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
        #[derive(serde::Deserialize)]
        struct Spec {
            position_unverified_labels: BTreeMap<String, String>,
        }
        serde_json::from_str::<Spec>(include_str!(
            "../../../../web-shared/src/cockpit-spec/flight-state.json"
        ))
        .expect("flight-state.json")
        .position_unverified_labels
    });
    LABELS
        .get(key)
        .cloned()
        .unwrap_or_else(|| key.replace('_', " "))
}

fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}
