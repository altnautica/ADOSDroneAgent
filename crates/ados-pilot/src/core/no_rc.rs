//! Flight with no RC transmitter in the pilot: the declaration, the
//! operator instances leaving the link-loss watch, the RC-seen latch and the
//! posture's readiness items (see [`crate::no_rc`] and
//! [`crate::link_watch`]).

use ados_protocol::pilot::{ControlMode, PlaceClass, RcReceiver, Readiness, Severity};
use serde_json::json;

use super::{Out, Pilot};
use crate::config::LinkLossAction;
use crate::flows::banner;

/// A link-loss wait above this, seconds, warns with no RC transmitter.
pub const NO_RC_LINK_LOSS_WARN_S: u32 = 15;

/// RC_CHANNELS that a receiver feeds: the first four channels in range.
fn rc_valid(ch: &[u16; 18]) -> bool {
    ch.iter().take(4).all(|v| (800..=2200).contains(v))
}

impl Pilot {
    /// The operator's RC declaration in force.
    pub fn rc_receiver(&self) -> RcReceiver {
        self.settings.pilot.rc_receiver
    }

    /// A `bye` from instance `sid`: it leaves the link-loss watch only while
    /// disarmed or after another instance took the stick session; any other
    /// `bye` counts as silence, so its timer runs on.
    pub(super) fn on_bye(&mut self, src: &str, sid: Option<&str>) {
        let Some(sid) = sid else {
            return;
        };
        let honoured =
            self.failsafe
                .instances
                .bye_honoured(sid, self.fc.armed, self.sessions.owner.as_ref());
        if honoured {
            self.failsafe.instances.leave(sid);
        }
        tracing::info!(target: "ados_pilot", src, honoured, "pilot_surface_bye");
    }

    /// `rc_receiver:<present|absent>`, disarmed only. `absent` needs ADOS
    /// control, ArduCopter and an open place ([`crate::no_rc::declare_refusal`]).
    pub(super) fn set_rc_receiver(&mut self, value: &str) -> Result<(), String> {
        let rc: RcReceiver = serde_json::from_value(json!(value)).map_err(|_| "bad_value")?;
        if self.fc.armed {
            return Err("armed".into());
        }
        if rc == RcReceiver::Absent {
            if let Some(err) = crate::no_rc::declare_refusal(
                self.fc.armed,
                self.control_mode(),
                self.fc.firmware,
                self.fc.vehicle_class(),
                self.settings.pilot.place_class,
            ) {
                return Err(err.into());
            }
        }
        self.settings.pilot.rc_receiver = rc;
        self.out.push(Out::Persist {
            section: "pilot",
            key: "rc_receiver",
            value: rc.as_str().into(),
        });
        tracing::info!(target: "ados_pilot", rc_receiver = rc.as_str(), "pilot_rc_receiver");
        Ok(())
    }

    /// With no RC transmitter, the control mode stays `ados` and the place
    /// stays open: the posture was declared for them.
    pub(super) fn no_rc_refusal(&self, kind: &str, value: &str) -> Option<&'static str> {
        if self.settings.pilot.rc_receiver.is_present() {
            return None;
        }
        let refused = match kind {
            "control_mode" => ControlMode::parse(value) != Some(ControlMode::Ados),
            "place_class" => value == PlaceClass::Confined.as_str(),
            _ => false,
        };
        refused.then_some("rc_receiver_absent")
    }

    /// Latch an RC receiver seen since the pilot connected to the FC, and,
    /// with no RC transmitter declared, alert when one appears in flight
    /// (readiness does not gate in flight).
    pub(super) fn watch_rc(&mut self, now_ms: u64) {
        if self.fc.rc.as_ref().is_some_and(|r| rc_valid(&r.v)) && !self.failsafe.rc_seen {
            self.failsafe.rc_seen = true;
            if !self.settings.pilot.rc_receiver.is_present() && self.fc.armed {
                self.banner = Some(banner("pilot.banner.rc_seen", &[], now_ms));
                tracing::warn!(target: "ados_pilot", "pilot_rc_seen_with_no_rc_declared");
            }
        }
    }

    /// The posture's readiness items, with no RC transmitter only.
    pub(super) fn no_rc_items(&self) -> Vec<Readiness> {
        let p = &self.settings.pilot;
        if p.rc_receiver.is_present() {
            return Vec::new();
        }
        let mut out = vec![Readiness {
            id: "rc_receiver".into(),
            ok: true,
            severity: Severity::Warn,
            msg: format!(
                "No controller: stop it from this phone: Pause, Land or Return. If the link drops for {} s it {}.",
                p.link_loss_s,
                match p.link_loss_action {
                    LinkLossAction::Rth => "returns",
                    LinkLossAction::Land => "lands",
                    LinkLossAction::Hold => "holds",
                }
            ),
        }];
        out.push(Readiness {
            id: "rc_seen".into(),
            ok: !self.failsafe.rc_seen,
            severity: Severity::Block,
            msg: if self.failsafe.rc_seen {
                "Controller detected: re-run setup so the controller's failsafe is on".into()
            } else {
                "No controller detected".into()
            },
        });
        if p.link_loss_action == LinkLossAction::Hold {
            out.push(Readiness {
                id: "link_loss_posture".into(),
                ok: false,
                severity: Severity::Block,
                msg: "No controller: a dropped link must return or land, not hold".into(),
            });
        } else if p.link_loss_s > NO_RC_LINK_LOSS_WARN_S {
            out.push(Readiness {
                id: "link_loss_posture".into(),
                ok: false,
                severity: Severity::Warn,
                msg: format!(
                    "No controller: a dropped link waits {} s ({NO_RC_LINK_LOSS_WARN_S} s or less advised)",
                    p.link_loss_s
                ),
            });
        }
        out
    }

    /// With no RC transmitter, the FC's mission must stay inside the pilot's
    /// caps ([`crate::config::PilotSection::max_dist`] and `max_alt`): the
    /// pilot does not clamp an FC-flown mission, so a launch into it or a
    /// resume is refused while any waypoint lies beyond either cap
    /// (`beyond_no_rc_caps`), or while the route cannot be read whole
    /// (`route_unchecked`). Waypoints flown "here" stay where the vehicle is.
    pub(super) fn no_rc_route_check(&self) -> Result<(), String> {
        if self.rc_receiver().is_present() {
            return Ok(());
        }
        let feed = self.route.feed();
        let unplaced = feed.items.iter().any(|i| i.ned.is_none() && !i.here);
        if feed.truncated || feed.stale || unplaced {
            tracing::warn!(target: "ados_pilot", reason = "route_unchecked", no_rc = true, "pilot_mission_refused");
            return Err("route_unchecked".into());
        }
        let home = self.fc.home_ne();
        let home_d = self.fc.home_local.map_or(0.0, |h| h[2]);
        let (max_dist, max_alt) = (
            self.settings.pilot.max_dist(),
            self.settings.pilot.max_alt(),
        );
        let beyond = feed.items.iter().find(|i| {
            i.ned.is_some_and(|p| {
                crate::geo::dist([p[0], p[1]], home) > max_dist || home_d - p[2] > max_alt
            })
        });
        if let Some(i) = beyond {
            tracing::warn!(
                target: "ados_pilot",
                reason = "beyond_no_rc_caps",
                seq = i.seq,
                ned = ?i.ned,
                max_dist_m = max_dist,
                max_alt_m = max_alt,
                "pilot_mission_refused"
            );
            return Err("beyond_no_rc_caps".into());
        }
        Ok(())
    }
}
