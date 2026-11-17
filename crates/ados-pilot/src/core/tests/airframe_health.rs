//! Airframe health in the pilot: the `airframe_balance` readiness row from
//! what a lift-off and the hover after it found, a blocked airframe refused a
//! launch, and the operator's reset.

use ados_protocol::pilot::Severity;
use serde_json::json;

use super::harness::World;
use crate::airframe::{LiftoffOutcome, Record};
use crate::core::Reply;
use crate::liftoff::sim::Airframe;

/// A test world whose operator surface stays connected through a long
/// lift-off, on a quad X with the World Engine x500's 0.174 m half-spacing,
/// its FC identity known.
fn world() -> World {
    let mut w = World::new();
    w.p.settings.pilot.link_loss_s = 300;
    w.p.settings.pilot.supervised_liftoff = true;
    w.p.settings.pilot.prop_tip_radius_m = Some(0.373);
    w.p.fc.params.insert("FRAME_TYPE".into(), 1.0);
    w.p.load_airframe("fc-a", None, Some(10.0));
    w
}

fn launch(w: &mut World) -> Reply {
    w.action_args("launch", json!({"after": "hold", "alt_m": 3.0}))
}

fn refused(r: &Reply) -> Option<String> {
    match r {
        Reply::Action(a) if !a.ok => a.err.clone(),
        _ => None,
    }
}

fn balance_row(w: &World) -> Option<ados_protocol::pilot::Readiness> {
    w.p.readiness(w.now)
        .into_iter()
        .find(|r| r.id == "airframe_balance")
}

/// The airframe's last supervised lift-off stopped for a saturated motor.
fn blocked() -> Record {
    Record {
        saturated: true,
        ..Record::default()
    }
}

#[test]
fn a_blocked_airframe_refuses_the_launch() {
    let mut w = World::new();
    w.p.settings.pilot.link_loss_s = 300;
    w.p.load_airframe("fc-a", Some(blocked()), Some(10.0));
    let row = balance_row(&w).expect("the airframe_balance row");
    assert_eq!((row.ok, row.severity), (false, Severity::Block));
    let r = launch(&mut w);
    assert_eq!(
        refused(&r).as_deref(),
        Some("not_ready:airframe_balance"),
        "{r:?}"
    );
    w.run(500);
    assert!(!w.armed, "nothing was armed");
}

#[test]
fn a_blocked_airframe_refuses_the_supervised_launch_in_every_control_mode() {
    for mode in ["fc_manual", "elrs"] {
        let mut w = World::new();
        w.p.settings.pilot.link_loss_s = 300;
        w.p.settings.pilot.supervised_liftoff = true;
        w.p.load_airframe("fc-a", Some(blocked()), Some(10.0));
        let r = w.action(&format!("control_mode:{mode}"));
        assert!(matches!(&r, Reply::Action(a) if a.ok), "{r:?}");
        let r = launch(&mut w);
        assert_eq!(
            refused(&r).as_deref(),
            Some("not_ready:airframe_balance"),
            "{mode}: {r:?}"
        );
        w.run(500);
        assert!(!w.armed, "{mode}: nothing was armed");
    }
}

#[test]
fn a_saturated_lift_off_blocks_the_next_launch_until_the_operator_clears_it() {
    // The lift-off stopped on the ground with its busiest motor at 95 % of
    // its range (the lift-off tests fly that stop on nose50).
    let mut w = world();
    w.p.airframe.note_liftoff(
        &LiftoffOutcome {
            i: None,
            thrust: 0.69,
            max_motor: 0.95,
            saturated: true,
        },
        w.now,
    );
    let row = balance_row(&w).expect("the airframe_balance row");
    assert_eq!((row.ok, row.severity), (false, Severity::Block));
    assert_eq!(
        row.msg,
        "Can't take off: no climb margin. Motors at 95 % at take-off. Lighten the drone or rebalance it."
    );
    // Filed under the FC identity.
    let saved = w.p.airframe.save().unwrap();
    assert_eq!(saved.uid, "fc-a");
    assert!(saved.record.saturated && saved.record.rev >= 1);
    let r = launch(&mut w);
    assert_eq!(
        refused(&r).as_deref(),
        Some("not_ready:airframe_balance"),
        "{r:?}"
    );
    // "I rebalanced it".
    let r = w.action("airframe_reset");
    assert!(matches!(&r, Reply::Action(a) if a.ok), "{r:?}");
    assert_eq!(balance_row(&w).map(|r| r.ok), Some(true));
    let cleared = w.p.airframe.save().unwrap();
    assert!(!cleared.record.saturated && cleared.record.rev == saved.record.rev + 1);
}

#[test]
fn the_hover_after_a_lift_off_teaches_the_trim_and_asks_to_rebalance() {
    let mut w = world().with_airframe(Airframe::new(0.55, [0.0, 0.25]));
    let r = launch(&mut w);
    assert!(matches!(&r, Reply::Action(a) if a.ok), "{r:?}");
    w.fly_takeoff(60_000);
    let probed = w.p.airframe.trim().expect("the lift-off's trim");
    // Steady hover at the launch height.
    w.run(6000);
    let learned = w.p.airframe.trim().unwrap();
    assert!(
        learned.samples > probed.samples + 50,
        "{probed:?} {learned:?}"
    );
    assert!((learned.i[1] - 0.25).abs() < 0.05, "{learned:?}");
    let row = balance_row(&w).expect("a rebalance warning");
    assert_eq!((row.ok, row.severity), (false, Severity::Warn));
    assert!(
        row.msg.starts_with("Balance: nose-heavy. Off by about ")
            && row.msg.ends_with(" mm. Move the battery back."),
        "{}",
        row.msg
    );
    // The disarm files it.
    let rev = w.p.airframe.record().rev;
    w.armed = false;
    w.landed = true;
    w.run(200);
    assert_eq!(w.p.airframe.record().rev, rev + 1);
}

#[test]
fn the_reset_is_refused_in_the_air() {
    let mut w = world();
    w.armed = true;
    w.landed = false;
    w.run(100);
    let r = w.action("airframe_reset");
    assert_eq!(refused(&r).as_deref(), Some("airborne"), "{r:?}");
}

#[test]
fn the_lift_off_switch_row_reads_the_spec_words() {
    let mut w = world();
    w.p.settings.pilot.supervised_liftoff = false;
    let row =
        w.p.readiness(w.now)
            .into_iter()
            .find(|r| r.id == "supervised_liftoff")
            .expect("the supervised_liftoff row");
    let spec: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../web-shared/src/cockpit-spec/setup-strings.json"
    ))
    .unwrap();
    assert_eq!(
        row.msg,
        spec["strings"]["readiness.supervised_liftoff.off"]
            .as_str()
            .unwrap()
    );
    assert_eq!(row.severity, Severity::Warn);
}
