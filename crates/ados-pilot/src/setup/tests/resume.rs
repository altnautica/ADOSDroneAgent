//! A wizard running when the agent restarts is interrupted, not a dead end:
//! its records survive and `resume` continues at the first unfinished step.

use serde_json::{json, Value};

use super::{Sim, G};
use crate::setup::schema::{StepState, WizardState};
use crate::setup::Engine;

#[test]
fn an_agent_restart_interrupts_setup_and_resume_continues_it() {
    let mut s = Sim::copter();
    assert_eq!(s.e.start(G, s.now)["ok"], json!(true));
    s.wait_step("frame", StepState::WaitingUser);
    let detect = s.result(G, "detect");
    assert!(detect.is_object(), "detect has a result: {detect}");

    // The agent restarts: a fresh engine over the same store and the same FC.
    let Sim { fc, now, _dir, .. } = s;
    let mut s = Sim {
        e: Engine::new(_dir.path(), 1_794_268_800_000),
        fc,
        now,
        _dir,
    };
    s.until(|s| s.e.uid.is_some() && s.e.params.complete(), 20_000);
    assert_eq!(s.e.wizard_state(G), WizardState::Interrupted);
    assert_eq!(s.step_state(G, "frame"), StepState::WaitingUser);
    assert_eq!(s.step_state(G, "detect"), StepState::Done);
    let v = serde_json::to_value(s.e.view(s.now)).unwrap();
    let ground = v["wizards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == json!("ground"))
        .unwrap();
    assert_eq!(ground["state"], json!("interrupted"));
    // Resume lands on the frame step, live again.
    let r = s.e.resume(G, s.now);
    assert_eq!(r["ok"], json!(true), "{r}");
    assert_eq!(r["wizard"]["state"], json!("running"));
    s.run(200);
    let cur = s.e.current_view().expect("a step is running");
    assert_eq!(cur.step, "frame");
    let inputs = cur.args.get("inputs").and_then(Value::as_object);
    assert!(
        inputs.is_some_and(|i| !i.is_empty()),
        "frame inputs: {:?}",
        cur.args
    );
    assert!(
        cur.actions.iter().any(|a| a.id == "apply"),
        "{:?}",
        cur.actions
    );
    assert_eq!(s.result(G, "detect"), detect, "detect's result survived");

    // A second resume is refused: the wizard is running again.
    assert_eq!(s.e.resume(G, s.now)["ok"], json!(false));
}
