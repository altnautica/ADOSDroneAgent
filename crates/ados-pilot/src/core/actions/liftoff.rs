//! The pilot's side of the supervised lift-off ([`crate::liftoff`]): the FC
//! set up for it before arming, the frames it sends each tick, the
//! operator's stops, and the spin-down and disarm when it stops on the
//! ground.

use ados_protocol::mavlink::ardupilotmega::MavCmd;
use ados_protocol::pilot::{Readiness, Severity, VehicleClass};

use super::STEP_TIMEOUT_MS;
use crate::core::Pilot;
use crate::fc::MASK_POS_VEL_YAW;
use crate::fc_profile::Firmware;
use crate::flows::{Launch, LaunchStep};
use crate::liftoff::{Cmd, Inputs, Liftoff, Motors, Stop, GCS_PID_ROLL_PITCH, GUID_OPTIONS_THRUST};

/// The FC setup is re-sent this often until the FC reports it…
const PREP_RESEND_MS: u64 = 1000;
/// …and the launch fails without it after this long.
const PREP_TIMEOUT_MS: u64 = 6000;
/// The rate-loop and motor streams the lift-off reads, Hz.
const STREAM_HZ: f32 = 25.0;
/// PID_TUNING and SERVO_OUTPUT_RAW message ids.
const STREAMS: [f32; 2] = [194.0, 36.0];
/// Rate-loop and motor data older than this is not used.
const RATES_FRESH_MS: u64 = 500;
/// A stopped lift-off holds thrust at zero this long before the disarm.
const SPINDOWN_MS: u64 = 300;
/// Ground contact: no higher than at rest by more than this (m), and not
/// climbing faster than this (m/s).
const GROUND_RISE_M: f32 = 0.05;
const GROUND_VZ_MPS: f32 = 0.2;
/// A supervised take-off fails after this long: the lift-off's own ground
/// limit plus the climb to the launch height.
const TAKEOFF_TIMEOUT_MS: u64 = 120_000;

impl Pilot {
    /// This launch lifts off under the pilot.
    pub(super) fn supervised_liftoff_applies(&self) -> bool {
        self.settings.pilot.supervised_liftoff
            && self.fc.firmware == Some(Firmware::Ardupilot)
            && self.fc.vehicle_class() == VehicleClass::Multirotor
    }

    /// `supervised_liftoff`, while the field-validation switch is off on an
    /// aircraft that would use it: every surface shows the FC's own
    /// take-off runs.
    pub(in crate::core) fn liftoff_switch_item(&self) -> Option<Readiness> {
        let would = self.fc.firmware == Some(Firmware::Ardupilot)
            && self.fc.vehicle_class() == VehicleClass::Multirotor;
        (would && !self.settings.pilot.supervised_liftoff).then(|| Readiness {
            id: "supervised_liftoff".into(),
            ok: false,
            severity: Severity::Warn,
            msg: "Standard take-off. Steady take-off is off.".into(),
        })
    }

    /// Before arming: the FC reads SET_ATTITUDE_TARGET thrust as thrust and
    /// streams its rate loop. `Ok(true)` once it reports both; the bits are
    /// ORed into the operator's own values.
    pub(super) fn liftoff_prepared(&mut self, l: &mut Launch, now_ms: u64) -> Result<bool, String> {
        let Some(s) = l.supervised.as_mut() else {
            return Ok(true);
        };
        let p = &self.fc.params;
        let has = |n: &str, bits: u32| p.get(n).is_some_and(|v| (*v as u32) & bits == bits);
        let ready = has("GUID_OPTIONS", GUID_OPTIONS_THRUST)
            && has("GCS_PID_MASK", GCS_PID_ROLL_PITCH)
            && p.contains_key("MOT_THST_HOVER");
        if ready {
            return Ok(true);
        }
        let (since, sent) = *s.prep.get_or_insert((now_ms, 0));
        if now_ms.saturating_sub(since) > PREP_TIMEOUT_MS {
            return Err("liftoff_fc_params".into());
        }
        if sent != 0 && now_ms.saturating_sub(sent) < PREP_RESEND_MS {
            return Ok(false);
        }
        s.prep = Some((since, now_ms.max(1)));
        let mut out = Vec::new();
        for (name, bits) in [
            ("GUID_OPTIONS", GUID_OPTIONS_THRUST),
            ("GCS_PID_MASK", GCS_PID_ROLL_PITCH),
        ] {
            match p.get(name) {
                Some(v) => out.push(self.tx().param_set(name, ((*v as u32) | bits) as f32)),
                None => out.push(self.tx().param_request(name)),
            }
        }
        if !p.contains_key("MOT_THST_HOVER") {
            out.push(self.tx().param_request("MOT_THST_HOVER"));
        }
        for m in out {
            self.mav(m);
        }
        self.liftoff_streams();
        Ok(false)
    }

    fn liftoff_streams(&mut self) {
        for id in STREAMS {
            let m = self.tx().command(
                MavCmd::MAV_CMD_SET_MESSAGE_INTERVAL,
                [id, 1e6 / STREAM_HZ, 0.0, 0.0, 0.0, 0.0, 0.0],
            );
            self.mav(m);
        }
    }

    /// The take-off step under supervision: the FC set up (it may have been
    /// armed already), GUIDED confirmed, then the lift-off each tick until it
    /// handed over and the launch height is reached.
    pub(super) fn drive_supervised_takeoff(&mut self, l: &mut Launch, elapsed: u64, now_ms: u64) {
        if l.error.is_some() {
            // A stopped lift-off keeps spinning down until it has disarmed.
            let _ = self.drive_liftoff(l, now_ms);
            return;
        }
        let lifting = l.supervised.as_ref().is_some_and(|s| s.liftoff.is_some());
        if !lifting {
            if !self.in_guided() {
                if elapsed > STEP_TIMEOUT_MS {
                    l.error = Some("mode_refused".into());
                }
                return;
            }
            match self.liftoff_prepared(l, now_ms) {
                Ok(true) => self.liftoff_streams(),
                Ok(false) => return,
                Err(e) => {
                    if self.fc.armed {
                        let m = self.tx().arm(false, false);
                        self.mav(m);
                    }
                    l.error = Some(e);
                    l.step_ms = now_ms;
                    return;
                }
            }
        }
        if let Err(e) = self.drive_liftoff(l, now_ms) {
            // Shown on the surfaces for the error window from now.
            l.error = Some(e);
            l.step_ms = now_ms;
            return;
        }
        let handed_over = l
            .supervised
            .as_ref()
            .and_then(|s| s.liftoff.as_ref())
            .is_some_and(Liftoff::handed_over);
        let stopped = l
            .supervised
            .as_ref()
            .and_then(|s| s.liftoff.as_ref())
            .is_some_and(|lo| lo.stopped().is_some());
        let height = self.agl(now_ms).unwrap_or(0.0);
        if handed_over && height >= 0.9 * l.alt_m {
            l.step = LaunchStep::After;
            l.step_ms = now_ms;
            self.launch_after(l, now_ms);
        } else if !self.fc.armed && !stopped {
            l.error = Some("disarmed".into());
        } else if elapsed > TAKEOFF_TIMEOUT_MS && !stopped {
            l.error = Some("takeoff_timeout".into());
        }
    }

    /// The hover thrust the lift-off ramps against: the airframe's learned
    /// hover, else the FC's `MOT_THST_HOVER`.
    pub(crate) fn liftoff_hover(&self) -> f32 {
        self.airframe
            .trim()
            .filter(|t| t.samples > 0 && t.hover_thr > 0.0)
            .map(|t| t.hover_thr)
            .or_else(|| self.fc.params.get("MOT_THST_HOVER").copied())
            .unwrap_or(0.35)
    }

    /// One tick of the take-off step under supervision. `Err` ends the
    /// launch (the stop is already handled).
    pub(super) fn drive_liftoff(&mut self, l: &mut Launch, now_ms: u64) -> Result<(), String> {
        let hover = self.liftoff_hover();
        let trim = self.airframe.trim();
        let inp = self.liftoff_inputs(now_ms);
        let link_lost = self.link_lost(now_ms);
        let Some(s) = l.supervised.as_mut() else {
            return Ok(());
        };
        let lo = s
            .liftoff
            .get_or_insert_with(|| Liftoff::new(trim, hover, l.alt_m, now_ms));
        if link_lost {
            lo.stop(Stop::Link);
        }
        let cmd = lo.step(&inp);
        if let Some(o) = lo.take_outcome() {
            self.airframe.note_liftoff(&o, now_ms);
            if o.saturated {
                s.detail =
                    self.airframe
                        .imbalance_words(&self.fc, inp.i.unwrap_or_default(), o.thrust);
            }
        }
        if let Some(stop) = lo.stopped() {
            if matches!(stop, Stop::Tipping | Stop::NoTrim) && s.detail.is_none() {
                let thrust = hover;
                s.detail = inp
                    .i
                    .and_then(|i| self.airframe.imbalance_words(&self.fc, i, thrust));
            }
            return self.liftoff_spindown(l, stop, &inp, now_ms);
        }
        match cmd {
            Some(Cmd::Attitude {
                roll,
                pitch,
                yaw,
                thrust,
            }) => {
                let m = self.tx().attitude_target(roll, pitch, yaw, thrust);
                self.mav(m);
            }
            Some(Cmd::Position { pos, vel, yaw }) => {
                let m = self.tx().setpoint(MASK_POS_VEL_YAW, pos, vel, yaw, 0.0);
                self.mav(m);
                self.setpoint = Some(pos);
            }
            None => {}
        }
        Ok(())
    }

    /// Stopped on the ground: thrust at zero at the attitude it has (holding
    /// any other attitude makes the rate loop fight the ground and can flip
    /// it), then a forced disarm once ground contact is confirmed, else the
    /// FC's Land.
    fn liftoff_spindown(
        &mut self,
        l: &mut Launch,
        stop: Stop,
        inp: &Inputs,
        now_ms: u64,
    ) -> Result<(), String> {
        let ground = self.liftoff_ground_contact(l, inp);
        let Some(s) = l.supervised.as_mut() else {
            return Ok(());
        };
        let since = *s.stopped_ms.get_or_insert(now_ms);
        if s.disarm_sent {
            return if self.fc.armed && self.in_guided() {
                Ok(())
            } else {
                Err(stop.code().into())
            };
        }
        if now_ms.saturating_sub(since) < SPINDOWN_MS {
            if let Some(a) = inp.att {
                let m = self.tx().attitude_target(a.roll, a.pitch, a.yaw, 0.0);
                self.mav(m);
            }
            return Ok(());
        }
        s.disarm_sent = true;
        tracing::warn!(target: "ados_pilot", reason = stop.code(), ground, "pilot_liftoff_stopped");
        if ground {
            let m = self.tx().arm(false, true);
            self.mav(m);
        } else {
            let fam = self.family();
            self.set_mode(fam.land(false), now_ms);
        }
        Ok(())
    }

    /// Ground contact, from the pilot's own evidence (the FC's landed state
    /// clears as soon as the thrust passes half of hover, skids still down):
    /// never lifted, no higher than at rest, not climbing, and a downward
    /// rangefinder (when fitted) reading no more than at rest.
    fn liftoff_ground_contact(&self, l: &Launch, inp: &Inputs) -> bool {
        let Some(lo) = l.supervised.as_ref().and_then(|s| s.liftoff.as_ref()) else {
            return false;
        };
        let (Some(rest), Some(now)) = (lo.start(), inp.local) else {
            return false;
        };
        let range_ok = match (lo.rest_agl(), self.fc.rangefinder_agl(inp.now_ms)) {
            (Some(r0), Some(r)) => r - r0 <= GROUND_RISE_M,
            _ => true,
        };
        !lo.lifted_ever()
            && rest.d - now.d <= GROUND_RISE_M
            && now.vd.abs() <= GROUND_VZ_MPS
            && range_ok
    }

    fn liftoff_inputs(&self, now_ms: u64) -> Inputs {
        let p = &self.fc.params;
        let n = match p.get("FRAME_CLASS").map(|v| *v as u32) {
            Some(2 | 5) => 6,
            Some(3 | 4 | 12 | 14) => 8,
            Some(7) => 3,
            _ => 4,
        };
        let pwm = |k: &str, d: f32| p.get(k).copied().filter(|v| *v > 0.0).unwrap_or(d);
        let (lo, hi) = (pwm("MOT_PWM_MIN", 1000.0), pwm("MOT_PWM_MAX", 2000.0));
        let motors = self
            .fc
            .rates
            .fresh_motors(n, now_ms, RATES_FRESH_MS)
            .filter(|o| !o.is_empty() && hi > lo)
            .map(|o| {
                let max = *o.iter().max().unwrap_or(&0);
                let min = *o.iter().min().unwrap_or(&0);
                let frac = |v: u16| ((f32::from(v) - lo) / (hi - lo)).clamp(0.0, 1.0);
                Motors {
                    max: frac(max),
                    min: frac(min),
                    spread_us: max - min,
                }
            });
        Inputs {
            now_ms,
            att: self
                .fc
                .attitude
                .filter(|s| now_ms.saturating_sub(s.at_ms) < RATES_FRESH_MS)
                .map(|s| s.v),
            local: self.fc.fresh_local(now_ms),
            i: self.fc.rates.fresh_i(now_ms, RATES_FRESH_MS),
            motors,
            agl: self.fc.rangefinder_agl(now_ms),
        }
    }

    /// The operator paused or stopped while the lift-off is still on the
    /// ground: it stops and disarms instead of the action's usual hold.
    /// `true` when it took the action.
    pub(super) fn liftoff_operator_stop(&mut self) -> bool {
        let Some(lo) = self
            .launch
            .as_mut()
            .and_then(|l| l.supervised.as_mut())
            .and_then(|s| s.liftoff.as_mut())
        else {
            return false;
        };
        if !lo.on_ground() {
            return false;
        }
        lo.stop(Stop::Operator);
        true
    }

    /// The surfaces' view of the lift-off, with the imbalance added to a
    /// stop's detail.
    pub(in crate::core) fn liftoff_view(l: &Launch) -> Option<ados_protocol::pilot::Liftoff> {
        let s = l.supervised.as_ref()?;
        let mut v = s.liftoff.as_ref()?.view();
        if let (Some(d), "stopped") = (&s.detail, v.phase.as_str()) {
            v.detail = format!("{} {d}", v.detail);
        }
        Some(v)
    }
}
