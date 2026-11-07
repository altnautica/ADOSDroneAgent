use super::*;

const DT: f32 = 0.02;

#[test]
fn step_input_reaches_v_h_without_exceeding_a_h() {
    let lim = limits(Profile::Normal);
    let mut s = Shaper::default();
    let mut prev = 0.0;
    for _ in 0..500 {
        let v = s.step([lim.v_h, 0.0], &lim, DT)[0];
        let a = (v - prev) / DT;
        assert!(a <= lim.a_h + 1e-3, "accel {a} exceeds {}", lim.a_h);
        prev = v;
    }
    assert!((prev - lim.v_h).abs() < 1e-3, "settled at {prev}");
}

#[test]
fn jerk_is_bounded() {
    let lim = limits(Profile::Cine);
    let mut s = Shaper::default();
    let mut prev_a = 0.0;
    for _ in 0..200 {
        s.step([lim.v_h, 0.0], &lim, DT);
        let j = (s.a[0] - prev_a).abs() / DT;
        assert!(j <= lim.jerk + 1e-2 || s.a[0] == 0.0, "jerk {j}");
        prev_a = s.a[0];
    }
}

fn stick(m: &mut Multirotor, axes: [f32; 4], pos: [f32; 3], yaw: f32) -> Command {
    let inp = StickIn {
        axes,
        pos,
        vel: [0.0; 3],
        yaw,
        dt: DT,
    };
    m.step(&inp, &limits(Profile::Normal), |v| v, (-3.0, 2.0))
}

#[test]
fn centred_sticks_capture_hold_below_capture_speed() {
    let mut m = Multirotor::default();
    for _ in 0..200 {
        stick(&mut m, [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, -5.0], 0.0);
    }
    assert!(m.shaper.v[0] > 7.0);
    let mut held = None;
    for i in 0..400 {
        if let Command::Hold { pos, .. } =
            stick(&mut m, [0.0; 4], [i as f32 * 0.01, 0.0, -5.0], 0.0)
        {
            held = Some((i, pos));
            break;
        }
    }
    let (_, pos) = held.expect("hold captured");
    assert_eq!(pos[2], -5.0);
    assert!(geo::norm(m.shaper.v) < HOLD_CAPTURE_MPS);
}

#[test]
fn throttle_climbs_and_centre_holds_altitude() {
    let mut m = Multirotor::default();
    match stick(&mut m, [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, -5.0], 0.0) {
        Command::Velocity { vel, .. } => assert!((vel[2] + 3.0).abs() < 1e-4),
        c => panic!("{c:?}"),
    }
    match stick(&mut m, [0.0, 0.0, -1.0, 0.0], [0.0, 0.0, -5.0], 0.0) {
        Command::Velocity { vel, .. } => assert!((vel[2] - 2.0).abs() < 1e-4),
        c => panic!("{c:?}"),
    }
}

#[test]
fn forward_stick_follows_heading() {
    let lim = limits(Profile::Normal);
    let v = Multirotor::stick_velocity([0.0, 1.0, 0.0, 0.0], std::f32::consts::FRAC_PI_2, &lim);
    assert!(v[0].abs() < 1e-4 && (v[1] - lim.v_h).abs() < 1e-4);
}

#[test]
fn yaw_stick_sets_rate() {
    let mut m = Multirotor::default();
    match stick(&mut m, [0.0, 0.0, 0.0, 1.0], [0.0, 0.0, -5.0], 0.0) {
        Command::Velocity { yaw_rate, .. } => assert!((yaw_rate - 90f32.to_radians()).abs() < 1e-4),
        c => panic!("{c:?}"),
    }
}
