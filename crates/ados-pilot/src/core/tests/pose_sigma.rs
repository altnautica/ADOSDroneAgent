//! The graded pose-sigma rule: over a metre and within `max_pose_sigma_m`
//! the position is uncertain and flown slowed; past the limit, or with a
//! `pose_sigma_wide` report that gives no sigma, it is unverified.

use ados_protocol::pilot::Owner;
use serde_json::json;

use super::harness::World;
use super::position_check::{extnav_world, run_flow};
use crate::core::nav_cap::uncertain_cap;

fn nav_sigma(w: &mut World, health: &str, reason: Option<&str>, sigma: Option<f32>) {
    w.p.on_feed(
        crate::events::TOPIC_NAV_STATE,
        &json!({"mode": "external_nav", "active_set": 1, "extnav_health": health,
                "health_reason": reason, "gps_fix": 0, "pose_sigma_m": sigma}),
        w.now,
    );
}

/// A skill flying a corridor at 2 m/s on a good pose, then 3 s of
/// `degraded`/`pose_sigma_wide` reports carrying `sigma`; returns the world
/// and the skill's lease.
fn corridor(sigma: Option<f32>) -> (World, u64) {
    let mut w = extnav_world();
    w.agl = Some(1.9);
    nav_sigma(&mut w, "good", None, Some(0.3));
    let id = w.lease();
    w.goal(id, 40.0, 0.0);
    w.run(300);
    assert_eq!(w.p.owner(), Owner::Skill);
    w.vel = [2.0, 0.0, 0.0];
    let end = w.now + 3000;
    while w.now < end {
        nav_sigma(&mut w, "degraded", Some("pose_sigma_wide"), sigma);
        w.goal(id, 40.0, 0.0);
        run_flow(&mut w, [0.0, 2.0 / 1.9], 60, 200);
    }
    (w, id)
}

/// 2.8 m after a 135 m corridor: the skill keeps flying, held to the
/// uncertain-position cap, and the state says why.
#[test]
fn a_wide_sigma_within_the_limit_is_flown_slowed() {
    let (w, id) = corridor(Some(2.8));
    let s = w.p.state(w.now);
    assert!(
        s.position_unverified.is_none(),
        "{:?}",
        s.position_unverified
    );
    assert_eq!(w.p.owner(), Owner::Skill);
    assert!(!w.revoked.iter().any(|(l, _)| *l == id), "{:?}", w.revoked);
    assert_eq!(s.pose_uncertainty_m, Some(2.8));
    let cap = uncertain_cap(2.8, 5.0, 2.0);
    assert!((cap - 1.55).abs() < 1e-3, "{cap}");
    let sc = s.speed_cap.expect("speed cap");
    assert_eq!(sc.reason, "position-uncertain");
    assert!((sc.v_h_mps - cap).abs() < 1e-3, "{sc:?}");
    let c = s.command.expect("command");
    assert!(c.v_cap_mps <= f64::from(cap) + 1e-3, "{}", c.v_cap_mps);
    assert_eq!(
        c.limits.first().map(String::as_str),
        Some("position_uncertain"),
        "{:?}",
        c.limits
    );
}

/// Past `max_pose_sigma_m` the position is unverified: the skill stops.
#[test]
fn a_sigma_past_the_limit_is_unverified() {
    let (w, id) = corridor(Some(5.5));
    let s = w.p.state(w.now);
    let p = s.position_unverified.expect("unverified");
    assert_eq!(p.reason, "extnav_degraded");
    assert_eq!(p.detail.as_deref(), Some("pose_sigma_wide"));
    assert_eq!(s.pose_uncertainty_m, None);
    assert!(w.revoked.iter().any(|(l, _)| *l == id), "{:?}", w.revoked);
}

/// A `pose_sigma_wide` report with no sigma cannot be sized: it is
/// unverified, as every wide sigma was before the limit existed.
#[test]
fn a_wide_sigma_report_without_its_sigma_is_unverified() {
    let (w, id) = corridor(None);
    let s = w.p.state(w.now);
    assert!(s.position_unverified.is_some());
    assert_eq!(s.pose_uncertainty_m, None);
    assert!(w.revoked.iter().any(|(l, _)| *l == id), "{:?}", w.revoked);
}

/// The limit is the operator's: a 7 m limit flies a 5.5 m sigma.
#[test]
fn the_limit_is_configurable() {
    let mut w = extnav_world();
    w.p.settings.pilot.max_pose_sigma_m = 7.0;
    nav_sigma(&mut w, "degraded", Some("pose_sigma_wide"), Some(5.5));
    let s = w.p.state(w.now);
    assert!(s.position_unverified.is_none());
    assert_eq!(s.pose_uncertainty_m, Some(5.5));
}

/// The cap starts at the flat GNSS-denied speed (at most 3 m/s), never
/// rises with the sigma, and meets the unverified 1 m/s at the limit.
#[test]
fn the_cap_falls_from_the_flat_cap_to_the_unverified_speed() {
    assert!((uncertain_cap(1.0, 5.0, 2.0) - 2.0).abs() < 1e-6);
    assert!((uncertain_cap(5.0, 5.0, 2.0) - 1.0).abs() < 1e-6);
    assert!((uncertain_cap(1.0, 5.0, 8.0) - 3.0).abs() < 1e-6);
    assert!((uncertain_cap(3.0, 5.0, 0.5) - 0.5).abs() < 1e-6);
    let mut last = f32::INFINITY;
    for i in 0..=40 {
        let v = uncertain_cap(1.0 + i as f32 * 0.1, 5.0, 2.0);
        assert!(v <= last && v <= 2.0, "{v}");
        last = v;
    }
}
