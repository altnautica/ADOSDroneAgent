use ados_protocol::mavlink::ardupilotmega::{MavMessage, OPTICAL_FLOW_DATA};
use ados_protocol::pilot::{Owner, Request};
use serde_json::json;

use super::harness::World;
use crate::modes::copter;

/// Run `ms`, the FC sending OPTICAL_FLOW `comp` (rad/s) at `quality` over
/// the world's range every tick.
pub(super) fn run_flow(w: &mut World, comp: [f32; 2], quality: u8, ms: u64) {
    let end = w.now + ms;
    while w.now < end {
        w.inbox.push(MavMessage::OPTICAL_FLOW(OPTICAL_FLOW_DATA {
            flow_comp_m_x: comp[0],
            flow_comp_m_y: comp[1],
            quality,
            ground_distance: w.agl.unwrap_or(-1.0),
            ..Default::default()
        }));
        w.run(20);
    }
}

/// Airborne at 1.9 m in GUIDED, flown by the pilot.
fn hovering() -> World {
    let mut w = World::new();
    w.airborne(copter::GUIDED);
    w.agl = Some(1.9);
    w.action("take_control");
    w.run(200);
    assert_eq!(w.p.owner(), Owner::Pilot);
    w
}

fn position_check_msg(w: &World) -> Option<String> {
    w.p.readiness(w.now)
        .into_iter()
        .find(|r| r.id == "position_check" && !r.ok)
        .map(|r| r.msg)
}

/// The mine-drift slide: the EKF says 0.3 m/s, the flow (at a dusty quality)
/// says 5.3 m/s forward.
#[test]
fn a_hold_the_flow_says_is_moving_is_not_a_calm_hold() {
    let mut w = hovering();
    w.vel = [0.3, 0.0, 0.0];
    run_flow(&mut w, [0.0, 2.8], 41, 2000);
    let s = w.p.state(w.now);
    let p = s.position_unverified.expect("position unverified");
    assert_eq!(p.reason, "flow_disagrees");
    assert_eq!(p.flow_mps, Some(5.3));
    assert_eq!(p.estimate_mps, Some(0.3));
    assert_eq!(s.hold_reason.as_deref(), Some("position-unverified"));
    assert_eq!(
        position_check_msg(&w).as_deref(),
        Some("Position unverified (nav): flow 5.3 m/s, estimate 0.3 m/s")
    );
    // The hold is a velocity brake, not a position hold: it asks minus the
    // flow-against-estimate gap (5.0 m/s north) held to the brake's cap,
    // so the ground speed the flow measures goes to zero.
    let sp = w.last_setpoint().unwrap();
    assert_eq!(sp.type_mask.bits(), crate::fc::MASK_VELOCITY);
    let cap = crate::core::position_check::flow_check::FLOW_BRAKE_MAX_MPS;
    assert!((sp.vx + cap).abs() < 1e-3 && sp.vy.abs() < 1e-3, "{sp:?}");
}

/// The FC's velocity response: its EKF velocity follows the asked velocity
/// with this time constant, s.
const FC_TAU_S: f32 = 0.25;

/// A frozen external position at 1.9 m, at yaw 0: the vehicle drifts north
/// at `v0` while the EKF says 0. `pinned` the EKF keeps leaking its
/// velocity back to 0 (the frozen position pulling it, time constant 1 s);
/// otherwise it stays off the ground velocity by `v0` throughout. Flow at
/// 10 Hz measures the ground velocity. Returns the ground velocities from
/// the trip on, 20 ms apart, and the distance flown after the trip.
fn frozen_drift(v0: f32, pinned: bool) -> (Vec<f32>, f32) {
    let mut w = hovering();
    let agl = w.agl.unwrap();
    let (mut est, mut ground) = (0.0f32, v0);
    let mut after = Vec::new();
    let mut flown = 0.0f32;
    for tick in 0..1000u32 {
        w.vel = [est, 0.0, 0.0];
        if tick % 5 == 0 {
            w.inbox.push(MavMessage::OPTICAL_FLOW(OPTICAL_FLOW_DATA {
                flow_comp_m_y: ground / agl,
                quality: 60,
                ground_distance: agl,
                ..Default::default()
            }));
        }
        w.run(20);
        let tripped = w.p.state(w.now).position_unverified.is_some();
        let ask = w
            .last_setpoint()
            .filter(|sp| sp.type_mask.bits() == crate::fc::MASK_VELOCITY)
            .map_or(0.0, |sp| sp.vx);
        let dt = 0.02;
        let accel = (ask - est) / FC_TAU_S;
        ground += accel * dt;
        est = if pinned {
            est + (accel - est) * dt
        } else {
            ground - v0
        };
        if tripped || !after.is_empty() {
            after.push(ground);
            flown += ground * dt;
        }
    }
    assert!(!after.is_empty(), "v0 {v0}: the flow check never tripped");
    (after, flown)
}

/// The frozen-pose slide: the EKF velocity pinned near zero while the flow
/// measures 1.1-3 m/s over the ground. The tripped hold brakes on the
/// flow, stopping within 3 m of the trip without hunting about zero.
#[test]
fn a_frozen_estimate_is_braked_to_a_stop_on_the_flow() {
    for pinned in [false, true] {
        for v0 in [1.1f32, 2.0, 3.0] {
            let (v, flown) = frozen_drift(v0, pinned);
            let end = *v.last().unwrap();
            let back = v.iter().copied().fold(f32::INFINITY, f32::min);
            // Sign changes of the ground velocity past a 0.05 m/s dead band.
            let mut flips = 0;
            let mut sign = 1.0f32;
            for &g in &v {
                if g * sign < -0.05 {
                    sign = -sign;
                    flips += 1;
                }
            }
            eprintln!(
                "v0 {v0} pinned {pinned}: {flown:.2} m after the trip, slowest {back:.2} m/s, end {end:.3} m/s, {flips} reversals"
            );
            assert!(flown.abs() <= 3.0, "v0 {v0}: {flown:.2} m after the trip");
            assert!(end.abs() < 0.1, "v0 {v0}: still {end:.2} m/s");
            assert!(flips <= 1, "v0 {v0}: {flips} reversals");
            assert!(back > -0.5, "v0 {v0}: slid back at {back:.2} m/s");
        }
    }
}

/// Quality-0 flow restarts the comparison: the brake asks nothing off the
/// flow it no longer has, and the hold asks zero on the estimate.
#[test]
fn the_flow_brake_lets_go_without_flow() {
    let mut w = hovering();
    w.vel = [0.3, 0.0, 0.0];
    run_flow(&mut w, [0.0, 2.8], 41, 2000);
    run_flow(&mut w, [0.0, 2.8], 0, 100);
    let sp = w.last_setpoint().unwrap();
    assert_eq!((sp.vx, sp.vy), (0.0, 0.0));
}

/// Flow and estimate agreeing, flying east-north-east at yaw 90°: forward
/// reads as +y flow, rightward as −x, so a correct estimate never trips.
#[test]
fn flow_matching_the_estimate_at_any_heading_is_verified() {
    let mut w = hovering();
    w.yaw = std::f32::consts::FRAC_PI_2;
    // 4 m/s forward (east) and 1 m/s right (south) at 1.9 m.
    w.vel = [-1.0, 4.0, 0.0];
    run_flow(&mut w, [-1.0 / 1.9, 4.0 / 1.9], 30, 4000);
    assert!(w.p.state(w.now).position_unverified.is_none());
    assert!(position_check_msg(&w).is_none());
}

#[test]
fn quality_zero_flow_is_no_measurement() {
    let mut w = hovering();
    run_flow(&mut w, [0.0, 2.8], 0, 3000);
    assert!(w.p.state(w.now).position_unverified.is_none());
}

#[test]
fn the_check_clears_once_flow_and_estimate_agree_again() {
    let mut w = hovering();
    run_flow(&mut w, [0.0, 2.8], 41, 2000);
    assert!(w.p.state(w.now).position_unverified.is_some());
    run_flow(&mut w, [0.0, 0.0], 41, 3500);
    assert!(w.p.state(w.now).position_unverified.is_none());
    assert_eq!(w.p.state(w.now).hold_reason, None);
}

pub(super) fn extnav_world() -> World {
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 6.0);
    w.gps_fix = Some(0);
    w.airborne(copter::GUIDED);
    w
}

fn nav_state(w: &mut World, health: &str, reason: Option<&str>) {
    w.p.on_feed(
        crate::events::TOPIC_NAV_STATE,
        &json!({"mode": "external_nav", "active_set": 1, "extnav_health": health,
                "health_reason": reason, "gps_fix": 0}),
        w.now,
    );
}

#[test]
fn a_lost_external_position_is_unverified_with_its_reason() {
    let mut w = extnav_world();
    nav_state(&mut w, "lost", Some("corridor_unmeasured"));
    let p = w.p.state(w.now).position_unverified.unwrap();
    assert_eq!(p.reason, "extnav_lost");
    assert_eq!(p.detail.as_deref(), Some("corridor_unmeasured"));
    assert_eq!(
        position_check_msg(&w).as_deref(),
        Some("Position unverified (nav): external position lost (Corridor, no measured speed)")
    );
    nav_state(&mut w, "degraded", Some("corridor_unmeasured"));
    let p = w.p.state(w.now).position_unverified.unwrap();
    assert_eq!(p.reason, "extnav_degraded");
    assert_eq!(p.detail.as_deref(), Some("corridor_unmeasured"));
    nav_state(&mut w, "degraded", Some("imu_feed_lost"));
    assert!(w.p.state(w.now).position_unverified.is_some());
    nav_state(&mut w, "degraded", None);
    assert!(w.p.state(w.now).position_unverified.is_none());
    nav_state(&mut w, "good", None);
    assert!(w.p.state(w.now).position_unverified.is_none());
}

/// The readiness line words Navigation's reason with the shared spec's label,
/// never the raw key.
#[test]
fn the_readiness_line_words_heading_and_imu_reasons() {
    let mut w = extnav_world();
    for (reason, label) in [
        ("heading_unconfirmed", "Heading unconfirmed"),
        ("imu_feed_attitude_stand_in", "IMU feed: attitude stand-in"),
        ("imu_feed_stalled", "IMU feed stalled"),
        ("imu_feed_starved", "IMU feed starved"),
    ] {
        nav_state(&mut w, "degraded", Some(reason));
        assert_eq!(
            position_check_msg(&w),
            Some(format!(
                "Position unverified (nav): external position degraded ({label})"
            ))
        );
    }
    // A reason the spec has no label for reads in words.
    nav_state(&mut w, "degraded", Some("imu_feed_lost"));
    assert_eq!(
        position_check_msg(&w).as_deref(),
        Some("Position unverified (nav): external position degraded (imu feed lost)")
    );
}

#[test]
fn a_lost_external_position_on_a_gps_source_set_is_not_the_estimate() {
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.airborne(copter::GUIDED);
    nav_state(&mut w, "lost", None);
    assert!(w.p.state(w.now).position_unverified.is_none());
}

#[test]
fn a_skill_on_an_unverified_position_loses_its_lease_and_brakes() {
    let mut w = extnav_world();
    nav_state(&mut w, "good", None);
    let id = w.lease();
    w.goal(id, 40.0, 0.0);
    w.run(500);
    assert_eq!(w.p.owner(), Owner::Skill);
    nav_state(&mut w, "lost", Some("ekf_flow_disagree"));
    w.run(100);
    assert_eq!(w.p.owner(), Owner::Pilot);
    assert!(w
        .revoked
        .iter()
        .any(|(l, r)| *l == id && r == "position_unverified"));
    let s = w.p.state(w.now);
    assert_eq!(s.hold_reason.as_deref(), Some("position-unverified"));
    assert_eq!(
        w.last_setpoint().unwrap().type_mask.bits(),
        crate::fc::MASK_VELOCITY
    );
}

#[test]
fn nothing_new_flies_on_an_unverified_position() {
    let mut w = extnav_world();
    w.action("take_control");
    nav_state(&mut w, "lost", None);
    w.req(Request::Goto {
        n: 40.0,
        e: 0.0,
        d: -4.0,
        yaw: None,
    });
    w.run(100);
    assert_eq!(w.p.owner(), Owner::Pilot, "goto refused");
    let lease = w.req(Request::LeaseAcquire {
        holder: ados_protocol::pilot::Holder {
            plugin_id: "com.example.follow".into(),
            skill_id: "follow-me".into(),
        },
        kind: "motion".into(),
    });
    match lease {
        super::super::Reply::Lease(l) => {
            assert_eq!(l.err.as_deref(), Some("position_unverified"))
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn stick_flight_on_an_unverified_position_is_slow() {
    let mut w = extnav_world();
    w.action("take_control");
    nav_state(&mut w, "lost", None);
    w.stick([0.0, 1.0, 0.0, 0.0], 2000);
    let sp = w.last_setpoint().unwrap();
    assert!(sp.vx.hypot(sp.vy) <= 1.0 + 1e-3, "{} {}", sp.vx, sp.vy);
}

fn refused(r: &super::super::Reply) -> Option<&str> {
    match r {
        super::super::Reply::Action(a) if !a.ok => a.err.as_deref(),
        _ => None,
    }
}

fn accepted(r: &super::super::Reply) -> bool {
    matches!(r, super::super::Reply::Action(a) if a.ok)
}

/// A mine corridor flown on good flow: Navigation reads `degraded` (the
/// scan cannot see the free axis) with no reason. The flow covers it, so the
/// skill keeps flying and a goto is taken.
#[test]
fn a_degraded_corridor_pose_with_no_reason_is_still_flown() {
    let mut w = extnav_world();
    w.agl = Some(1.9);
    nav_state(&mut w, "good", None);
    let id = w.lease();
    w.goal(id, 40.0, 0.0);
    w.run(300);
    assert_eq!(w.p.owner(), Owner::Skill);
    w.vel = [2.0, 0.0, 0.0];
    let end = w.now + 3000;
    while w.now < end {
        nav_state(&mut w, "degraded", None);
        w.goal(id, 40.0, 0.0);
        run_flow(&mut w, [0.0, 2.0 / 1.9], 60, 200);
    }
    let s = w.p.state(w.now);
    assert!(s.position_unverified.is_none());
    assert!(!w.revoked.iter().any(|(l, _)| *l == id), "{:?}", w.revoked);
    assert_eq!(w.p.owner(), Owner::Skill, "the skill keeps flying");
    assert_ne!(s.hold_reason.as_deref(), Some("position-unverified"));
    assert!(position_check_msg(&w).is_none());
}

/// Navigation going silent after a lost report does not clear the state on
/// an external-nav source set: stale counts as unknown.
#[test]
fn a_stale_nav_report_keeps_the_position_unverified() {
    let mut w = extnav_world();
    nav_state(&mut w, "lost", Some("corridor_unmeasured"));
    w.run(5_000);
    let p =
        w.p.state(w.now)
            .position_unverified
            .expect("still unverified");
    assert_eq!(p.reason, "extnav_stale");
    assert_eq!(
        position_check_msg(&w).as_deref(),
        Some("Position unverified (nav): no report from vision navigation")
    );
    nav_state(&mut w, "good", None);
    assert!(w.p.state(w.now).position_unverified.is_none());

    // On a GPS source set Navigation's silence says nothing of the position.
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 3.0);
    w.airborne(copter::GUIDED);
    nav_state(&mut w, "lost", None);
    w.run(5_000);
    assert!(w.p.state(w.now).position_unverified.is_none());
}

/// STOP, Land and RTH are never gated by an unverified position; Resume is.
#[test]
fn stop_land_and_rth_work_while_unverified_and_resume_is_refused() {
    for name in ["pause", "land", "rth"] {
        let mut w = extnav_world();
        w.action("take_control");
        nav_state(&mut w, "lost", None);
        w.run(100);
        assert!(accepted(&w.action(name)), "{name}");
    }
    let mut w = extnav_world();
    w.mission("active", 3, 8);
    w.run(100);
    w.action("pause");
    nav_state(&mut w, "lost", None);
    w.run(100);
    assert_eq!(refused(&w.action("resume")), Some("position_unverified"));
}

/// On the ground a launch is refused and arming is blocked by the check.
#[test]
fn launch_and_arm_are_refused_while_unverified() {
    let mut w = World::new();
    w.p.fc.params.insert("EK3_SRC1_POSXY".into(), 6.0);
    w.run(500);
    nav_state(&mut w, "lost", Some("corridor_unmeasured"));
    assert_eq!(
        refused(&w.action_args("launch", json!({"after": "hold", "alt_m": 4.0}))),
        Some("position_unverified")
    );
    assert!(position_check_msg(&w).is_some());
    assert!(refused(&w.action("arm")).is_some());
    assert!(!w.armed);
}

/// A seeded xorshift64* with Box-Muller normals, for flow noise.
struct Noise(u64);

impl Noise {
    fn new(seed: u64) -> Self {
        Self(
            seed.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(1)
                .max(1),
        )
    }
    /// Uniform in (0, 1].
    fn unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) + 1) as f64 / (1u64 << 53) as f64
    }
    fn normal(&mut self) -> f32 {
        let (u1, u2) = (self.unit(), self.unit());
        ((-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()) as f32
    }
}

/// One standard deviation of the flow rate's noise, rad/s, per axis and
/// sample: about 1.1 m/s at 1.9 m, a dusty floor at quality 20.
const DUSTY_SIGMA_RAD_S: f32 = 0.6;
/// The EKF velocity's lag behind the ground velocity, s.
const EKF_LAG_S: f32 = 0.3;

/// A flight north at yaw 0, 1.9 m up, `secs` long, the FC sending
/// OPTICAL_FLOW at 20 Hz (the agent's stream rate) with seeded white noise
/// at [`DUSTY_SIGMA_RAD_S`]. The ground velocity is `ground(t)`; the EKF's
/// follows it [`EKF_LAG_S`] behind, less `freeze.1` m/s from `freeze.0` s
/// on (a frozen external position holding it back). Returns when the check
/// first said the position was unverified, s.
fn dusty_flight(
    seed: u64,
    secs: f32,
    ground: impl Fn(f32) -> f32,
    freeze: Option<(f32, f32)>,
) -> Option<f32> {
    let mut w = hovering();
    let agl = w.agl.unwrap();
    let t0 = w.now;
    let mut noise = Noise::new(seed);
    let (mut est, mut sample) = (0.0f32, None);
    while w.now - t0 < (secs * 1000.0) as u64 {
        let t = (w.now - t0) as f32 / 1000.0;
        let g = ground(t);
        est += (g - est) * 0.02 / EKF_LAG_S;
        let held = freeze.filter(|(at, _)| t >= *at).map_or(0.0, |(_, v)| v);
        w.vel = [est - held, 0.0, 0.0];
        let bin = (w.now - t0) / 50;
        if sample != Some(bin) {
            sample = Some(bin);
            let comp_y = g / agl + DUSTY_SIGMA_RAD_S * noise.normal();
            let comp_x = DUSTY_SIGMA_RAD_S * noise.normal();
            w.inbox.push(MavMessage::OPTICAL_FLOW(OPTICAL_FLOW_DATA {
                flow_comp_m_x: comp_x,
                flow_comp_m_y: comp_y,
                quality: 20,
                ground_distance: agl,
                ..Default::default()
            }));
        }
        w.run(20);
        if w.p.position_unverified(w.now).is_some() {
            return Some((w.now - t0) as f32 / 1000.0);
        }
    }
    None
}

/// Stop, speed up at 1 m/s² to 1.6 m/s, cruise, slow down: a 7.6 s cycle.
fn stop_and_go(t: f32) -> f32 {
    match t % 7.6 {
        c if c < 2.0 => 0.0,
        c if c < 3.6 => c - 2.0,
        c if c < 6.0 => 1.6,
        c => 1.6 - (c - 6.0),
    }
}

/// Noisy flow over a dusty floor, its mean on the ground velocity, against
/// an estimate that is right but for its lag: never unverified. Each seed
/// tripped the bare 1.5 m threshold (noise alone moves the 3 s gap by about
/// 0.4 m per axis at 20 Hz, the lag through the speed changes by more).
#[test]
fn noisy_flow_around_a_correct_estimate_is_verified() {
    for seed in 1..=3 {
        let tripped = dusty_flight(seed, 30.0, stop_and_go, None);
        assert_eq!(tripped, None, "seed {seed}: unverified at {tripped:?} s");
    }
}

/// The same noisy flow at a steady 1.6 m/s, the estimate frozen 1 m/s
/// short of it from 10 s on: the check still catches it, within 3.5 s.
#[test]
fn a_frozen_estimate_is_caught_through_noisy_flow() {
    for seed in 1..=3 {
        let tripped = dusty_flight(seed, 20.0, |_| 1.6, Some((10.0, 1.0)));
        let at = tripped.unwrap_or_else(|| panic!("seed {seed}: never caught"));
        eprintln!("seed {seed}: caught {:.2} s after the freeze", at - 10.0);
        assert!((10.0..13.5).contains(&at), "seed {seed}: caught at {at} s");
    }
}
