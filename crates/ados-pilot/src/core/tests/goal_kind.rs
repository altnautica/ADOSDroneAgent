//! The published goal kind and view: which goal the skill's lease sent,
//! and, for a `follow_intent`, the viewpoint the view planner picked.

use ados_protocol::pilot::{Goal, GoalKind, Request};

use super::shaping::{fly_for, flying};

/// An intent round `subject`: 5-7 m off it, 4-5 m over the ground, the
/// bearing left to the pilot.
fn intent(subject: [f32; 2]) -> Goal {
    let mut g = Goal::new(
        GoalKind::FollowIntent,
        [subject[0] - 6.0, subject[1], -5.0],
        4.0,
    );
    g.subject_ne = Some(subject);
    g.standoff_band_m = Some([5.0, 7.0]);
    g.height_band_m = Some([4.0, 5.0]);
    g
}

#[test]
fn the_state_names_the_goal_kind_and_the_view_picked() {
    let (mut w, id) = flying();
    // No goal yet: no kind.
    w.run(100);
    let s = w.p.state(w.now);
    assert_eq!((s.goal_kind, s.view), (None, None));

    // The skill's own point: its kind, no view.
    fly_for(&mut w, id, 500, |_, _| {
        Goal::new(GoalKind::Orbit, [3.0, 0.0, -5.0], 4.0)
    });
    let s = w.p.state(w.now);
    assert_eq!(s.goal_kind, Some(GoalKind::Orbit));
    assert_eq!(s.view, None);

    // An intent: the kind and the viewpoint picked inside its bands.
    let subject = [w.pos[0] + 20.0, w.pos[1]];
    fly_for(&mut w, id, 1000, |_, _| intent(subject));
    let s = w.p.state(w.now);
    assert_eq!(s.goal_kind, Some(GoalKind::FollowIntent));
    let v = s.view.expect("a view picked");
    assert!((5.0..=7.0).contains(&v.radius_m), "{v:?}");
    assert!((0.0..360.0).contains(&v.bearing_deg), "{v:?}");
    assert!((0.0..=1.0).contains(&v.visibility), "{v:?}");
    assert!(v.height_m >= 3.4, "{v:?}");
    let frame = serde_json::to_value(&s).unwrap();
    assert_eq!(frame["goal_kind"], "follow_intent");
    assert!(frame["view"]["radius_m"].is_number(), "{frame}");

    // The lease given back: no kind, no view.
    w.req(Request::LeaseRelease { lease_id: id });
    w.run(100);
    let s = w.p.state(w.now);
    assert_eq!((s.goal_kind, s.view), (None, None));
}
