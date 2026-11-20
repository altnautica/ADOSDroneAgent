//! The flights of the matrix. Each one takes a seed, flies the pilot from
//! the ground through one decision sequence and returns the run for its
//! checks; a seed varies the wind, sensor noise, battery, airframe and
//! route.

use ados_pilot::liftoff::sim::Airframe;
use ados_pilot::modes::copter;
use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::Owner;
use serde_json::json;

use crate::sim::Sim;

/// One scenario: fly a seed, return the run.
pub type Scenario = fn(u64) -> Sim;

/// The scenarios, by name.
pub const ALL: &[(&str, Scenario)] = &[
    ("low_battery", low_battery),
    ("energy_return", energy_return),
    ("energy_withdrawn", energy_withdrawn),
    ("liftoff", liftoff),
    ("link_loss", link_loss),
    ("mission", mission),
];

fn ok(a: &ados_protocol::pilot::ActionResult) -> bool {
    a.ok
}

/// Launch to `alt_m` with the FC's take-off, handed to the pilot's hold.
pub fn launch_to_hold(s: &mut Sim, alt_m: f32) {
    let a = s.action("launch", Some(json!({"after": "hold", "alt_m": alt_m})));
    assert!(ok(&a), "launch: {a:?}");
    let up = s.run_until(30_000, |s| {
        -s.fc.pos[2] > alt_m - 0.3 && s.p.owner() == Owner::Pilot && s.fc.mode == copter::GUIDED
    });
    assert!(up, "never reached the hold: {:?}", s.state.sequence);
    s.run(1_000);
}

/// Fly the sticks `axes` for `ms`, from centred sticks (the pilot's gate),
/// then centre them for a second.
pub fn fly_sticks(s: &mut Sim, axes: [f32; 4], ms: u64) {
    s.sticks = Some([0.0; 4]);
    s.run(800);
    s.sticks = Some(axes);
    s.run(ms);
    s.sticks = Some([0.0; 4]);
    s.run(1_000);
}

/// Low battery at the floor: a GNSS copter flown away from home drains to
/// the pilot's 30 % floor (no charge level on the FC), counts down, returns
/// by SmartRTL and lands home.
pub fn low_battery(seed: u64) -> Sim {
    let mut s = Sim::new("low_battery", seed, |_| {});
    s.fc.battery_pct = s.rng.range(34.0, 37.0);
    s.fc.drain_pct_s = s.rng.range(0.15, 0.3);
    let out = [s.rng.range(-0.5, 0.5), s.rng.range(0.5, 1.0), 0.0, 0.0];
    let out_ms = s.rng.range(2_000.0, 5_000.0) as u64;
    s.boot();
    launch_to_hold(&mut s, 5.0);
    fly_sticks(&mut s, out, out_ms);
    assert!(
        s.home_distance_m() > 3.0,
        "flew out: {}",
        s.home_distance_m()
    );
    let counting = s.run_until(120_000, |s| s.state.low_battery_action.is_some());
    assert!(counting, "no low-battery action at {}%", s.fc.battery_pct);
    let fired_ms = s.now;
    let reported = s.fc.battery_pct.floor();
    assert_eq!(s.state.low_battery_action.as_deref(), Some("rth"));
    assert!(
        (29.0..=30.0).contains(&reported),
        "acts at the 30 % floor, not at {reported}%"
    );
    assert!(s.state.rth_countdown_s.is_some(), "a countdown");
    let landed = s.run_until(180_000, |s| !s.fc.armed);
    assert!(landed, "never landed: mode {} at {:?}", s.fc.mode, s.fc.pos);
    let srtl = s.mode_commands(copter::SMART_RTL);
    assert_eq!(srtl.len(), 1, "SmartRTL once: {srtl:?}");
    let delay = srtl[0] - fired_ms;
    assert!(
        (9_500..=10_600).contains(&delay),
        "SmartRTL {delay} ms after the countdown began"
    );
    assert!(s.mode_commands(copter::RTL).is_empty(), "never a plain RTL");
    assert!(
        s.mode_commands(copter::LAND).is_empty(),
        "never lands in place"
    );
    assert!(
        s.home_distance_m() < 1.5,
        "landed home: {}",
        s.home_distance_m()
    );
    s
}

/// The energy-aware return, as a rig survey on a 2.5 Ah pack flew it in
/// simulation: a GNSS copter with a measured current (21-24 A) flown out
/// 100-140 m at a modest stick speed, with SmartRTL retracing at 0.80-0.88
/// of the speed it flew out (the range seen there). The pilot returns on its
/// energy estimate above the 20 % floor (`BATT_LOW_MAH` 500), and the copter
/// lands home above the FC's low level (20 %), never mind its critical one.
pub fn energy_return(seed: u64) -> Sim {
    let mut s = energy_sim("energy_return", seed, 10);
    s.fc.battery_pct = s.rng.range(68.0, 72.0);
    let out_ms = s.rng.range(40_000.0, 55_000.0) as u64;
    s.boot();
    launch_to_hold(&mut s, 5.0);
    fly_sticks(&mut s, [0.0, 0.5, 0.0, 0.0], out_ms);
    let out_m = s.home_distance_m();
    assert!(out_m > 80.0, "flew out: {out_m}");
    let counting = s.run_until(300_000, |s| s.state.low_battery_action.is_some());
    assert!(counting, "no low-battery action at {}%", s.fc.battery_pct);
    let fired_pct = s.fc.battery_pct;
    let reserve = s.state.battery_reserve.clone().expect("a reserve airborne");
    assert_eq!(reserve.basis, "energy", "{reserve:?} at {fired_pct}%");
    assert!(fired_pct > 21.0, "above the floor: {fired_pct}%");
    let landed = s.run_until(300_000, |s| !s.fc.armed);
    assert!(landed, "never landed: mode {} at {:?}", s.fc.mode, s.fc.pos);
    assert_eq!(s.mode_commands(copter::SMART_RTL).len(), 1);
    assert!(
        s.mode_commands(copter::LAND).is_empty(),
        "never lands in place"
    );
    assert!(
        s.home_distance_m() < 1.5,
        "landed home: {}",
        s.home_distance_m()
    );
    // Touchdown above the FC's low level (BATT_LOW_MAH 500, 20 %): its own
    // low failsafe never interrupts the return.
    assert!(
        s.fc.battery_pct >= 20.0,
        "landed at {}% (fired at {fired_pct}%, {out_m:.0} m out): under the FC low level",
        s.fc.battery_pct
    );
    s
}

/// The 2.5 Ah rig copter of [`energy_return`]: capacity, levels, a measured
/// current of 21-24 A and the battery draining at it, SmartRTL retracing at
/// 0.80-0.88 of the speed flown out; a low-battery countdown of
/// `countdown_s`.
fn energy_sim(name: &'static str, seed: u64, countdown_s: u32) -> Sim {
    let mut s = Sim::new(name, seed, |st| {
        st.pilot.link_loss_s = 3600;
        st.pilot.low_battery_countdown_s = countdown_s;
    });
    for (n, v) in [
        ("BATT_CAPACITY", 2500.0),
        ("BATT_LOW_MAH", 500.0),
        ("BATT_CRT_MAH", 250.0),
        ("WPNAV_SPEED", 400.0),
    ] {
        s.fc.params.insert(n.to_string(), v);
    }
    let amps = s.rng.range(21.0, 24.0);
    s.fc.current_a = Some(amps);
    // mAh per second over the capacity, in %.
    s.fc.drain_pct_s = amps / 3.6 / 2500.0 * 100.0;
    s.fc.srtl_share = Some(s.rng.range(0.80, 0.88));
    s
}

/// Fly `axes` from centred sticks until `done`, at most `cap_ms`; then
/// centre the sticks for a second.
fn fly_until(s: &mut Sim, axes: [f32; 4], cap_ms: u64, done: impl FnMut(&Sim) -> bool) -> bool {
    s.sticks = Some([0.0; 4]);
    s.run(800);
    s.sticks = Some(axes);
    let reached = s.run_until(cap_ms, done);
    s.sticks = Some([0.0; 4]);
    s.run(1_000);
    reached
}

/// A countdown whose shortfall goes away is withdrawn, and the return comes
/// after a fresh countdown. As on a rig survey in simulation (2.5 Ah, at real
/// time and accelerated alike): the path home of the survey's first pass is
/// long, the countdown starts, then the crossing pass prunes the path
/// (SmartRTL cuts the loop) and the need falls well below the charge. The
/// countdown used to freeze with its deadline set, still published, and the
/// next shortfall 140 s later fired SmartRTL at once, with no countdown. Here
/// the copter flies a loop (60 m out, 30 m across, back to 6 m short of
/// home), the countdown (20 s) starts while it holds there, and it closes the
/// loop within the countdown, crossing its first leg 6 m from home.
pub fn energy_withdrawn(seed: u64) -> Sim {
    let mut s = energy_sim("energy_withdrawn", seed, 20);
    s.fc.battery_pct = s.rng.range(90.0, 92.0);
    s.boot();
    launch_to_hold(&mut s, 5.0);
    /// Stick axes, flown until the position check passes.
    type Leg = ([f32; 4], fn(&Sim) -> bool);
    let legs: [Leg; 3] = [
        ([0.0, 0.5, 0.0, 0.0], |s| s.fc.pos[0] > 60.0),
        ([0.5, 0.0, 0.0, 0.0], |s| s.fc.pos[1] > 30.0),
        ([0.0, -0.5, 0.0, 0.0], |s| s.fc.pos[0] < 6.0),
    ];
    for (axes, done) in legs {
        assert!(
            fly_until(&mut s, axes, 120_000, done),
            "leg {axes:?} at {:?}",
            s.fc.pos
        );
    }
    assert_eq!(
        s.state.rth_countdown_s, None,
        "counting down before the loop's end"
    );
    let counting = s.run_until(180_000, |s| s.state.rth_countdown_s.is_some());
    assert!(counting, "no countdown at {}%", s.fc.battery_pct);
    let first = s.now;
    let first_pct = s.fc.battery_pct;
    assert!(first_pct > 21.0, "the energy estimate, not the floor");
    // Close the loop: across the first leg beside home.
    assert!(fly_until(&mut s, [-0.5, 0.0, 0.0, 0.0], 15_000, |s| s
        .fc
        .pos[1]
        < -2.0));
    assert!(
        s.now - first < 20_000,
        "the loop closed inside the countdown"
    );
    assert!(s.home_distance_m() < 10.0, "beside home: {:?}", s.fc.pos);
    assert_eq!(s.fc.mode, copter::GUIDED, "still flown by the pilot");
    assert_eq!(
        s.state.rth_countdown_s, None,
        "withdrawn at {}%",
        s.fc.battery_pct
    );
    assert_eq!(s.state.low_battery_action, None);
    assert!(
        s.mode_commands(copter::SMART_RTL).is_empty(),
        "no return yet"
    );
    // The next shortfall counts down again, in full, before the return.
    let again = s.run_until(300_000, |s| s.state.rth_countdown_s.is_some());
    assert!(again, "no second countdown at {}%", s.fc.battery_pct);
    let second = s.now;
    let landed = s.run_until(300_000, |s| !s.fc.armed);
    assert!(landed, "never landed: mode {} at {:?}", s.fc.mode, s.fc.pos);
    let srtl = s.mode_commands(copter::SMART_RTL);
    assert_eq!(srtl.len(), 1, "SmartRTL once: {srtl:?}");
    let delay = srtl[0] - second;
    assert!(
        (19_500..=20_600).contains(&delay),
        "SmartRTL {delay} ms after the second countdown began (first at {first_pct:.1}%)"
    );
    assert!(
        s.home_distance_m() < 1.5,
        "landed home: {}",
        s.home_distance_m()
    );
    assert!(
        s.fc.battery_pct >= 20.0,
        "landed at {}%, under the FC low level",
        s.fc.battery_pct
    );
    s
}

/// The supervised lift-off on a seeded unbalanced airframe reaches a level
/// hover at the launch height under the pilot, never through the FC's own
/// take-off.
pub fn liftoff(seed: u64) -> Sim {
    let mut s = Sim::new("liftoff", seed, |st| st.pilot.supervised_liftoff = true);
    let hover = s.rng.range(0.48, 0.56);
    let trim = [s.rng.range(-0.1, 0.1), s.rng.range(-0.22, 0.22)];
    s.fc.rig.af = Airframe::new(hover, trim);
    s.boot();
    let a = s.action("launch", Some(json!({"after": "hold", "alt_m": 3.0})));
    assert!(ok(&a), "launch: {a:?}");
    let mut peak_deg: f32 = 0.0;
    let mut phases: Vec<String> = Vec::new();
    let mut taking_off = false;
    let mut handed_ms = None;
    let end = s.now + 60_000;
    // Every tick through the lift-off to the hand-over, then 15 s of hover.
    while s.now < end && handed_ms.is_none_or(|t| s.now < t + 15_000) {
        if s.fc.rig.engaged && s.fc.rig.af.h < 0.5 {
            peak_deg = peak_deg.max(s.fc.att[0].abs().max(s.fc.att[1].abs()).to_degrees());
        }
        let seq = s.state.sequence.as_ref();
        if let Some(l) = seq.and_then(|q| q.liftoff.as_ref()) {
            if phases.last() != Some(&l.phase) {
                phases.push(l.phase.clone());
            }
        }
        let in_takeoff = seq.is_some_and(|q| q.step == "takeoff");
        if taking_off && !in_takeoff && handed_ms.is_none() {
            handed_ms = Some(s.now);
        }
        taking_off |= in_takeoff;
        s.step();
    }
    assert!(handed_ms.is_some(), "still taking off: {phases:?}");
    let seq = s.state.sequence.clone();
    assert!(
        seq.as_ref().is_none_or(|q| q.error.is_none()),
        "the launch stopped: {seq:?} after {phases:?}"
    );
    assert!(!phases.iter().any(|p| p == "stopped"), "{phases:?}");
    assert!(
        s.commands(MavCmd::MAV_CMD_NAV_TAKEOFF).next().is_none(),
        "never the FC's take-off"
    );
    assert!(s.fc.armed && !s.fc.landed, "airborne");
    assert!(peak_deg < 5.0, "lifted off level: peak {peak_deg}°");
    // The launch hands over at 90 % of its height; the hold keeps the 3 m
    // the launch asked for.
    let h = -s.fc.pos[2];
    assert!(
        (2.9..=3.1).contains(&h) && s.fc.vel[2].abs() < 0.2,
        "hovers at the launch height: {:?} {:?}",
        s.fc.pos,
        s.fc.vel
    );
    assert_eq!(s.p.owner(), Owner::Pilot);
    assert!(s.p.airframe.trim().is_some(), "the lift-off kept its trim");
    s
}

/// The operator's link drops after a stick leg: the pilot brakes, counts
/// down `link_loss_s` from the last contact, returns by SmartRTL and lands
/// home.
pub fn link_loss(seed: u64) -> Sim {
    let mut s = Sim::new("link_loss", seed, |_| {});
    let out = [s.rng.range(-0.6, 0.6), s.rng.range(0.4, 0.9), 0.0, 0.0];
    let out_ms = s.rng.range(2_000.0, 5_000.0) as u64;
    let quiet_ms = s.rng.range(0.0, 3_000.0) as u64;
    s.boot();
    launch_to_hold(&mut s, 5.0);
    fly_sticks(&mut s, out, out_ms);
    s.run(quiet_ms);
    s.link = false;
    s.sticks = None;
    let last = s.last_contact_ms;
    let link_loss_ms = u64::from(s.p.settings.pilot.link_loss_s) * 1000;
    let counted = s.run_until(link_loss_ms, |s| s.state.link_loss_in_s.is_some());
    assert!(counted, "no link-loss countdown");
    let returned = s.run_until(link_loss_ms + 2_000, |s| {
        !s.mode_commands(copter::SMART_RTL).is_empty()
    });
    assert!(returned, "no return after the link dropped");
    let at = s.mode_commands(copter::SMART_RTL)[0] - last;
    assert!(
        (link_loss_ms..=link_loss_ms + 300).contains(&at),
        "SmartRTL {at} ms after the last contact"
    );
    let landed = s.run_until(180_000, |s| !s.fc.armed);
    assert!(landed, "never landed: mode {} at {:?}", s.fc.mode, s.fc.pos);
    assert_eq!(s.mode_commands(copter::SMART_RTL).len(), 1);
    assert!(s.mode_commands(copter::RTL).is_empty(), "never a plain RTL");
    assert_eq!(s.p.owner(), Owner::FcMode);
    assert!(
        s.home_distance_m() < 1.5,
        "landed home: {}",
        s.home_distance_m()
    );
    s
}

/// A mission launched from the ground: the operator pauses and resumes it,
/// one waypoint is blocked so the pilot holds and skips it at the end of the
/// countdown, and the mission's end returns home.
pub fn mission(seed: u64) -> Sim {
    let mut s = Sim::new("mission", seed, |_| {});
    s.fc.mission = (0..5)
        .map(|_| {
            let bearing = s.rng.range(0.0, std::f32::consts::TAU);
            let r = s.rng.range(10.0, 25.0);
            let d = -s.rng.range(5.0, 8.0);
            [r * bearing.cos(), r * bearing.sin(), d]
        })
        .collect();
    let blocked = 3 + (s.rng.next_u64() % 2) as u32;
    s.fc.blocked_seq = Some(blocked);
    let pause_after_ms = s.rng.range(500.0, 2_500.0) as u64;
    s.boot();
    let a = s.action(
        "launch",
        Some(json!({"after": "mission", "alt_m": 5.0, "when_done": "rth"})),
    );
    assert!(ok(&a), "launch: {a:?}");
    let auto = s.run_until(30_000, |s| s.fc.mode == copter::AUTO && s.fc.seq >= 1);
    assert!(auto, "never flew the mission: {:?}", s.state.sequence);
    assert_eq!(s.p.owner(), Owner::MissionFc);

    // Hold: pause on the way to item 2, the copter stays put, resume.
    assert!(
        s.run_until(60_000, |s| s.fc.seq == 2),
        "never reached item 2"
    );
    s.run(pause_after_ms);
    assert!(ok(&s.action("pause", None)));
    // Braked to its stopping point first.
    s.run(1_500);
    let held_at = s.fc.pos;
    s.run(3_000);
    let drift = ((s.fc.pos[0] - held_at[0]).powi(2) + (s.fc.pos[1] - held_at[1]).powi(2)).sqrt();
    assert!(drift < 0.5, "held while paused: drifted {drift} m");
    assert_eq!(
        s.state.mission.as_ref().map(|m| m.state.as_str()),
        Some("paused")
    );
    assert!(ok(&s.action("resume", None)));
    let pauses: Vec<f32> = s
        .commands(MavCmd::MAV_CMD_DO_PAUSE_CONTINUE)
        .map(|(_, c)| c.param1)
        .collect();
    assert_eq!(pauses, vec![0.0, 1.0], "one pause, one continue");
    assert_eq!(s.p.owner(), Owner::MissionFc);

    // Skip: the blocked item stalls, holds, and is skipped at the countdown.
    let stalled = s.run_until(120_000, |s| s.state.mission_stall.is_some());
    assert!(stalled, "the blocked item {blocked} never stalled");
    assert_eq!(s.fc.seq, blocked);
    let skipped = s.run_until(70_000, |s| s.fc.seq > blocked);
    assert!(skipped, "never skipped item {blocked}");
    let currents: Vec<u32> = s
        .commands(MavCmd::MAV_CMD_DO_SET_MISSION_CURRENT)
        .map(|(_, c)| c.param1 as u32)
        .collect();
    assert_eq!(currents, vec![blocked + 1]);
    assert!(
        !s.mode_commands(copter::BRAKE).is_empty(),
        "braked at the stall"
    );

    // The mission's end: when-done RTH, home and down.
    let landed = s.run_until(240_000, |s| !s.fc.armed);
    assert!(landed, "never landed: mode {} at {:?}", s.fc.mode, s.fc.pos);
    assert!(s.fc.mission_complete, "flew the rest of the mission");
    assert_eq!(s.mode_commands(copter::SMART_RTL).len(), 1);
    assert!(s.mode_commands(copter::RTL).is_empty(), "never a plain RTL");
    let gaps = s
        .state
        .mission
        .as_ref()
        .map(|m| m.gaps.clone())
        .unwrap_or_default();
    assert_eq!(
        gaps.iter()
            .map(|g| (g.seq, g.reason.as_str()))
            .collect::<Vec<_>>(),
        vec![(blocked, "stall_timeout")]
    );
    assert!(
        s.home_distance_m() < 1.5,
        "landed home: {}",
        s.home_distance_m()
    );
    s
}
