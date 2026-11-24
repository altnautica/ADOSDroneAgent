//! The alert list in the Pilot block: one section per alert group in the
//! spec's order, each readiness item's parameters as `NAME  current → wanted`
//! (six, then the rest counted) and a dim line naming where to fix it.

use serde_json::{json, Value};

use super::{alert_block, app_with_frame, stall_frame};
use crate::ui::tests::draw;

/// A quiet frame on the ground with readiness items carrying detail.
fn frame_with_detail() -> Value {
    let mut frame = stall_frame();
    let o = frame.as_object_mut().unwrap();
    o.remove("mission_stall");
    o.remove("banner");
    frame["armed"] = json!(false);
    frame["landed"] = json!(true);
    frame["statustext"] = json!([]);
    let params: Vec<Value> = [
        ("MAV_GCS_SYSID", json!(255), "1"),
        ("FS_GCS_ENABLE", Value::Null, "5"),
        ("P3", json!(0), "1"),
        ("P4", json!(0), "1"),
        ("P5", json!(0), "1"),
        ("P6", json!(0), "1"),
        ("P7", json!(0), "1"),
        ("P8", json!(0), "1"),
    ]
    .into_iter()
    .map(|(name, have, want)| json!({"name": name, "have": have, "want": want}))
    .collect();
    frame["readiness"] = json!([
        {"id": "rangefinder", "ok": false, "severity": "warn", "msg": "No rangefinder",
         "fix": {"kind": "settings", "section": "sensors"}},
        {"id": "fc_profile", "ok": false, "severity": "block", "msg": "FC params differ",
         "params": params, "fix": {"kind": "apply_profile"}},
    ]);
    frame
}

/// The index of the first line in `block` holding `needle`.
fn at(block: &[&str], needle: &str, text: &str) -> usize {
    block
        .iter()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no {needle:?}\n{text}"))
}

#[test]
fn alerts_list_by_section_with_parameter_and_fix_lines() {
    let mut app = app_with_frame(1, frame_with_detail());
    let text = draw(&mut app, 220, 90);
    let block = alert_block(&text);
    let setup = at(&block, "Flight controller setup", &text);
    let item = at(&block, "FC params differ", &text);
    let first = at(&block, "MAV_GCS_SYSID  255 → 1", &text);
    let unread = at(&block, "FS_GCS_ENABLE  — → 5", &text);
    let more = at(&block, "… 2 more", &text);
    let fix = at(&block, "Fix: Aircraft setup", &text);
    let sensors = at(&block, "Sensors", &text);
    let range = at(&block, "No rangefinder", &text);
    let settings = at(&block, "Fix: Settings › sensors", &text);
    assert!(
        setup < item && item < first && first < unread && unread < more && more < fix,
        "the setup section reads title, item, parameters, fold, fix\n{text}"
    );
    assert!(
        fix < sensors && sensors < range && range < settings,
        "the sensors section follows setup in the spec order\n{text}"
    );
    assert!(!text.contains("P7  0 → 1"), "past six the rest fold\n{text}");
    assert!(text.contains("P6  0 → 1"), "{text}");
}
