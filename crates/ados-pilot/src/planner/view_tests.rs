use super::*;

/// Trunks as vertical disks from the ground up.
struct Disks(Vec<(V2, f32)>);

impl Sight for Disks {
    fn clearance(&self, ne: V2, _d: f32) -> f32 {
        self.0
            .iter()
            .map(|&(c, r)| geo::dist(ne, c) - r)
            .fold(f32::INFINITY, f32::min)
    }

    fn sees(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        let (a, b) = ([from[0], from[1]], [to[0], to[1]]);
        self.0.iter().all(|&(c, r)| {
            let ab = geo::sub(b, a);
            let l2 = geo::dot(ab, ab).max(1e-9);
            let t = (geo::dot(geo::sub(c, a), ab) / l2).clamp(0.0, 1.0);
            geo::dist(geo::add(a, geo::scale(ab, t)), c) > r
        })
    }

    fn seen(&self, _from: [f32; 3], _to: [f32; 3]) -> bool {
        true
    }
}

/// [`Disks`] seen only within `.1` m of the vehicle.
struct Fogged(Disks, f32);

impl Sight for Fogged {
    fn clearance(&self, ne: V2, d: f32) -> f32 {
        self.0.clearance(ne, d)
    }

    fn sees(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        self.0.sees(from, to)
    }

    fn seen(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        geo::dist([from[0], from[1]], [to[0], to[1]]) <= self.1
    }
}

/// A subject at `ne` walking north at `v` m/s, predicted on in a line.
fn walking(ne: V2, v: f32) -> SubjectView {
    let preds = [0.5f32, 1.0, 1.5, 2.0, 3.0]
        .map(|t| Prediction {
            dt_s: t,
            ne: [ne[0] + v * t, ne[1]],
        })
        .to_vec();
    SubjectView {
        ne,
        vel: [v, 0.0],
        ground_d: Some(0.0),
        height_m: 1.75,
        preds,
        turn_prob: 0.0,
        unseen_s: 0.0,
    }
}

fn ask(pref_deg: Option<f32>) -> ViewAsk {
    ViewAsk {
        band: [5.0, 7.0],
        bearing_pref: pref_deg.map(f32::to_radians),
        bearing_world: false,
        heights: [4.0, 5.0],
        d_pref: -4.0,
        ground_d: Some(0.0),
        margin: 1.5,
        require_los: false,
    }
}

fn bearing_deg(c: &ViewChoice) -> f32 {
    c.bearing.to_degrees()
}

#[test]
fn behind_a_walking_subject_in_the_open() {
    let mut v = ViewPlanner::new();
    let s = walking([10.0, 0.0], 1.4);
    let c = v
        .choose(&s, &ask(Some(180.0)), [4.0, 0.0, -4.0], &Disks(vec![]), 0)
        .expect("a view");
    assert!((bearing_deg(&c).abs() - 180.0).abs() < 1.0, "{c:?}");
    assert!((c.radius - 6.0).abs() < 1e-3, "{c:?}");
    assert!((c.d + 4.0).abs() < 1e-3, "kept the skill's height: {c:?}");
    assert_eq!(c.visibility, 1.0);
    assert!(!c.lost);
}

#[test]
fn a_viewpoint_past_the_seen_space_is_half_credited() {
    let s = walking([10.0, 0.0], 1.4);
    // Seen 3 m round the vehicle: every viewpoint lies past the edge.
    let fog = Fogged(Disks(vec![]), 3.0);
    let c = ViewPlanner::new()
        .choose(&s, &ask(Some(180.0)), [-4.0, 0.0, -4.0], &fog, 0)
        .expect("a view");
    assert!((c.visibility - UNSEEN_VIS).abs() < 1e-5, "{c:?}");
    // Auto: the viewpoint inside the seen space wins over every one past it.
    let fog = Fogged(Disks(vec![]), 8.0);
    let c = ViewPlanner::new()
        .choose(&s, &ask(None), [4.0, 5.0, -4.0], &fog, 0)
        .expect("a view");
    assert!(geo::dist(c.ne, [4.0, 5.0]) <= 8.0, "{c:?}");
    assert_eq!(c.visibility, 1.0, "{c:?}");
}

/// Full-height wall `a` → `b` as touching 0.3 m disks.
fn wall(out: &mut Vec<(V2, f32)>, a: V2, b: V2) {
    let n = (geo::dist(a, b) / 0.4).ceil() as usize;
    for k in 0..=n {
        out.push((
            geo::add(a, geo::scale(geo::sub(b, a), k as f32 / n as f32)),
            0.3,
        ));
    }
}

#[test]
fn round_a_t_the_view_goes_where_the_way_is_open() {
    // A 5 m corridor north ending in a 5 m cross corridor; the walker
    // turned east out of the vehicle's sight a second ago.
    let mut w = Vec::new();
    wall(&mut w, [-6.0, -2.5], [27.5, -2.5]);
    wall(&mut w, [-6.0, 2.5], [27.5, 2.5]);
    wall(&mut w, [27.5, 2.5], [27.5, 30.0]);
    wall(&mut w, [27.5, -2.5], [27.5, -20.0]);
    wall(&mut w, [32.5, -20.0], [32.5, 30.0]);
    let mut s = walking([30.2, 4.0], 0.0);
    s.vel = [0.0, 1.4];
    s.unseen_s = 1.0;
    let sight = Disks(w);
    let mut v = ViewPlanner::new();
    let c = v
        .choose(&s, &ask(None), [21.7, 0.3, -4.0], &sight, 0)
        .expect("a view");
    assert_eq!(c.visibility, 1.0, "{c:?}");
    assert!(
        sight.sees([21.7, 0.3, -4.0], [c.ne[0], c.ne[1], -4.0]),
        "{c:?}"
    );
}

#[test]
fn a_trunk_behind_the_subject_moves_the_view_round_it() {
    // A 0.4 m trunk 3 m behind a walker going north hides it from the
    // viewpoint straight behind, for every prediction.
    let trunk = Disks(vec![([7.0, 0.0], 0.4)]);
    let s = walking([10.0, 0.0], 0.0);
    let mut v = ViewPlanner::new();
    let c = v
        .choose(&s, &ask(Some(180.0)), [4.0, 0.0, -4.0], &trunk, 0)
        .expect("a view");
    assert_eq!(c.visibility, 1.0, "{c:?}");
    let off = 180.0 - bearing_deg(&c).abs();
    assert!((10.0..=50.0).contains(&off), "{off}° off behind: {c:?}");
    // Auto bearing: the nearest view that sees.
    let mut v = ViewPlanner::new();
    let c = v
        .choose(&s, &ask(None), [4.0, 1.0, -4.0], &trunk, 0)
        .expect("a view");
    assert_eq!(c.visibility, 1.0, "{c:?}");
    assert!(geo::dist(c.ne, [4.0, 1.0]) < 3.0, "{c:?}");
}

#[test]
fn a_viewpoint_inside_the_margin_is_never_picked() {
    // Trunks on every behind slot but one, 15° off.
    let s = walking([10.0, 0.0], 0.0);
    let mut trunks = Vec::new();
    for deg in [-165.0f32, 180.0, 165.0, 150.0] {
        for r in [5.0f32, 6.0, 7.0] {
            let u = geo::bearing_unit(deg.to_radians());
            trunks.push((geo::add([10.0, 0.0], geo::scale(u, r)), 0.3));
        }
    }
    let sight = Disks(trunks);
    let mut v = ViewPlanner::new();
    let c = v
        .choose(&s, &ask(Some(180.0)), [4.0, 0.0, -4.0], &sight, 0)
        .expect("a view");
    assert!(sight.clearance(c.ne, c.d) >= 1.5, "{c:?}");
}

#[test]
fn heights_keep_the_floor_and_the_band() {
    let s = walking([10.0, 0.0], 1.4);
    let mut a = ask(Some(180.0));
    a.heights = [2.0, 6.0];
    a.d_pref = -1.0;
    let mut v = ViewPlanner::new();
    let c = v
        .choose(&s, &a, [4.0, 0.0, -4.0], &Disks(vec![]), 0)
        .expect("a view");
    assert!(
        (-6.0..=-FOLLOW_HEIGHT_FLOOR_M).contains(&c.d),
        "down {} outside the band over the floor",
        c.d
    );
}

#[test]
fn the_heading_turns_at_its_rate_and_round_the_vehicles_side() {
    let mut v = ViewPlanner::new();
    let a = ask(Some(180.0));
    let s = walking([10.0, 0.0], 1.4);
    v.choose(&s, &a, [4.0, 1.0, -4.0], &Disks(vec![]), 0);
    assert_eq!(v.heading(), Some(0.0));
    // The subject turns back toward the vehicle: the heading swings at
    // 30°/s, the viewpoint passing the vehicle's side (east, +1 m).
    let mut back = walking([10.0, 0.0], -1.4);
    back.vel = [-1.4, 0.0];
    let c = v
        .choose(&back, &a, [4.0, 1.0, -4.0], &Disks(vec![]), 100)
        .expect("a view");
    let h = v.heading().expect("a heading").to_degrees();
    assert!((h.abs() - 3.0).abs() < 0.1, "turned {h}° in 0.1 s");
    // The behind slot moves toward the east (the vehicle's side).
    assert!(c.ne[1] > -0.1, "{c:?}");
    for k in 2..=70 {
        v.choose(&back, &a, [4.0, 1.0, -4.0], &Disks(vec![]), 100 * k);
    }
    let h = v.heading().expect("a heading").to_degrees();
    assert!((h.abs() - 180.0).abs() < 1.0, "heading {h}° after 7 s");
}

#[test]
fn a_predicted_turn_turns_the_heading_early() {
    let mut s = walking([10.0, 0.0], 1.4);
    // Turning east: the predictions curve round.
    for p in s.preds.iter_mut() {
        p.ne = [10.0 + 0.7 * p.dt_s, 1.2 * p.dt_s];
    }
    s.turn_prob = 0.6;
    let mut v = ViewPlanner::new();
    v.choose(&s, &ask(Some(180.0)), [4.0, 0.0, -4.0], &Disks(vec![]), 0);
    let h = v.heading().expect("a heading").to_degrees();
    assert!(h > 45.0, "heading {h}° along the turn");
}

#[test]
fn lost_looks_only_where_the_subject_is_most_probably_now() {
    // The predictions walk behind a wall of trunks; the subject now does
    // not: a lost view scores only the latter.
    let mut s = walking([10.0, 0.0], 1.4);
    s.unseen_s = 1.0;
    let wall: Vec<(V2, f32)> = (0..10)
        .map(|k| ([13.0, -2.0 + 0.5 * k as f32], 0.3))
        .collect();
    let mut v = ViewPlanner::new();
    let c = v
        .choose(&s, &ask(Some(180.0)), [4.0, 0.0, -4.0], &Disks(wall), 0)
        .expect("a view");
    assert!(c.lost);
    assert_eq!(c.visibility, 1.0, "{c:?}");
}

#[test]
fn a_plan_that_lost_the_subject_moves_the_view() {
    // A trunk hides the subject from the slots 15° east of behind; the
    // first view (straight behind) sees it, but its planned trajectory did
    // not.
    let s = walking([10.0, 0.0], 0.0);
    let trunk = geo::add(
        [10.0, 0.0],
        geo::scale(geo::bearing_unit(165f32.to_radians()), 3.0),
    );
    let sight = Disks(vec![(trunk, 0.4)]);
    let a = ask(None);
    let mut v = ViewPlanner::new();
    let first = v
        .choose(&s, &a, [4.0, 0.0, -4.0], &sight, 0)
        .expect("a view");
    assert_eq!(first.visibility, 1.0, "{first:?}");
    v.note_plan_visibility(0.0);
    let next = v
        .choose(&s, &a, [4.0, 0.0, -4.0], &sight, 100)
        .expect("a view");
    assert_ne!((first.ne, first.d), (next.ne, next.d));
    // The new view sees the subject ...
    assert_eq!(next.visibility, 1.0, "{next:?}");
    assert!(
        sees_subject(
            &sight,
            &s,
            s.ne,
            [next.ne[0], next.ne[1], next.d],
            a.ground_d
        ),
        "{next:?}"
    );
    // ... and scores better than the first one held with its loss.
    let held_lost = first.cost - HOLD_BONUS + W_VIS;
    assert!(next.cost < held_lost, "{} vs {held_lost}", next.cost);
}

/// The skill's orbit phase after `dt` s: on at `rate` (rad/s, clockwise)
/// unless it leads the vehicle's bearing from the centre by more than 45°,
/// held then (follow-me's lead rule).
fn advance_orbit_phase(phase: f32, centre: V2, vehicle: V2, rate: f32, dt: f32) -> f32 {
    let here = geo::bearing_of(geo::sub(vehicle, centre));
    if geo::wrap_pi(phase - here) > 45f32.to_radians() {
        return phase;
    }
    geo::wrap_pi(phase + rate * dt)
}

#[test]
fn a_world_bearing_orbit_goes_all_the_way_round() {
    // A standing subject, the skill's phase at 10°/s under its lead rule,
    // the vehicle flying at up to 3 m/s to each viewpoint.
    let s = walking([10.0, 0.0], 0.0);
    let mut a = ask(None);
    a.bearing_world = true;
    let mut veh = [4.0f32, 0.0];
    let mut phase = std::f32::consts::PI;
    let mut v = ViewPlanner::new();
    let start = geo::bearing_of(geo::sub(veh, s.ne));
    let (mut last, mut turned) = (start, 0.0f32);
    let mut t_ms = 0;
    while turned < std::f32::consts::TAU && t_ms < 60_000 {
        a.bearing_pref = Some(phase);
        let c = v
            .choose(&s, &a, [veh[0], veh[1], -4.0], &Disks(vec![]), t_ms)
            .expect("a view");
        let to = geo::sub(c.ne, veh);
        let len = geo::norm(to);
        if len > 0.0 {
            veh = geo::add(veh, geo::scale(to, len.min(0.3) / len));
        }
        let r = geo::dist(veh, s.ne);
        assert!(r >= 4.5, "{r} m from the subject at {t_ms} ms");
        let b = geo::bearing_of(geo::sub(veh, s.ne));
        turned += geo::wrap_pi(b - last);
        last = b;
        phase = advance_orbit_phase(phase, s.ne, veh, 10f32.to_radians(), 0.1);
        t_ms += VIEW_PERIOD_MS;
    }
    // 36 s at the orbit rate; the lead rule may hold the phase a little.
    assert!(
        turned >= std::f32::consts::TAU,
        "turned {:.0}° in {t_ms} ms",
        turned.to_degrees()
    );
    assert!(t_ms <= 45_000, "a full turn took {t_ms} ms");
}

#[test]
fn predictions_interpolate_then_extrapolate() {
    let s = walking([0.0, 0.0], 2.0);
    assert!((s.at(0.25)[0] - 0.5).abs() < 1e-4);
    assert!((s.at(1.25)[0] - 2.5).abs() < 1e-4);
    assert!((s.at(5.0)[0] - 10.0).abs() < 1e-4);
}
