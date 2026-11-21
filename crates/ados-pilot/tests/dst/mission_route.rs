//! Missions past obstacles on the planar lidar: a leg through a wall under
//! avoid `bypass` is flown round in GUIDED and the FC resumes AUTO at the
//! blocked item; under `brake` the same leg holds and counts down to its
//! skip; with avoidance off the FC flies the leg as planned; and a waypoint
//! inside a wall is skipped at once in either mode, without the countdown.

use ados_pilot::modes::copter;
use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{AvoidMode, Owner};
use serde_json::json;

use crate::metrics::{Meter, RunMetrics, MARGIN_M};
use crate::sim::Sim;

/// The wall across the second leg: north of home, out to this either side.
const WALL_N_M: f32 = 15.0;
const WALL_HALF_M: f32 = 4.0;
/// Its posts: radius and spacing, m (overlapping: no gap through it).
const POST_R_M: f32 = 0.25;
const POST_STEP_M: f32 = 0.4;
const ALT_M: f32 = 5.0;
/// The blocked item.
const BLOCKED_SEQ: u32 = 2;
/// A flight's sim-time limit, ms.
const TIMEOUT_MS: u64 = 120_000;

fn wall() -> Vec<([f32; 2], f32)> {
    let n = (2.0 * WALL_HALF_M / POST_STEP_M).round() as i32;
    (0..=n)
        .map(|k| ([WALL_N_M, -WALL_HALF_M + POST_STEP_M * k as f32], POST_R_M))
        .collect()
}

/// What one mission flight showed.
struct Flight {
    s: Sim,
    m: RunMetrics,
    /// The pilot published a detour for the blocked item.
    detoured: bool,
    /// The pilot flew the vehicle as a goto at some point.
    goto_seen: bool,
    /// The FC was in AUTO at the blocked item after the detour began.
    resumed_at_item: bool,
}

/// Up to the mission with `avoid`, the wall across the leg to item 2 and
/// `wp2` as that item; fly until `done` or the timeout.
fn fly(
    name: &'static str,
    seed: u64,
    avoid: AvoidMode,
    wp2: [f32; 2],
    mut done: impl FnMut(&Sim) -> bool,
) -> Flight {
    let mut s = Sim::new(name, seed, |c| c.pilot.avoid = avoid);
    let d = -ALT_M;
    s.fc.mission = vec![
        [3.0, 0.0, d],
        [wp2[0], wp2[1], d],
        [wp2[0] - 6.0, wp2[1] + 9.0, d],
    ];
    s.stand = wall();
    s.boot();
    let a = s.action("launch", Some(json!({"after": "mission", "alt_m": ALT_M})));
    assert!(a.ok, "launch: {a:?}");
    let auto = s.run_until(30_000, |s| s.fc.mode == copter::AUTO && s.fc.seq >= 1);
    assert!(auto, "never flew the mission: {:?}", s.state.sequence);
    let t = s.fc.truth();
    let mut meter = Meter::new(seed, [t[0], t[1]], wp2[0]);
    let (mut detoured, mut goto_seen, mut resumed_at_item) = (false, false, false);
    let t0 = s.now;
    while s.now - t0 < TIMEOUT_MS && !done(&s) {
        s.step();
        let t = s.fc.truth();
        meter.tick([t[0], t[1]], s.stand_clearance_m());
        goto_seen |= s.p.owner() == Owner::Goto;
        detoured |= s
            .state
            .mission_detour
            .as_ref()
            .is_some_and(|d| d.seq == BLOCKED_SEQ);
        resumed_at_item |= detoured && s.fc.mode == copter::AUTO && s.fc.seq == BLOCKED_SEQ;
    }
    Flight {
        s,
        m: meter.finish(),
        detoured,
        goto_seen,
        resumed_at_item,
    }
}

fn gaps(s: &Sim) -> Vec<(u32, String)> {
    s.state
        .mission
        .as_ref()
        .map(|m| m.gaps.iter().map(|g| (g.seq, g.reason.clone())).collect())
        .unwrap_or_default()
}

fn currents(s: &Sim) -> Vec<u32> {
    s.commands(MavCmd::MAV_CMD_DO_SET_MISSION_CURRENT)
        .map(|(_, c)| c.param1 as u32)
        .collect()
}

#[test]
fn dst_bypass_flies_round_a_wall_on_a_mission_leg_and_resumes_auto_at_the_item() {
    for seed in crate::SEEDS {
        let f = fly(
            "mission_bypass",
            seed,
            AvoidMode::Bypass,
            [28.0, 0.0],
            |s| s.fc.mission_complete,
        );
        let s = &f.s;
        eprintln!(
            "dst mission_bypass s{seed}: {:?}, detour {}, {:.1} sim s",
            f.m,
            f.detoured,
            s.sim_s()
        );
        assert!(f.detoured, "s{seed}: no detour: {:?}", s.state.owner_label);
        assert!(f.resumed_at_item, "s{seed}: AUTO never resumed at item 2");
        assert!(
            s.fc.mission_complete,
            "s{seed}: mission not flown to its end"
        );
        assert_eq!(
            currents(s),
            vec![BLOCKED_SEQ],
            "s{seed}: current set once, to the item"
        );
        assert!(
            gaps(s).is_empty(),
            "s{seed}: nothing skipped: {:?}",
            gaps(s)
        );
        assert!(!f.m.collision, "s{seed}: hit the wall: {:?}", f.m);
        assert_eq!(f.m.margin_entries, 0, "s{seed}: into the margin: {:?}", f.m);
        assert!(f.m.min_clearance_m >= MARGIN_M, "s{seed}: {:?}", f.m);
    }
}

#[test]
fn dst_brake_holds_before_a_wall_on_a_mission_leg_and_skips_at_the_countdown() {
    let seed = 1;
    let f = fly("mission_brake", seed, AvoidMode::Brake, [28.0, 0.0], |s| {
        !gaps(s).is_empty()
    });
    let s = &f.s;
    eprintln!(
        "dst mission_brake s{seed}: {:?}, {:.1} sim s",
        f.m,
        s.sim_s()
    );
    assert!(!f.goto_seen && !f.detoured, "brake never routes round");
    assert!(
        !s.mode_commands(copter::BRAKE).is_empty(),
        "braked for the wall"
    );
    assert_eq!(
        gaps(s),
        vec![(BLOCKED_SEQ, "stall_timeout".to_owned())],
        "held, then skipped at the countdown"
    );
    assert!(!f.m.collision, "hit the wall: {:?}", f.m);
}

#[test]
fn dst_avoid_off_flies_the_mission_leg_as_planned() {
    let f = fly("mission_off", 1, AvoidMode::Off, [28.0, 0.0], |s| {
        s.fc.mission_complete
    });
    let s = &f.s;
    assert!(s.fc.mission_complete, "flown to its end");
    assert!(s.mode_commands(copter::BRAKE).is_empty(), "never braked");
    assert!(!f.goto_seen && !f.detoured, "never routed");
    assert!(currents(s).is_empty() && gaps(s).is_empty());
}

#[test]
fn dst_a_waypoint_inside_a_wall_is_skipped_at_once_in_either_mode() {
    for avoid in [AvoidMode::Brake, AvoidMode::Bypass] {
        for seed in crate::SEEDS {
            // The item sits on a post of the wall.
            let wp2 = [WALL_N_M, 0.0];
            let f = fly("mission_inside", seed, avoid, wp2, |s| {
                s.fc.mission_complete
            });
            let s = &f.s;
            let skip_s = s
                .commands(MavCmd::MAV_CMD_DO_SET_MISSION_CURRENT)
                .next()
                .map(|(t, _)| t);
            let braked = s.mode_commands(copter::BRAKE).first().copied();
            eprintln!(
                "dst mission_inside {avoid:?} s{seed}: {:?}, brake at {braked:?}, skip at {skip_s:?}",
                f.m
            );
            assert_eq!(
                gaps(s),
                vec![(BLOCKED_SEQ, "unreachable_inside_obstacle".to_owned())],
                "{avoid:?} s{seed}: skipped as inside"
            );
            assert_eq!(currents(s), vec![BLOCKED_SEQ + 1], "{avoid:?} s{seed}");
            let (Some(b), Some(k)) = (braked, skip_s) else {
                panic!("{avoid:?} s{seed}: no hold or no skip");
            };
            // At once: within a few seconds of the hold, never the countdown.
            assert!(
                k - b < 5_000,
                "{avoid:?} s{seed}: skipped {} ms after the hold",
                k - b
            );
            assert!(
                s.state.mission_stall.is_none(),
                "{avoid:?} s{seed}: no stall left"
            );
            assert!(s.fc.mission_complete, "{avoid:?} s{seed}: the rest flown");
            assert!(!f.m.collision, "{avoid:?} s{seed}: hit the wall: {:?}", f.m);
        }
    }
}
