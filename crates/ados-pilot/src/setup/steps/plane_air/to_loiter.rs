//! The shared lead-in to every fixed-wing air step: a GUIDED loiter over the
//! tuning box.

use crate::setup::adapters::Mode;
use crate::setup::fcstate::Class;
use crate::setup::flight::{
    self, airborne, fixedwing_flight, height, plane_loiter, Hop, Leg, Transition,
    PLANE_LOITER_ALT_M,
};
use crate::setup::step::Cx;

const VTOL_CLIMB_M: f32 = 30.0;

/// Gets the aircraft to a fixed-wing loiter over the box: takes off if on
/// the ground (TAKEOFF mode; VTOL: vertical take-off then transition), or
/// re-centres the loiter if already flying.
#[derive(Debug, Clone)]
pub struct ToLoiter {
    hop: Hop,
    trans: Transition,
    sent_ms: u64,
    pub ready: bool,
    pub failed: Option<&'static str>,
}

impl Default for ToLoiter {
    fn default() -> Self {
        ToLoiter {
            hop: Hop::default(),
            trans: Transition::Idle,
            sent_ms: 0,
            ready: false,
            failed: None,
        }
    }
}

impl ToLoiter {
    /// Takes off only from the ground: an armed aircraft that is neither on
    /// the ground nor airborne (a plane skimming slowly, a landed state the FC
    /// does not report) gets no take-off command.
    pub fn start(&mut self, cx: &mut Cx) {
        *self = ToLoiter::default();
        // A lone step started in flight has no take-off to centre the box on.
        flight::anchor_box(cx);
        if airborne(cx) {
            if fixedwing_flight(cx) {
                plane_loiter(cx, PLANE_LOITER_ALT_M);
                self.sent_ms = cx.now;
            } else {
                self.trans = Transition::to_fw(cx);
            }
        } else if cx.fc.armed && !cx.fc.on_ground() {
            self.failed = Some("flight_state");
        } else {
            let alt = if cx.adapter.class == Class::Vtol {
                VTOL_CLIMB_M
            } else {
                cx.params
                    .get("TKOFF_ALT")
                    .filter(|a| *a > 5.0)
                    .unwrap_or(50.0)
            };
            self.hop.start(cx, alt);
        }
    }

    pub fn tick(&mut self, cx: &mut Cx) {
        if self.ready || self.failed.is_some() {
            return;
        }
        if !matches!(self.hop.leg, Leg::Idle) {
            self.hop.tick(cx);
            match self.hop.leg {
                Leg::Failed(w) => self.failed = Some(w),
                Leg::Holding => {
                    self.hop.leg = Leg::Idle;
                    if cx.adapter.class == Class::Vtol {
                        self.trans = Transition::to_fw(cx);
                    } else {
                        plane_loiter(cx, PLANE_LOITER_ALT_M);
                        self.sent_ms = cx.now;
                    }
                }
                _ => {}
            }
            return;
        }
        match self.trans {
            Transition::ToFw { .. } | Transition::ToMc { .. } => {
                self.trans.tick(cx);
                return;
            }
            Transition::Failed => {
                self.failed = Some("transition_failed");
                return;
            }
            Transition::Done => {
                self.trans = Transition::Idle;
                plane_loiter(cx, PLANE_LOITER_ALT_M);
                self.sent_ms = cx.now;
            }
            Transition::Idle => {}
        }
        if self.sent_ms == 0 {
            return;
        }
        let near = height(cx).is_some_and(|h| (h - PLANE_LOITER_ALT_M).abs() < 10.0);
        if near && flight::in_mode(cx, Mode::Guided) {
            self.ready = true;
        } else if cx.now.saturating_sub(self.sent_ms) > 5000 && !flight::in_mode(cx, Mode::Guided) {
            plane_loiter(cx, PLANE_LOITER_ALT_M);
            self.sent_ms = cx.now;
        } else if cx.now.saturating_sub(self.sent_ms) > 120_000 {
            self.failed = Some("loiter_timeout");
        }
    }
}
