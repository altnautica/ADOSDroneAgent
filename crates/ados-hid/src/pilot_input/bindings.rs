//! The Settings › Controls binding file (the export/import document and the body
//! of `PUT /api/v1/ground-station/input/bindings`): validation, the locked-action
//! check, and the subset the ground station handles natively.

use serde::{Deserialize, Serialize};

use super::spec::{ds4, Binding};

/// The document `kind`.
pub const CONTROLS_KIND: &str = "ados-controls";

/// The actions the ground station executes itself from the pad (sticks aside):
/// the safety set. Every other binding belongs to the cockpit in the browser.
pub const NATIVE_ACTIONS: [&str; 5] = ["pause", "rth", "rth_cancel", "arm", "kill"];

/// The ground-station emergency lane switch: L2 + R2 + Share held 2 s hands the
/// RC link to ELRS. Native only, never rebindable, so it is not a spec row.
pub const ELRS_EMERGENCY_ACTION: &str = "control_mode:elrs";

/// A whole binding document. `bindings` is the complete effective list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlsDoc {
    pub v: u32,
    pub kind: String,
    pub pad: String,
    pub stick_mode: String,
    pub deadzone: f32,
    pub profile: String,
    pub bindings: Vec<Binding>,
}

/// Why a document was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocError {
    /// The shape is wrong (bad `v`, `kind`, `pad`, `stick_mode`, `deadzone`, a row).
    Invalid(String),
    /// Locked actions without their binding: `"<action>:<gesture>[:combo]"` each.
    MissingLocked(Vec<String>),
}

impl ControlsDoc {
    /// The document equivalent of the spec defaults ("ADOS default").
    pub fn ados_default() -> Self {
        let s = ds4();
        Self {
            v: 1,
            kind: CONTROLS_KIND.to_string(),
            pad: "ds4".to_string(),
            stick_mode: s.default_mode.clone(),
            deadzone: s.conditioning.radial_deadzone,
            profile: "ados_default".to_string(),
            bindings: s.bindings.clone(),
        }
    }

    /// Check the shape and the locked actions.
    pub fn validate(&self) -> Result<(), DocError> {
        let s = ds4();
        let bad = |m: String| Err(DocError::Invalid(m));
        if self.v != 1 {
            return bad(format!("unsupported v {}", self.v));
        }
        if self.kind != CONTROLS_KIND {
            return bad(format!("kind must be {CONTROLS_KIND}"));
        }
        if !matches!(self.pad.as_str(), "ds4" | "generic") {
            return bad(format!("unknown pad {}", self.pad));
        }
        if !s.stick_modes.contains_key(&self.stick_mode) {
            return bad(format!("unknown stick_mode {}", self.stick_mode));
        }
        let (lo, hi) = (s.conditioning.deadzone_min, s.conditioning.deadzone_max);
        if !(lo..=hi).contains(&self.deadzone) {
            return bad(format!("deadzone must be within {lo}..{hi}"));
        }
        for (i, b) in self.bindings.iter().enumerate() {
            let control_ok =
                |c: &str| super::pad::bit(c).is_some() || matches!(c, "stick_l" | "stick_r");
            if !control_ok(&b.control) || !b.with.iter().all(|c| control_ok(c)) {
                return bad(format!("bindings[{i}]: unknown control"));
            }
            match b.gesture.as_str() {
                "tap" | "press" | "double_tap" | "analog" => {}
                "hold" if b.hold_ms.is_some_and(|ms| ms > 0) => {}
                "hold" => return bad(format!("bindings[{i}]: hold needs hold_ms")),
                g => return bad(format!("bindings[{i}]: unknown gesture {g}")),
            }
        }
        // A locked action is kept when some row binds it with its gesture and
        // combo shape, on buttons that can be pressed, held at least as long as
        // the default, under the default's condition (arm keeps the centred-stick
        // guard). It may sit on any control.
        let pressable = |c: &str| super::pad::bit(c).is_some();
        let missing: Vec<String> = s
            .locked_actions
            .iter()
            .filter(|l| {
                let default = s.bindings.iter().find(|d| {
                    d.locked
                        && d.action == l.action
                        && d.gesture == l.gesture
                        && d.with.is_empty() != l.combo
                });
                !self.bindings.iter().any(|b| {
                    b.action == l.action
                        && b.gesture == l.gesture
                        && b.with.is_empty() != l.combo
                        && pressable(&b.control)
                        && b.with.iter().all(|c| pressable(c))
                        && default.is_none_or(|d| {
                            b.when == d.when && b.hold_ms.unwrap_or(0) >= d.hold_ms.unwrap_or(0)
                        })
                })
            })
            .map(|l| {
                let mut id = format!("{}:{}", l.action, l.gesture);
                if l.combo {
                    id.push_str(":combo");
                }
                id
            })
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(DocError::MissingLocked(missing))
        }
    }

    /// The rows the ground station runs natively, plus the emergency lane switch.
    pub fn native_bindings(&self) -> Vec<Binding> {
        let mut rows: Vec<Binding> = self
            .bindings
            .iter()
            .filter(|b| NATIVE_ACTIONS.contains(&b.action.as_str()))
            .filter(|b| matches!(b.gesture.as_str(), "tap" | "press" | "hold"))
            .cloned()
            .collect();
        rows.push(Binding {
            control: "share".to_string(),
            with: vec!["l2".to_string(), "r2".to_string()],
            gesture: "hold".to_string(),
            hold_ms: Some(2000),
            when: None,
            action: ELRS_EMERGENCY_ACTION.to_string(),
            rumble: Some("ramp".to_string()),
            locked: true,
        });
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_document_is_valid() {
        assert_eq!(ControlsDoc::ados_default().validate(), Ok(()));
    }

    #[test]
    fn removing_pause_is_refused_with_its_name() {
        let mut doc = ControlsDoc::ados_default();
        doc.bindings
            .retain(|b| !(b.action == "pause" && b.with.is_empty()));
        // Options + Circle still binds pause, but it is a combo; the locked
        // pause must stay a single control.
        let gesture = &ds4()
            .locked_actions
            .iter()
            .find(|l| l.action == "pause")
            .unwrap()
            .gesture;
        match doc.validate() {
            Err(DocError::MissingLocked(m)) => assert_eq!(m, vec![format!("pause:{gesture}")]),
            other => panic!("expected missing pause, got {other:?}"),
        }
    }

    #[test]
    fn a_locked_binding_may_move_to_another_control() {
        let mut doc = ControlsDoc::ados_default();
        for b in doc.bindings.iter_mut().filter(|b| b.action == "kill") {
            b.control = "cross".to_string();
        }
        assert_eq!(doc.validate(), Ok(()));
        let kill = doc
            .native_bindings()
            .into_iter()
            .find(|b| b.action == "kill")
            .unwrap();
        assert_eq!(kill.control, "cross");
    }

    #[test]
    fn weakened_locked_rows_count_as_missing() {
        let weaken = |f: &dyn Fn(&mut Binding)| {
            let mut doc = ControlsDoc::ados_default();
            doc.bindings
                .iter_mut()
                .filter(|b| b.action == "arm")
                .for_each(f);
            match doc.validate() {
                Err(DocError::MissingLocked(m)) => m,
                other => panic!("expected refusal, got {other:?}"),
            }
        };
        let arm = vec!["arm:hold:combo".to_string()];
        assert_eq!(weaken(&|b| b.hold_ms = Some(500)), arm, "shorter hold");
        assert_eq!(
            weaken(&|b| b.when = None),
            arm,
            "centred-stick guard dropped"
        );
        assert_eq!(
            weaken(&|b| b.control = "stick_l".into()),
            arm,
            "not a button"
        );
        assert_eq!(
            weaken(&|b| b.with = vec!["stick_r".into()]),
            arm,
            "not a button"
        );
        // A longer hold is fine.
        let mut doc = ControlsDoc::ados_default();
        doc.bindings
            .iter_mut()
            .filter(|b| b.action == "arm")
            .for_each(|b| b.hold_ms = Some(3000));
        assert_eq!(doc.validate(), Ok(()));
    }

    #[test]
    fn shape_errors_are_invalid() {
        let mut doc = ControlsDoc::ados_default();
        doc.deadzone = 0.5;
        assert!(matches!(doc.validate(), Err(DocError::Invalid(_))));
        let mut doc = ControlsDoc::ados_default();
        doc.bindings[0].control = "turbo".to_string();
        assert!(matches!(doc.validate(), Err(DocError::Invalid(_))));
    }

    #[test]
    fn native_subset_is_the_safety_set_plus_the_elrs_switch() {
        let rows = ControlsDoc::ados_default().native_bindings();
        assert!(rows
            .iter()
            .all(|b| NATIVE_ACTIONS.contains(&b.action.as_str())
                || b.action == ELRS_EMERGENCY_ACTION));
        for a in ["pause", "rth", "arm", "kill", ELRS_EMERGENCY_ACTION] {
            assert!(rows.iter().any(|b| b.action == a), "{a}");
        }
    }
}
