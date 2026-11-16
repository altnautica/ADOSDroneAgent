use super::*;

const DT: f32 = 0.02;

fn normal() -> Limits {
    limits(VehicleClass::Multirotor, Profile::Normal)
}

/// Run `s` toward `target` for `n` ticks; the largest acceleration while
/// speeding up and while slowing, and the largest jerk.
fn peaks(s: &mut Shaper, target: V2, lim: &Limits, n: usize) -> (f32, f32, f32) {
    let (mut up, mut down, mut jerk) = (0.0f32, 0.0f32, 0.0f32);
    let mut prev_a = s.a;
    for _ in 0..n {
        let v0 = s.v;
        s.track_ab(target, lim.a_acc, lim.a_brk, lim.jerk, DT);
        let a = geo::scale(geo::sub(s.v, v0), 1.0 / DT);
        if geo::norm(s.v) >= geo::norm(v0) {
            up = up.max(geo::norm(a));
        } else {
            down = down.max(geo::norm(a));
        }
        jerk = jerk.max(geo::norm(geo::sub(a, prev_a)) / DT);
        prev_a = a;
    }
    (up, down, jerk)
}

#[test]
fn a_step_speeds_up_on_the_accelerate_limit_and_settles_without_a_snap() {
    let lim = normal();
    let mut s = Shaper::default();
    let (up, _, jerk) = peaks(&mut s, [lim.v_h, 0.0], &lim, 500);
    assert!(up <= lim.a_acc + 1e-3, "accel {up}");
    assert!(jerk <= lim.jerk + 1e-2, "jerk {jerk}");
    assert!((s.v[0] - lim.v_h).abs() < 1e-3, "settled at {}", s.v[0]);
    assert!(s.v[0] <= lim.v_h + 1e-3, "no overshoot");
}

#[test]
fn a_release_brakes_on_the_brake_limit_to_rest_without_reversing() {
    let lim = normal();
    let mut s = Shaper::default();
    peaks(&mut s, [lim.v_h, 0.0], &lim, 500);
    let (_, down, jerk) = peaks(&mut s, [0.0, 0.0], &lim, 300);
    assert!(down <= lim.a_brk + 1e-3, "brake {down}");
    assert!(jerk <= lim.jerk + 1e-2, "jerk {jerk}");
    assert_eq!(s.v, [0.0, 0.0], "at rest");
    assert_eq!(s.a, [0.0, 0.0]);
    // Never reverses on the way.
    let mut s = Shaper::default();
    peaks(&mut s, [lim.v_h, 0.0], &lim, 500);
    for _ in 0..300 {
        s.track_ab([0.0, 0.0], lim.a_acc, lim.a_brk, lim.jerk, DT);
        assert!(s.v[0] >= 0.0, "reversed to {}", s.v[0]);
    }
}

#[test]
fn a_new_push_during_a_brake_unwinds_the_brake_at_the_jerk_limit() {
    let lim = normal();
    let mut s = Shaper::default();
    peaks(&mut s, [lim.v_h, 0.0], &lim, 500);
    peaks(&mut s, [0.0, 0.0], &lim, 60);
    assert!(s.a[0] < -lim.a_acc, "braking harder than it accelerates");
    let (_, _, jerk) = peaks(&mut s, [lim.v_h, 0.0], &lim, 300);
    assert!(jerk <= lim.jerk + 1e-2, "jerk {jerk}");
}

#[test]
fn the_vertical_shaper_ramps_and_brakes_within_its_acceleration() {
    let lim = normal();
    let mut s = Shaper1::default();
    let mut prev = 0.0;
    for i in 0..600 {
        let target = if i < 300 { -lim.v_up } else { 0.0 };
        let v = s.track(target, lim.a_v, lim.jerk_v, DT);
        assert!((v - prev).abs() <= lim.a_v * DT + 1e-4, "step {}", v - prev);
        prev = v;
    }
    assert_eq!(s.v, 0.0);
}

#[test]
fn the_sticks_ask_after_the_centre_threshold_and_the_expo() {
    let lim = normal();
    let a = StickAsk::of([0.015, -0.019, 0.01, -0.01], 0.0, &lim);
    assert!(a.is_release(), "residuals under the threshold are centred: {a:?}");
    let a = StickAsk::of([0.0, 1.0, 1.0, 1.0], 0.0, &lim);
    assert!((a.ne[0] - lim.v_h).abs() < 1e-4);
    assert!((a.vd + lim.v_up).abs() < 1e-4, "throttle up climbs");
    assert!((a.yaw_rate - lim.yaw_dps.to_radians()).abs() < 1e-4);
    let a = StickAsk::of([0.0, 0.0, -1.0, 0.0], 0.0, &lim);
    assert!((a.vd - lim.v_down).abs() < 1e-4, "throttle down descends");
    // Half stick on the Normal curve: (1 - 0.25)·0.5 + 0.25·0.125.
    let a = StickAsk::of([0.0, 0.5, 0.0, 0.0], 0.0, &lim);
    assert!((a.ne[0] - 0.40625 * lim.v_h).abs() < 1e-4, "{}", a.ne[0]);
    // A full diagonal flies the horizontal limit.
    let a = StickAsk::of([1.0, 1.0, 0.0, 0.0], 0.0, &lim);
    assert!((geo::norm(a.ne) - lim.v_h).abs() < 1e-4);
}

#[test]
fn forward_stick_follows_heading() {
    let lim = normal();
    let a = StickAsk::of([0.0, 1.0, 0.0, 0.0], std::f32::consts::FRAC_PI_2, &lim);
    assert!(a.ne[0].abs() < 1e-4 && (a.ne[1] - lim.v_h).abs() < 1e-4);
}

#[test]
fn the_profiles_brake_firmer_than_they_accelerate_and_sport_has_the_most_expo() {
    let [c, n, s] = [Profile::Cine, Profile::Normal, Profile::Sport]
        .map(|p| limits(VehicleClass::Multirotor, p));
    for l in [c, n, s] {
        assert!(l.a_brk >= l.a_acc);
        assert!(l.a_brk <= 2.5 && l.jerk <= 5.0 && l.a_v <= 1.0 && l.yaw_dps2 <= 270.0);
    }
    assert!(s.expo_h > n.expo_h && s.expo_v > n.expo_v && s.expo_yaw > n.expo_yaw);
    assert_eq!([c.v_h, n.v_h, s.v_h], [3.0, 7.0, 10.0]);
    // A class without a row of its own flies the multirotor row held to Cine.
    assert_eq!(limits(VehicleClass::Vtol, Profile::Sport).v_h, 3.0);
}

#[test]
fn the_fc_guidance_limits_cap_the_profile_in_either_unit() {
    let lim = limits(VehicleClass::Multirotor, Profile::Sport);
    let p = |n: &str| match n {
        "WP_ACC" => Some(2.0),
        "PSC_NE_JERK" => Some(4.0),
        "WP_ACC_Z" => Some(0.5),
        "PSC_D_JERK" => Some(3.0),
        "ATC_ACC_Y_MAX" => Some(18000.0),
        _ => None,
    };
    let c = fc_capped(lim, p);
    assert_eq!((c.a_acc, c.a_brk, c.jerk), (2.0, 2.0, 4.0));
    assert_eq!((c.a_v, c.jerk_v, c.yaw_dps2), (0.5, 3.0, 180.0));
    let old = |n: &str| match n {
        "WPNAV_ACCEL" => Some(150.0),
        "ATC_ACCEL_Y_MAX" => Some(9000.0),
        "PSC_JERK_XY" => Some(0.0),
        _ => None,
    };
    let c = fc_capped(lim, old);
    assert_eq!((c.a_acc, c.a_brk, c.yaw_dps2), (1.5, 1.5, 90.0));
    assert_eq!(c.jerk, lim.jerk, "0 is no limit");
    // Never raised above the profile.
    let lax = |n: &str| (n == "WP_ACC").then_some(9.0);
    assert_eq!(fc_capped(lim, lax), lim);
}
