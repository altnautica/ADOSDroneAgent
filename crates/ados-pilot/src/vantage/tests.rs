use super::*;

fn obs_with(points: &[V2], center: V2) -> Obstacles {
    let mut o = Obstacles::new();
    o.grid.reset(center);
    for p in points {
        o.grid.mark(*p);
    }
    o.grid.transform();
    o
}

fn disc(c: V2, r: f32) -> Vec<V2> {
    let mut v = Vec::new();
    let n = (r / 0.1).ceil() as i32;
    for i in -n..=n {
        for j in -n..=n {
            let p = [c[0] + i as f32 * 0.1, c[1] + j as f32 * 0.1];
            if (p[0] - c[0]).hypot(p[1] - c[1]) <= r {
                v.push(p);
            }
        }
    }
    v
}

fn spec(pred: V2) -> VantageSpec {
    VantageSpec::new(pred, [0.5, 0.0, 0.5], 1.0, 5.0, 12.0, [0.0, 0.0])
}

#[test]
fn a_trunk_hiding_the_prediction_gives_a_side_vantage_with_line_of_sight() {
    // Drone at the origin, trunk 4 m north, subject predicted 7 m north behind it.
    let o = obs_with(&disc([4.0, 0.0], 0.5), [0.0, 0.0]);
    assert!(!o.line_of_sight([0.0, 0.0], [7.0, 0.0]));
    let c = choose(&o, &spec([7.0, 0.0]), [0.0, 0.0], None, 1.5, 2.0);
    let p = c.point.expect("a vantage");
    assert!(o.line_of_sight(p, [7.0, 0.0]));
    assert!(p[1].abs() > 1.0, "moved to the trunk's side: {p:?}");
    let occ = c.occluder.expect("the trunk");
    assert!(
        occ.width_m < NARROW_OCCLUDER_M && (occ.ne[0] - 4.0).abs() < 1.0,
        "{occ:?}"
    );
}

#[test]
fn a_wall_corner_gives_a_vantage_past_the_corner_at_a_safe_offset() {
    // Wall along e = 2 from n = -10 to n = 6; subject predicted round the corner at (8, 6).
    let wall: Vec<V2> = (0..160).map(|k| [-10.0 + k as f32 * 0.1, 2.0]).collect();
    let o = obs_with(&wall, [0.0, 0.0]);
    let c = choose(&o, &spec([8.0, 6.0]), [0.0, 0.0], None, 1.5, 2.0);
    let p = c.point.expect("a vantage");
    assert!(o.line_of_sight(p, [8.0, 6.0]));
    assert!(o.grid.clearance(p).unwrap_or(f32::INFINITY) >= 1.5);
    assert!(
        c.occluder.expect("the wall").width_m >= NARROW_OCCLUDER_M,
        "a wall is wide"
    );
}

#[test]
fn a_prediction_inside_an_obstacle_is_snapped_to_free_space() {
    let o = obs_with(&disc([7.0, 0.0], 0.6), [0.0, 0.0]);
    let s = snap_free(&o.grid, [7.0, 0.0], 0.3, 2.0);
    assert!(o.grid.clearance(s).unwrap_or(f32::INFINITY) >= 0.3);
    assert!((s[0] - 7.0).hypot(s[1]) <= 2.0);
}

#[test]
fn vantage_beyond_the_search_radius_is_refused() {
    let o = obs_with(&disc([4.0, 0.0], 0.5), [0.0, 0.0]);
    let mut s = spec([7.0, 0.0]);
    s.max_radius_m = 0.5;
    assert!(choose(&o, &s, [0.0, 0.0], None, 1.5, 2.0).point.is_none());
}

#[test]
fn clear_line_of_sight_keeps_the_current_position_as_the_vantage() {
    let o = obs_with(&[], [0.0, 0.0]);
    let c = choose(&o, &spec([7.0, 0.0]), [0.0, 0.0], None, 1.5, 2.0);
    assert_eq!(c.point, Some([0.0, 0.0]));
    assert!(c.coverage > 0.99);
}

#[test]
fn a_subject_stopped_behind_the_trunk_is_covered() {
    // Predicted 7 m north in the open; the subject may also have stopped
    // just behind the trunk at (5.5, 0), which the origin cannot see.
    let o = obs_with(&disc([4.0, 0.0], 0.5), [0.0, 0.0]);
    let mut s = spec([2.0, 5.0]);
    s.hypotheses_ne = vec![[5.5, 0.0]];
    assert!(o.line_of_sight([0.0, 0.0], [2.0, 5.0]));
    assert!(!o.line_of_sight([0.0, 0.0], [5.5, 0.0]));
    let c = choose(&o, &s, [0.0, 0.0], None, 1.5, 2.0);
    let p = c.point.expect("a vantage");
    assert_ne!(p, [0.0, 0.0], "the origin misses the stop hypothesis");
    assert!(o.line_of_sight(p, [5.5, 0.0]), "sees the stop: {p:?}");
}

#[test]
fn a_doorway_gives_a_vantage_in_line_with_the_opening() {
    // Wall along n = 4 with a 1.2 m door at e = 0, too narrow to fly through
    // at a 1.5 m margin; subject predicted beyond it at (9, 0).
    let wall: Vec<V2> = (0..200)
        .map(|k| [4.0, -10.0 + k as f32 * 0.1])
        .filter(|p| p[1].abs() > 0.6)
        .collect();
    let o = obs_with(&wall, [0.0, -3.0]);
    let from = [0.0, -3.0];
    let mut s = spec([9.0, 0.0]);
    s.anchor_ne = from;
    s.cov_ne = [0.05, 0.0, 0.05];
    let c = choose(&o, &s, from, None, 1.5, 2.0);
    let p = c.point.expect("a vantage");
    assert!(p[0] < 4.0, "stays on the near side: {p:?}");
    assert!(o.line_of_sight(p, [9.0, 0.0]), "{p:?}");
    let t = (4.0 - p[0]) / (9.0 - p[0]);
    let e_at_wall = p[1] + t * (0.0 - p[1]);
    assert!(e_at_wall.abs() < 0.6, "looks through the door: {p:?}");
}

#[test]
fn a_vantage_that_fills_up_is_replaced_on_the_next_replan() {
    let trunk = disc([4.0, 0.0], 0.5);
    let o = obs_with(&trunk, [0.0, 0.0]);
    let first = choose(&o, &spec([7.0, 0.0]), [0.0, 0.0], None, 1.5, 2.0)
        .point
        .expect("a vantage");
    // A parked car appears on the chosen point.
    let mut pts = trunk.clone();
    pts.extend(disc(first, 0.8));
    let o = obs_with(&pts, [0.0, 0.0]);
    let next = choose(&o, &spec([7.0, 0.0]), [0.0, 0.0], None, 1.5, 2.0)
        .point
        .expect("another vantage");
    assert!(geo::dist(first, next) > 1.0, "{first:?} -> {next:?}");
    assert!(o.grid.clearance(next).unwrap_or(f32::INFINITY) >= 1.5);
}

#[test]
fn a_prediction_off_the_grid_still_gets_a_vantage_toward_it() {
    // The grid spans 16 m each side; the subject is predicted 30 m north,
    // hidden by a trunk 4 m north.
    let o = obs_with(&disc([4.0, 0.0], 0.5), [0.0, 0.0]);
    let mut s = spec([30.0, 0.0]);
    s.max_radius_m = 14.0;
    let c = choose(&o, &s, [0.0, 0.0], None, 1.5, 2.0);
    let p = c.point.expect("a vantage");
    assert!(p[0] > 2.0, "toward the prediction: {p:?}");
    assert!(o.line_of_sight(p, [30.0, 0.0]));
}

#[test]
fn a_partial_view_with_no_better_point_keeps_the_current_position() {
    // The prediction is in the open; a hypothesis hides behind a trunk to
    // the east. The search bound leaves no ring point, so the vehicle stays
    // where it sees most of the area rather than reporting no view.
    let o = obs_with(&disc([0.0, 4.0], 0.5), [0.0, 0.0]);
    let mut s = spec([7.0, 0.0]);
    s.hypotheses_ne = vec![[0.0, 7.0]];
    s.max_radius_m = 0.5;
    assert!(!o.line_of_sight([0.0, 0.0], [0.0, 7.0]));
    let c = choose(&o, &s, [0.0, 0.0], None, 1.5, 2.0);
    assert_eq!(c.point, Some([0.0, 0.0]));
    assert!(c.coverage > 0.8 && c.coverage < 1.0, "{}", c.coverage);
}

#[test]
fn a_bound_that_is_not_a_number_refuses_every_point() {
    let o = obs_with(&disc([4.0, 0.0], 0.5), [0.0, 0.0]);
    let mut s = spec([7.0, 0.0]);
    s.max_radius_m = f32::NAN;
    assert!(choose(&o, &s, [0.0, 0.0], None, 1.5, 2.0).point.is_none());
    let s = spec([7.0, 0.0]);
    let fence = Some(([0.0, 0.0], f32::NAN));
    assert!(choose(&o, &s, [0.0, 0.0], fence, 1.5, 2.0).point.is_none());
}

#[test]
fn a_vantage_outside_the_geofence_is_refused() {
    // The trunk hides the prediction from the vehicle at home; every point
    // that sees past it lies more than 1 m from home, past the fence.
    let o = obs_with(&disc([4.0, 0.0], 0.5), [0.0, 0.0]);
    let s = spec([7.0, 0.0]);
    let open = choose(&o, &s, [0.0, 0.0], None, 1.5, 2.0);
    let p = open.point.expect("a vantage without the fence");
    assert!(geo::dist(p, [0.0, 0.0]) > 1.0, "{p:?}");
    let fenced = choose(&o, &s, [0.0, 0.0], Some(([0.0, 0.0], 1.0)), 1.5, 2.0);
    assert!(fenced.point.is_none(), "{:?}", fenced.point);
    // A fence wide enough keeps every chosen point inside it.
    let wide = choose(&o, &s, [0.0, 0.0], Some(([0.0, 0.0], 8.0)), 1.5, 2.0);
    let q = wide.point.expect("a vantage inside an 8 m fence");
    assert!(geo::dist(q, [0.0, 0.0]) <= 8.0, "{q:?}");
}
