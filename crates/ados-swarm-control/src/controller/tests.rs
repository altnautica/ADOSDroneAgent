use std::time::Duration;

use super::*;
use crate::neighbor::{STATUS_ARMED, STATUS_GPS_OK, STATUS_GUIDED};
use crate::separation::{climb_offset_m, SEPARATION_HARD_M};
use crate::setpoint::SetpointKind;

const HOME: (f64, f64) = (12.9716, 77.5946);
const ALT: f64 = 30.0;

fn cfg(mode: &str) -> SwarmControlConfig {
    SwarmControlConfig {
        enabled: true,
        mode: mode.into(),
        ..Default::default()
    }
}

fn controller(mode: &str, slots: &[u8]) -> SwarmController {
    SwarmController::new(&cfg(mode), slots)
}

fn own(slot: u8) -> OwnState {
    OwnState {
        slot,
        lat_deg: HOME.0,
        lon_deg: HOME.1,
        alt_rel_m: ALT,
        vn: 0.0,
        ve: 0.0,
        vd: 0.0,
        armed: true,
        guided: true,
        // Fresh by default so every existing case still exercises what it was
        // written for; the staleness cases below set this explicitly.
        fix_age: Some(std::time::Duration::ZERO),
    }
}

/// A neighbour at a local NED offset from `own`.
fn fix(slot: u8, offset: Ned) -> NeighborFix {
    let o = GeoOrigin::new(HOME.0, HOME.1, ALT);
    let (lat, lon, alt) = o.to_geo(offset);
    NeighborFix {
        slot,
        lat_deg: lat,
        lon_deg: lon,
        alt_m: alt,
        vn: 0.0,
        ve: 0.0,
        vd: 0.0,
        status: STATUS_ARMED | STATUS_GUIDED | STATUS_GPS_OK,
    }
}

fn t0() -> Instant {
    Instant::now()
}

// ---------------------------------------------------------------- precedence

#[test]
fn precedence_is_the_ladder_for_every_combination_of_active_layers() {
    let t = t0();
    let slots = [1u8, 2, 3];
    // Layers are switched on cumulatively from the bottom of the ladder up, and
    // each addition must take over from the one below it.
    let far = fix(2, Ned::new(60.0, 0.0, 0.0)); // outside every radius
    let near = fix(3, Ned::new(SEPARATION_HARD_M - 1.0, 0.0, 0.0));

    // hold only
    let mut c = controller("hold", &slots);
    assert_eq!(c.tick(&own(1), &[far], t).precedence, ModePrecedence::Hold);

    // flocking beats hold
    let mut c = controller("flocking", &slots);
    c.set_target(Some((HOME.0 + 0.01, HOME.1, ALT)));
    assert_eq!(
        c.tick(&own(1), &[far], t).precedence,
        ModePrecedence::Flocking
    );

    // formation beats flocking (it is a different commanded mode, higher rung)
    let mut c = controller("formation", &slots);
    assert_eq!(
        c.tick(&own(1), &[far], t).precedence,
        ModePrecedence::Formation
    );

    // operator beats formation
    let mut c = controller("formation", &slots);
    c.set_operator_directive(
        OperatorDirective::Goto {
            lat_deg: HOME.0 + 0.001,
            lon_deg: HOME.1,
            alt_m: ALT,
        },
        t,
    );
    assert_eq!(
        c.tick(&own(1), &[far], t).precedence,
        ModePrecedence::Operator
    );

    // hard separation beats the operator, which is the whole point of the ladder
    let mut c = controller("formation", &slots);
    c.set_operator_directive(
        OperatorDirective::Goto {
            lat_deg: HOME.0 + 0.001,
            lon_deg: HOME.1,
            alt_m: ALT,
        },
        t,
    );
    let out = c.tick(&own(1), &[far, near], t);
    assert_eq!(out.precedence, ModePrecedence::HardSeparation);
    assert!(out.emergency);
}

#[test]
fn hard_separation_outranks_every_mode_including_hold() {
    let t = t0();
    let near = fix(2, Ned::new(2.0, 0.0, 0.0));
    for mode in ["hold", "flocking", "formation"] {
        let mut c = controller(mode, &[1, 2]);
        let out = c.tick(&own(1), &[near], t);
        assert_eq!(
            out.precedence,
            ModePrecedence::HardSeparation,
            "mode {mode} did not yield to the safety layer"
        );
    }
}

#[test]
fn formation_falls_back_to_hold_when_this_drone_has_no_station() {
    let t = t0();
    // The formation table covers slots 1-3; this drone is slot 9.
    let mut c = controller("formation", &[1, 2, 3]);
    let out = c.tick(&own(9), &[fix(2, Ned::new(40.0, 0.0, 0.0))], t);
    assert_eq!(out.precedence, ModePrecedence::Hold);
}

#[test]
fn the_active_level_is_reported_not_the_commanded_one() {
    let t = t0();
    let mut c = controller("formation", &[1, 2]);
    assert_eq!(c.mode(), SwarmMode::Formation);
    let out = c.tick(&own(1), &[fix(2, Ned::new(1.5, 0.0, 0.0))], t);
    assert_eq!(out.precedence, ModePrecedence::HardSeparation);
    assert_eq!(c.precedence(), ModePrecedence::HardSeparation);
    assert_eq!(
        c.mode(),
        SwarmMode::Formation,
        "the commanded mode is unchanged; only the ACTIVE level differs"
    );
    assert_eq!(c.precedence().as_wire(), "hard-separation");
}

// -------------------------------------------------------------- suppression

#[test]
fn an_empty_neighbour_table_for_the_stale_window_emits_no_setpoint() {
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    c.set_target(Some((HOME.0 + 0.01, HOME.1, ALT)));

    // Fresh: with a neighbour, it flies.
    let out = c.tick(&own(1), &[fix(2, Ned::new(20.0, 0.0, 0.0))], t);
    assert!(out.setpoint.is_some());
    assert_eq!(out.precedence, ModePrecedence::Flocking);

    // Just inside the window with an empty table: still flying on the last fix.
    let out = c.tick(
        &own(1),
        &[],
        t + crate::NEIGHBOR_STALE - Duration::from_millis(1),
    );
    assert!(out.setpoint.is_some(), "must not give up early");

    // At the window: nothing at all.
    let out = c.tick(&own(1), &[], t + crate::NEIGHBOR_STALE);
    assert!(out.setpoint.is_none(), "flew on stale data: {out:?}");
    assert_eq!(out.suppressed, Some(Suppression::NeighborsStale));
    assert_eq!(out.precedence, ModePrecedence::Hold);
    assert!(!out.emergency);

    // And it stays silent while the table stays empty.
    for k in 1..20 {
        let out = c.tick(&own(1), &[], t + crate::NEIGHBOR_STALE + CONTROL_PERIOD * k);
        assert!(out.setpoint.is_none());
    }
    // A fresh beacon revives it immediately.
    let late = t + crate::NEIGHBOR_STALE + Duration::from_secs(5);
    let out = c.tick(&own(1), &[fix(2, Ned::new(20.0, 0.0, 0.0))], late);
    assert!(out.setpoint.is_some(), "a fresh fix must revive the layer");
}

#[test]
fn a_stale_table_releases_the_hard_latch_rather_than_holding_a_climb() {
    let t = t0();
    let mut c = controller("hold", &[1, 2]);
    assert_eq!(
        c.tick(&own(1), &[fix(2, Ned::new(2.0, 0.0, 0.0))], t)
            .precedence,
        ModePrecedence::HardSeparation
    );
    assert!(c.hard_latch().is_some());
    let out = c.tick(&own(1), &[], t + crate::NEIGHBOR_STALE);
    assert!(
        c.hard_latch().is_none(),
        "a latch on vanished data is stale data"
    );
    assert!(out.setpoint.is_none());
    assert!(!out.emergency);
}

#[test]
fn a_disarmed_vehicle_gets_nothing() {
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    let mut o = own(1);
    o.armed = false;
    let out = c.tick(&o, &[fix(2, Ned::new(2.0, 0.0, 0.0))], t);
    assert!(out.setpoint.is_none());
    assert_eq!(out.suppressed, Some(Suppression::NotArmed));
    // The separation layer is documented as running "whenever armed", so a
    // disarmed drone must not even flag an emergency at 2 m — it is sitting on
    // the ground next to another drone.
    assert!(!out.emergency);
    assert!(c.hard_latch().is_none());
}

#[test]
fn a_vehicle_out_of_guided_gets_nothing_but_still_raises_the_alarm() {
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    let mut o = own(1);
    o.guided = false;
    let out = c.tick(&o, &[fix(2, Ned::new(2.0, 0.0, 0.0))], t);
    assert!(out.setpoint.is_none(), "setpoints need GUIDED");
    assert_eq!(out.suppressed, Some(Suppression::NotGuided));
    assert!(
        out.emergency,
        "a near miss is a near miss whatever mode the FC is in"
    );
    assert_eq!(out.precedence, ModePrecedence::Hold, "nothing is driving");
}

#[test]
fn a_frozen_own_position_stops_the_controller_commanding() {
    // The partial-stall case, and the one that failed open. Position updates
    // stop while heartbeats continue, so armed and guided stay true and the
    // fix simply freezes. Every neighbour is measured in a frame anchored on
    // that fix, so the controller would keep commanding confidently against
    // where the aircraft used to be.
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    let mut o = own(1);
    o.fix_age = Some(crate::OWN_STATE_STALE);
    let out = c.tick(&o, &[fix(2, Ned::new(2.0, 0.0, 0.0))], t);
    assert!(
        out.setpoint.is_none(),
        "a stale own fix must not command anything"
    );
    assert_eq!(out.suppressed, Some(Suppression::OwnStateStale));
    assert_eq!(out.precedence, ModePrecedence::Hold);
}

#[test]
fn a_position_never_seen_is_treated_as_infinitely_stale() {
    // Absent is not fresh. A controller that has never had a fix has no frame
    // to measure anyone in.
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    let mut o = own(1);
    o.fix_age = None;
    let out = c.tick(&o, &[fix(2, Ned::new(2.0, 0.0, 0.0))], t);
    assert!(out.setpoint.is_none());
    assert_eq!(out.suppressed, Some(Suppression::OwnStateStale));
}

#[test]
fn own_state_freshness_is_checked_before_the_neighbour_table() {
    // Ordering matters for the diagnosis, not just the outcome. With BOTH the
    // own fix stale and the neighbours stale, the reported reason must be the
    // own fix: it is the one that makes every other reading untrustworthy, and
    // an operator told "neighbours stale" would go looking at the radio.
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    let mut o = own(1);
    o.fix_age = None;
    let out = c.tick(&o, &[], t);
    assert_eq!(out.suppressed, Some(Suppression::OwnStateStale));
}

#[test]
fn a_fresh_fix_just_inside_the_window_still_flies() {
    // The gate must not have become a ban: a fix refreshed inside the window is
    // exactly the normal case at any real telemetry rate.
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    let mut o = own(1);
    o.fix_age = Some(crate::OWN_STATE_STALE - std::time::Duration::from_millis(1));
    let out = c.tick(&o, &[fix(2, Ned::new(2.0, 0.0, 0.0))], t);
    assert_ne!(
        out.suppressed,
        Some(Suppression::OwnStateStale),
        "a fix inside the window is fresh"
    );
}

#[test]
fn a_disabled_swarm_never_commands_anything() {
    let t = t0();
    let mut c = SwarmController::new(&SwarmControlConfig::default(), &[1, 2]);
    let out = c.tick(&own(1), &[fix(2, Ned::new(1.0, 0.0, 0.0))], t);
    assert!(out.setpoint.is_none());
    assert_eq!(out.suppressed, Some(Suppression::Disabled));
    assert!(!out.emergency);
}

#[test]
fn hold_with_nothing_to_say_emits_nothing() {
    let t = t0();
    let mut c = controller("hold", &[1, 2]);
    // A neighbour far outside every radius: the safety term is silent, so the
    // layer must be too rather than dribbling zero setpoints at 10 Hz.
    let out = c.tick(&own(1), &[fix(2, Ned::new(200.0, 0.0, 0.0))], t);
    assert!(out.setpoint.is_none());
    assert_eq!(out.suppressed, Some(Suppression::NothingToDo));
}

#[test]
fn hold_still_applies_the_safety_term_when_a_neighbour_is_close() {
    let t = t0();
    let mut c = controller("hold", &[1, 2]);
    // Inside the soft radius but outside the hard one: a real repulsive nudge.
    let out = c.tick(&own(1), &[fix(2, Ned::new(4.5, 0.0, 0.0))], t);
    let s = out.setpoint.expect("the safety layer runs whenever armed");
    assert!(s.vn < 0.0, "pushed away from the neighbour: {s:?}");
    assert_eq!(
        out.precedence,
        ModePrecedence::Hold,
        "nothing is OVERRIDING"
    );
}

// ------------------------------------------------------------------- hard

#[test]
fn the_hard_override_climbs_and_keeps_only_the_repulsive_push() {
    let t = t0();
    let mut c = controller("formation", &[1, 2]);
    // Neighbour north-east and close: the formation layer's horizontal plan is
    // discarded, but the safety layer's own push away from that neighbour is not.
    let out = c.tick(&own(1), &[fix(2, Ned::new(1.0, 1.0, 0.0))], t);
    let s = out.setpoint.expect("engaged");
    assert_eq!(s.kind, SetpointKind::Velocity);
    assert!(
        s.vn < 0.0 && s.ve < 0.0,
        "pushed away from the offender: {s:?}"
    );
    let horizontal = (s.vn as f64).hypot(s.ve as f64);
    // Bounded by the repulsive law itself, not by a behaviour layer: at 1.41 m the
    // plan's `1.5 * (1/d - 1/8)` is under 0.9 m/s.
    assert!(
        horizontal < 1.0,
        "this is the safety push, not a manoeuvre: {horizontal}"
    );
    assert!(
        s.vd < 0.0,
        "down-positive, so a climb is negative: {}",
        s.vd
    );
    assert_eq!(out.precedence, ModePrecedence::HardSeparation);
    assert!(out.emergency);

    // A drone with no close neighbour at all cannot be in the override, so the
    // pure-vertical case is reached only through the pin path with the offender
    // already outside the repulsive radius.
    let mut c = controller("hold", &[1, 2]);
    let out = c.tick(&own(1), &[fix(2, Ned::new(200.0, 0.0, 0.0))], t);
    assert!(out.setpoint.is_none());
}

#[test]
fn the_climb_target_is_latched_and_ordered_by_slot() {
    let t = t0();
    // Every one of these is below the offender's slot 9, so all of them climb, by
    // an amount that grows with the gap.
    for slot in [1u8, 2, 7, 8] {
        let mut c = controller("hold", &[slot, 9]);
        c.tick(&own(slot), &[fix(9, Ned::new(1.0, 0.0, 0.0))], t);
        let (offender, target) = c.hard_latch().expect("engaged");
        assert_eq!(offender, 9);
        assert!(
            (target - (ALT + climb_offset_m(slot, 9))).abs() < 1e-6,
            "slot {slot} target {target}"
        );
        // The target does NOT walk upward as the vehicle climbs toward it.
        let mut climbing = own(slot);
        climbing.alt_rel_m = ALT + 0.2;
        c.tick(
            &climbing,
            &[fix(9, Ned::new(1.0, 0.0, 0.0))],
            t + CONTROL_PERIOD,
        );
        assert_eq!(c.hard_latch().map(|l| l.1), Some(target), "target drifted");
    }

    // The HIGHER slot holds its altitude rather than climbing, which is what
    // makes the climber's target stationary.
    let mut c = controller("hold", &[9, 1]);
    c.tick(&own(9), &[fix(1, Ned::new(1.0, 0.0, 0.0))], t);
    let (offender, target) = c.hard_latch().expect("engaged");
    assert_eq!(offender, 1);
    assert!(
        (target - ALT).abs() < 1e-9,
        "the higher slot holds: {target}"
    );

    // Ordered by slot, so two converging drones always deconflict the same way
    // with no negotiation.
    assert!(climb_offset_m(1, 9) > climb_offset_m(2, 9));
    assert_eq!(climb_offset_m(9, 1), 0.0);
}

#[test]
fn the_hard_override_dwells_before_releasing() {
    let t = t0();
    let mut c = controller("hold", &[1, 2]);
    let close = fix(2, Ned::new(2.0, 0.0, 0.0));
    let clear = fix(2, Ned::new(60.0, 0.0, 0.0));
    assert_eq!(
        c.tick(&own(1), &[close], t).precedence,
        ModePrecedence::HardSeparation
    );
    // The geometry clears immediately, but the override holds for the dwell so it
    // cannot chatter at the loop rate.
    let out = c.tick(&own(1), &[clear], t + CONTROL_PERIOD);
    assert_eq!(out.precedence, ModePrecedence::HardSeparation, "chattered");
    assert!(c.hard_latch().is_some());
    let out = c.tick(&own(1), &[clear], t + HARD_DWELL);
    assert_ne!(out.precedence, ModePrecedence::HardSeparation);
    assert!(c.hard_latch().is_none());
    assert!(!out.emergency);
}

#[test]
fn a_trim_does_not_escalate_but_a_pin_does() {
    let t = t0();
    let goto_north = OperatorDirective::Goto {
        lat_deg: HOME.0 + 0.01,
        lon_deg: HOME.1,
        alt_m: ALT,
    };

    // Standing still 6 m short of a neighbour, commanded straight at it. The
    // barrier trims the 10 m/s down to the 2 m/s the margin affords — ordinary
    // traffic, and escalating here would freeze every drone in a dense lattice.
    let mut c = controller("hold", &[1, 2]);
    c.set_operator_directive(goto_north, t);
    let out = c.tick(&own(1), &[fix(2, Ned::new(6.0, 0.0, 0.0))], t);
    assert_eq!(out.precedence, ModePrecedence::Operator, "{out:?}");
    assert!(c.hard_latch().is_none(), "a trim is not an emergency");
    let s = out.setpoint.expect("still commanded, just slower");
    assert!(
        s.vn > 0.0 && (s.vn as f64) < 10.0,
        "trimmed, not zeroed: {s:?}"
    );

    // Same geometry, but already closing at the speed ceiling: the braking
    // distance has eaten the whole margin, so there is no horizontal solution
    // left. That is the deadlock the deterministic vertical rule exists for, and
    // it engages while the vehicle is still 6 m clear of the 4 m floor.
    let mut c = controller("hold", &[1, 2]);
    c.set_operator_directive(goto_north, t);
    let mut fast = own(1);
    fast.vn = MAX_COMMAND_SPEED_MPS;
    let out = c.tick(&fast, &[fix(2, Ned::new(6.0, 0.0, 0.0))], t);
    assert_eq!(
        out.precedence,
        ModePrecedence::HardSeparation,
        "pinned with 6 m still to go: {out:?}"
    );
    assert_eq!(c.hard_latch().map(|l| l.0), Some(2));
    let s = out.setpoint.expect("engaged");
    // The operator's 10 m/s northward plan is gone; only the repulsive push
    // southward and the climb remain.
    assert!(s.vn < 0.0 && (s.vn as f64) > -0.5, "{s:?}");
    assert_eq!(s.ve, 0.0);
    assert!(s.vd < 0.0);
}

// --------------------------------------------------------------- operator

#[test]
fn an_operator_directive_expires_and_hands_back_to_the_autonomy_layer() {
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    c.set_target(Some((HOME.0 + 0.01, HOME.1, ALT)));
    let far = fix(2, Ned::new(60.0, 0.0, 0.0));
    c.set_operator_directive(OperatorDirective::Hold, t);
    assert_eq!(
        c.tick(&own(1), &[far], t).precedence,
        ModePrecedence::Operator
    );
    assert_eq!(
        c.tick(
            &own(1),
            &[far],
            t + OPERATOR_DIRECTIVE_TTL - Duration::from_millis(1)
        )
        .precedence,
        ModePrecedence::Operator
    );
    assert_eq!(
        c.tick(&own(1), &[far], t + OPERATOR_DIRECTIVE_TTL)
            .precedence,
        ModePrecedence::Flocking,
        "a directive whose issuer went quiet must not outrank autonomy forever"
    );
    // Re-issuing renews it.
    let late = t + OPERATOR_DIRECTIVE_TTL;
    c.set_operator_directive(OperatorDirective::Hold, late);
    assert_eq!(
        c.tick(&own(1), &[far], late).precedence,
        ModePrecedence::Operator
    );
    c.clear_operator_directive();
    assert_eq!(
        c.tick(&own(1), &[far], late).precedence,
        ModePrecedence::Flocking
    );
}

#[test]
fn an_operator_goto_commands_toward_the_point_and_saturates() {
    let t = t0();
    let mut c = controller("hold", &[1, 2]);
    // 1000 m north.
    let (lat, lon, alt) = GeoOrigin::new(HOME.0, HOME.1, ALT).to_geo(Ned::new(1000.0, 0.0, 0.0));
    c.set_operator_directive(
        OperatorDirective::Goto {
            lat_deg: lat,
            lon_deg: lon,
            alt_m: alt,
        },
        t,
    );
    let out = c.tick(&own(1), &[fix(2, Ned::new(0.0, 300.0, 0.0))], t);
    let s = out.setpoint.expect("commanded");
    assert!(s.vn > 0.0 && s.ve.abs() < 1e-3, "{s:?}");
    let speed = (s.vn as f64).hypot(s.ve as f64);
    assert!(
        (speed - MAX_COMMAND_SPEED_MPS).abs() < 1e-3,
        "must saturate, got {speed}"
    );
    assert_eq!(out.precedence, ModePrecedence::Operator);
}

#[test]
fn an_operator_hold_commands_a_stop() {
    let t = t0();
    let mut c = controller("flocking", &[1, 2]);
    c.set_target(Some((HOME.0 + 0.05, HOME.1, ALT)));
    c.set_operator_directive(OperatorDirective::Hold, t);
    let out = c.tick(&own(1), &[fix(2, Ned::new(60.0, 0.0, 0.0))], t);
    let s = out.setpoint.expect("a hold is still a command");
    assert!(
        (s.vn as f64).hypot(s.ve as f64) < 1e-6,
        "a hold must cancel the flocking pull, not blend with it: {s:?}"
    );
    assert_eq!(out.precedence, ModePrecedence::Operator);
}

// ----------------------------------------------------------------- config

#[test]
fn config_drives_the_mode_the_formation_and_the_gains() {
    let slots = [1u8, 2, 3, 4];
    let c = SwarmController::new(
        &SwarmControlConfig {
            enabled: true,
            mode: "formation".into(),
            default_formation: "wedge".into(),
            default_spacing: 20,
            ..Default::default()
        },
        &slots,
    );
    assert_eq!(c.mode(), SwarmMode::Formation);
    assert_eq!(c.formation().name, "wedge");
    assert_eq!(c.formation().offsets.len(), 4);
    let apex = c.formation().station(1).expect("slot 1");
    let wing = c.formation().station(2).expect("slot 2");
    assert!(wing.n < apex.n, "the apex leads: {apex:?} vs {wing:?}");
    // Assert the SPACING, not a station coordinate: the table is translated so
    // its centroid is the origin, so no single offset equals the spacing.
    let stations: Vec<Ned> = (1..=4u8)
        .map(|s| c.formation().station(s).expect("registered"))
        .collect();
    let min = crate::separation::min_pairwise_distance(&stations).expect("four stations");
    assert!(min >= 20.0 * 0.999, "spacing honoured, got {min}");
    assert!(min < 20.0 * 1.5, "and not inflated, got {min}");
}

#[test]
fn reapplying_config_never_disarms_a_live_hard_override() {
    let t = t0();
    let mut c = controller("hold", &[1, 2]);
    c.tick(&own(1), &[fix(2, Ned::new(1.0, 0.0, 0.0))], t);
    let latch = c.hard_latch().expect("engaged");
    c.apply_config(&cfg("flocking"), &[1, 2]);
    assert_eq!(
        c.hard_latch(),
        Some(latch),
        "a config reload must not release the safety layer"
    );
    assert_eq!(c.mode(), SwarmMode::Flocking);
}

#[test]
fn set_formation_rebuilds_for_a_new_slot_set() {
    let mut c = controller("formation", &[1, 2]);
    c.set_formation(
        FormationName::Circle,
        &[1, 2, 3, 4, 5],
        12.0,
        FormationAnchor::Slot(1),
    );
    assert_eq!(c.formation().name, "circle");
    assert_eq!(c.formation().offsets.len(), 5);
    assert_eq!(c.formation().anchor, FormationAnchor::Slot(1));
}

// --------------------------------------------------------------- counters

#[test]
fn counters_separate_emissions_from_suppressions_and_engagements() {
    let t = t0();
    let mut c = controller("hold", &[1, 2]);
    assert_eq!(c.counters(), SwarmControlCounters::default());
    // One suppression (nothing to do).
    c.tick(&own(1), &[fix(2, Ned::new(200.0, 0.0, 0.0))], t);
    assert_eq!(c.counters().ticks_suppressed, 1);
    assert_eq!(c.counters().setpoints_emitted, 0);
    // One engagement plus one emission.
    c.tick(
        &own(1),
        &[fix(2, Ned::new(1.0, 0.0, 0.0))],
        t + CONTROL_PERIOD,
    );
    assert_eq!(c.counters().hard_engagements, 1);
    assert_eq!(c.counters().setpoints_emitted, 1);
    // Still engaged on the next tick: no second engagement counted.
    c.tick(
        &own(1),
        &[fix(2, Ned::new(1.0, 0.0, 0.0))],
        t + CONTROL_PERIOD * 2,
    );
    assert_eq!(c.counters().hard_engagements, 1);
    assert_eq!(c.counters().setpoints_emitted, 2);
}

// ------------------------------------------------------------------ frame

#[test]
fn a_neighbour_with_a_broken_fix_is_dropped_not_flown_into() {
    let t = t0();
    let mut c = controller("hold", &[1, 2, 3]);
    let mut bad = fix(2, Ned::new(2.0, 0.0, 0.0));
    bad.lat_deg = f64::NAN;
    // The broken fix would be a 2 m breach if it were believed.
    let out = c.tick(&own(1), &[bad], t);
    assert_eq!(out.precedence, ModePrecedence::Hold);
    assert!(!out.emergency, "a NaN fix must not raise a phantom alarm");
    assert_eq!(
        out.suppressed,
        Some(Suppression::NothingToDo),
        "the fix is discarded, leaving an empty table on a fresh clock"
    );
}

#[test]
fn every_emitted_setpoint_is_valid_and_in_the_relative_altitude_frame() {
    let t = t0();
    for mode in ["hold", "flocking", "formation"] {
        let mut c = controller(mode, &[1, 2, 3]);
        c.set_target(Some((HOME.0 + 0.02, HOME.1 + 0.02, ALT + 10.0)));
        for k in 0..40u32 {
            let out = c.tick(
                &own(1),
                &[
                    fix(2, Ned::new(5.0, 3.0, 0.0)),
                    fix(3, Ned::new(-12.0, 20.0, -4.0)),
                ],
                t + CONTROL_PERIOD * k,
            );
            if let Some(s) = out.setpoint {
                assert!(s.is_valid(), "mode {mode} tick {k}: {s:?}");
                assert_eq!(s.coordinate_frame(), 6);
                let speed =
                    ((s.vn as f64).powi(2) + (s.ve as f64).powi(2) + (s.vd as f64).powi(2)).sqrt();
                assert!(
                    speed <= MAX_COMMAND_SPEED_MPS + 1e-6,
                    "mode {mode} tick {k} speed {speed}"
                );
            }
        }
    }
}
