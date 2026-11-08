//! The flight-controller setup and tuning engine.
//!
//! Two wizards, `ground` (stationary, no props spinning) and `air` (flown by
//! the pilot, the operator able to take over at any time), whose steps
//! surfaces render generically from [`schema::StepView`]. Each step can also
//! run on its own later. State persists per FC (AUTOPILOT_VERSION `uid`), and
//! every parameter is snapshotted before the first parameter-changing step.
//!
//! [`Engine`] is a pure state machine: FC frames in, a tick, requests in;
//! MAVLink frames and pilot effects out through [`out::Outbox`].
//! [`service`] wires it to the pilot daemon.

pub mod adapters;
pub mod doublets;
pub mod fcstate;
pub mod flight;
pub mod measure;
pub mod out;
pub mod params;
pub mod schema;
pub mod service;
pub mod statustext;
pub mod step;
pub mod steps;
pub mod store;

#[cfg(test)]
mod fake_fc;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use ados_protocol::mavlink::ardupilotmega::{MavMessage, MavType};
use ados_protocol::mavlink::MavHeader;
use serde_json::{json, Map, Value};

use adapters::{Adapter, Mode};
use fcstate::{Class, FcState, Fw, IdSource};
use out::{Effect, Outbox};
use params::ParamStore;
use schema::{
    Action, Report, Running, SetupView, StepState, StepView, StreamFrame, WizardId, WizardState, WizardView,
};
use step::{Cx, PilotInputs, Step, WizardData};
use store::{iso_utc, AircraftState, Store};

/// Step frames go out at 5 Hz while a wizard runs.
pub const STREAM_PERIOD_MS: u64 = 200;
/// Air abort thresholds.
pub const ABORT_BATTERY_PCT: i8 = 30;
pub const ABORT_WIND_MPS: f32 = 8.0;
/// Operator stick deflection that takes over a non-blending air step.
pub const TAKEOVER_AXIS: f32 = 0.3;
/// PreArm texts count for `setup_needed` this long.
const PREARM_WINDOW_MS: u64 = 10_000;
/// AUTOPILOT_VERSION message id.
const MSG_AUTOPILOT_VERSION: u32 = 148;
/// Wait this long after AUTOPILOT_VERSION before filing under an identity, so
/// the raw frame's `uid2` (a separate channel) is in.
const IDENTITY_SETTLE_MS: u64 = 300;

struct Run {
    wizard: WizardId,
    steps: Vec<Box<dyn Step>>,
    idx: usize,
    /// Only `idx` runs (Settings › Aircraft setup re-run).
    single: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tuning {
    None,
    Requested,
    Held,
}

/// The landing or return an air abort flies.
struct AbortFlight {
    hop: flight::Hop,
    plane: bool,
}

pub struct Engine {
    store: Store,
    fc: FcState,
    params: ParamStore,
    out: Outbox,
    inputs: PilotInputs,
    /// `Some` once the FC's uid is known.
    uid: Option<String>,
    uid_source: Option<IdSource>,
    state: AircraftState,
    snapshot_body: Option<String>,
    run: Option<Run>,
    tuning: Tuning,
    abort_flight: Option<AbortFlight>,
    loading: bool,
    version_asked_ms: u64,
    last_stream_ms: u64,
    wizard_dirty: bool,
    /// The owner reported by the pilot is `tuning`.
    pilot_tuning: bool,
    epoch_offset_ms: u64,
}

/// The vehicle class from the heartbeat type and `Q_ENABLE`.
pub fn vehicle_class(mav_type: MavType, q_enable: Option<f32>) -> Class {
    use MavType::*;
    match mav_type {
        MAV_TYPE_FIXED_WING => {
            if q_enable.unwrap_or(0.0) >= 1.0 {
                Class::Vtol
            } else {
                Class::Fixedwing
            }
        }
        MAV_TYPE_VTOL_TAILSITTER_DUOROTOR
        | MAV_TYPE_VTOL_TAILSITTER_QUADROTOR
        | MAV_TYPE_VTOL_TILTROTOR
        | MAV_TYPE_VTOL_FIXEDROTOR
        | MAV_TYPE_VTOL_TAILSITTER
        | MAV_TYPE_VTOL_TILTWING
        | MAV_TYPE_VTOL_RESERVED5 => Class::Vtol,
        _ => Class::Multirotor,
    }
}

fn reply_err(err: &str) -> Value {
    json!({"ok": false, "err": err})
}

/// An optional step offers `skip` beside its first action (`fresh`: just
/// begun or retried) and beside `retry` once it has failed, so a failure
/// never strands the wizard on a step the aircraft does not need.
fn offer_skip(step: &mut dyn Step, fresh: bool) {
    let state = step.core().state;
    let wants = state == StepState::Failed || fresh && state == StepState::WaitingUser;
    if wants && step.optional() && !step.core().actions.iter().any(|a| a.id == "skip") {
        step.core_mut().actions.push(Action::secondary("skip"));
    }
}

impl Engine {
    pub fn new(dir: impl Into<PathBuf>, epoch_offset_ms: u64) -> Self {
        Engine {
            store: Store::new(dir.into()),
            fc: FcState::new(),
            params: ParamStore::new(),
            out: Outbox::new(),
            inputs: PilotInputs::default(),
            uid: None,
            uid_source: None,
            state: AircraftState { v: 1, ..Default::default() },
            snapshot_body: None,
            run: None,
            tuning: Tuning::None,
            abort_flight: None,
            loading: false,
            version_asked_ms: 0,
            last_stream_ms: 0,
            wizard_dirty: false,
            pilot_tuning: false,
            epoch_offset_ms,
        }
    }

    pub fn fc(&self) -> &FcState {
        &self.fc
    }

    pub fn params(&self) -> &ParamStore {
        &self.params
    }

    pub fn outbox(&mut self) -> &mut Outbox {
        &mut self.out
    }

    fn iso(&self, now: u64) -> String {
        iso_utc(self.epoch_offset_ms + now)
    }

    pub fn adapter(&self) -> Option<Adapter> {
        let fw = self.fc.firmware()?;
        let class = vehicle_class(self.fc.mav_type?, self.params.get("Q_ENABLE"));
        Some(Adapter::new(fw, class))
    }

    fn running(&self) -> bool {
        self.run.is_some() || self.abort_flight.is_some()
    }

    // ---------------------------------------------------------------- FC in

    pub fn on_fc(&mut self, sysid: u8, compid: u8, msg: &MavMessage, now: u64) {
        let header = MavHeader { system_id: sysid, component_id: compid, sequence: 0 };
        let was_linked = self.fc.heartbeat_ms.is_some();
        self.fc.ingest(&header, msg, now);
        if !was_linked && self.fc.heartbeat_ms.is_some() {
            self.out.target = (self.fc.sysid, self.fc.compid);
            self.params.set_target(self.fc.sysid, self.fc.compid, self.fc.firmware() == Some(Fw::Px4));
        }
        self.params.on_msg(msg, now);
        let Some(adapter) = self.adapter() else { return };
        if let Some(run) = self.run.as_mut() {
            let ground_done = self.state.ground.state == WizardState::Done;
            let Engine { fc, params, out, state, inputs, snapshot_body, .. } = self;
            let mut cx = Cx {
                now,
                wizard: run.wizard,
                fc,
                params,
                out,
                data: &mut state.data,
                adapter,
                inputs,
                ground_done,
                snapshot: snapshot_body.as_deref(),
            };
            if let Some(s) = run.steps.get_mut(run.idx) {
                s.on_msg(msg, &mut cx);
            }
        }
    }

    /// A raw AUTOPILOT_VERSION frame: its `uid2` (the decoded message lacks it).
    pub fn on_version_frame(&mut self, frame: &[u8]) {
        if let Some(u2) = fcstate::uid2_from_frame(frame) {
            self.fc.uid2 = Some(u2);
        }
    }

    /// File the setup state under the aircraft identity once AUTOPILOT_VERSION
    /// has settled (its raw `uid2` frame comes on a separate channel). The
    /// first identity sticks for the session.
    fn adopt_identity(&mut self, now: u64) {
        if self.uid.is_some() {
            return;
        }
        if self.fc.version_ms.is_none_or(|t| now.saturating_sub(t) < IDENTITY_SETTLE_MS) {
            return;
        }
        let Some((uid, source)) = self.fc.identity() else { return };
        self.state = self.store.load(&uid);
        self.state.uid_source = Some(source);
        self.snapshot_body = self
            .state
            .data
            .snapshot
            .as_deref()
            .and_then(|n| self.store.read_snapshot(&uid, n));
        if source == IdSource::Derived {
            tracing::info!(uid = %uid, "fc_setup_identity_derived");
        }
        self.uid = Some(uid);
        self.uid_source = Some(source);
        self.wizard_dirty = true;
    }

    /// Pilot-side inputs for the next tick.
    pub fn set_inputs(&mut self, inputs: PilotInputs) {
        self.inputs = inputs;
    }

    /// The pilot's current owner is (or is not) `tuning`.
    pub fn set_pilot_tuning(&mut self, tuning: bool) {
        self.pilot_tuning = tuning;
    }

    /// Result of an [`Effect::EnterTuning`].
    pub fn tuning_result(&mut self, ok: bool, now: u64) {
        if ok {
            self.tuning = Tuning::Held;
            self.pilot_tuning = true;
        } else {
            self.tuning = Tuning::None;
            self.fail_current("setup.blocker.tuning_refused", now);
        }
    }

    // ---------------------------------------------------------------- tick

    pub fn tick(&mut self, now: u64) {
        // Load identity and parameters as soon as an FC appears.
        if self.fc.heartbeat_ms.is_some() {
            if self.fc.version_ms.is_none() && now.saturating_sub(self.version_asked_ms) >= 2000 {
                self.version_asked_ms = now;
                self.out.request_message(MSG_AUTOPILOT_VERSION);
            }
            if !self.loading && !self.params.complete() && !self.fc.armed {
                self.loading = true;
                self.params.request_all(&mut self.out.msgs, now);
            }
        }
        self.adopt_identity(now);
        self.params.tick(&mut self.out.msgs, now);
        self.tick_abort_flight(now);
        self.tick_run(now);
    }

    fn tick_abort_flight(&mut self, now: u64) {
        let Some(adapter) = self.adapter() else { return };
        let Some(af) = self.abort_flight.as_mut() else { return };
        let Engine { fc, params, out, state, inputs, .. } = self;
        let mut cx = Cx {
            now,
            wizard: WizardId::Air,
            fc,
            params,
            out,
            data: &mut state.data,
            adapter,
            inputs,
            ground_done: true,
            snapshot: None,
        };
        if af.plane {
            flight::set_mode(&mut cx, Mode::Rtl);
            self.abort_flight = None;
            self.release_tuning();
            return;
        }
        if af.hop.leg == flight::Leg::Idle {
            af.hop.land(&mut cx);
        }
        af.hop.tick(&mut cx);
        if matches!(af.hop.leg, flight::Leg::Landed | flight::Leg::Failed(_)) {
            self.abort_flight = None;
            self.release_tuning();
        }
    }

    fn release_tuning(&mut self) {
        if self.tuning != Tuning::None {
            self.tuning = Tuning::None;
            self.out.effect(Effect::ExitTuning { operator: false });
        }
    }

    fn tick_run(&mut self, now: u64) {
        let Some(adapter) = self.adapter() else { return };
        let ground_done = self.state.ground.state == WizardState::Done;
        let mut abort: Option<&'static str> = None;
        let mut takeover = false;
        let mut advance = false;
        let mut enter_tuning = false;
        {
            let Some(run) = self.run.as_mut() else { return };
            let Engine { fc, params, out, state, inputs, snapshot_body, tuning, pilot_tuning, .. } = self;
            let mut cx = Cx {
                now,
                wizard: run.wizard,
                fc,
                params,
                out,
                data: &mut state.data,
                adapter,
                inputs,
                ground_done,
                snapshot: snapshot_body.as_deref(),
            };
            let Some(step) = run.steps.get_mut(run.idx) else { return };
            if step.flight() && *tuning == Tuning::None && run.wizard == WizardId::Air {
                enter_tuning = true;
            }
            step.tick(&mut cx);
            offer_skip(step.as_mut(), false);
            // Air abort monitor.
            if step.flight() && *tuning == Tuning::Held && cx.fc.armed {
                // An operator action ended `tuning`, or the sticks moved.
                let sticks = !step.blends_sticks() && cx.inputs.axes.iter().any(|a| a.abs() > TAKEOVER_AXIS);
                if !*pilot_tuning || sticks {
                    abort = Some("operator");
                    takeover = true;
                } else if cx.inputs.link_lost {
                    abort = Some("link_loss");
                } else if cx.fc.battery_pct.is_some_and(|p| p < ABORT_BATTERY_PCT) {
                    abort = Some("battery");
                } else if cx.fc.wind.is_some_and(|(_, s, _)| s > ABORT_WIND_MPS) {
                    abort = Some("wind");
                } else if let (Some((l, _)), Some(c)) = (cx.fc.local, cx.data.box_center) {
                    use ados_protocol::mavlink::ardupilotmega::MavVtolState as V;
                    let fw_flight = matches!(
                        cx.fc.vtol_state,
                        Some(V::MAV_VTOL_STATE_FW | V::MAV_VTOL_STATE_TRANSITION_TO_FW | V::MAV_VTOL_STATE_TRANSITION_TO_MC)
                    );
                    let d = ((l[0] - c[0]).powi(2) + (l[1] - c[1]).powi(2)).sqrt();
                    if d > adapter.tuning_box_m(fw_flight) {
                        abort = Some("drift");
                    }
                }
            }
            if abort.is_none() {
                let st = step.core().state;
                if st == StepState::Done || st == StepState::Skipped {
                    advance = true;
                }
            }
        }
        if enter_tuning {
            self.tuning = Tuning::Requested;
            self.out.effect(Effect::EnterTuning);
        }
        if let Some(reason) = abort {
            self.abort_air(reason, takeover, now);
            return;
        }
        self.record_current(now);
        if advance {
            self.advance(now);
        }
        // Effects some steps raise directly are already in the outbox.
    }

    /// Persists the current step's state, blockers and result: an ended
    /// wizard still shows why a step failed (an air abort's trigger).
    fn record_current(&mut self, now: u64) {
        let Some(run) = self.run.as_ref() else { return };
        let Some(step) = run.steps.get(run.idx) else { return };
        let core = step.core();
        let rec = json!({"state": core.state, "blockers": core.blockers, "result": core.result, "at": self.iso(now)});
        let wr = match run.wizard {
            WizardId::Ground => &mut self.state.ground,
            WizardId::Air => &mut self.state.air,
        };
        if wr.steps.get(step.id()) != Some(&rec) {
            let changed_state = wr.steps.get(step.id()).and_then(|r| r.get("state")) != rec.get("state");
            wr.steps.insert(step.id().to_string(), rec);
            if changed_state {
                self.persist(now);
                self.wizard_dirty = true;
            }
        }
    }

    fn persist(&mut self, now: u64) {
        let Some(uid) = self.uid.clone() else { return };
        self.state.updated_at = Some(self.iso(now));
        if let Some(a) = self.adapter() {
            self.state.firmware = Some(a.fw.as_str().into());
            self.state.vehicle_class = Some(a.class.as_str().into());
        }
        self.state.firmware_version = self.fc.version_string();
        if let Err(e) = self.store.save(&uid, &self.state) {
            tracing::warn!(error = %e, "fc_setup_save_failed");
        }
    }

    fn ensure_snapshot(&mut self, now: u64) {
        if self.state.data.snapshot.is_some() && self.snapshot_body.is_some() {
            return;
        }
        let Some(uid) = self.uid.clone() else { return };
        if !self.params.complete() {
            return;
        }
        let body = self.params.to_param_file(&format!("ADOS FC setup snapshot {}", self.iso(now)));
        match self.store.write_snapshot(&uid, &self.iso(now), &body) {
            Ok(name) => {
                self.state.data.snapshot = Some(name);
                self.snapshot_body = Some(body);
                self.persist(now);
            }
            Err(e) => tracing::warn!(error = %e, "fc_setup_snapshot_failed"),
        }
    }

    /// Begin the step at `run.idx`, skipping inapplicable ones.
    fn begin_current(&mut self, now: u64) {
        let Some(adapter) = self.adapter() else { return };
        loop {
            let needs_snapshot = self
                .run
                .as_ref()
                .and_then(|r| r.steps.get(r.idx))
                .is_some_and(|s| s.id() != "detect" && s.id() != "preflight");
            if needs_snapshot {
                self.ensure_snapshot(now);
            }
            let ground_done = self.state.ground.state == WizardState::Done;
            let Some(run) = self.run.as_mut() else { return };
            let single = run.single;
            let Engine { fc, params, out, state, inputs, snapshot_body, .. } = self;
            let mut cx = Cx {
                now,
                wizard: run.wizard,
                fc,
                params,
                out,
                data: &mut state.data,
                adapter,
                inputs,
                ground_done,
                snapshot: snapshot_body.as_deref(),
            };
            let Some(step) = run.steps.get_mut(run.idx) else { return };
            if let Some(reason) = step.skip_reason(&cx) {
                step.on_skipped(reason, &mut cx);
                step.core_mut().skip(reason);
                step.core_mut().blockers = vec![format!("setup.blocker.{reason}")];
                if single {
                    self.record_current(now);
                    self.finish_run(now);
                    return;
                }
                self.record_current(now);
                let run = self.run.as_mut().expect("run");
                run.idx += 1;
                if run.idx >= run.steps.len() {
                    self.finish_run(now);
                    return;
                }
                continue;
            }
            step.begin(&mut cx);
            offer_skip(step.as_mut(), true);
            self.wizard_dirty = true;
            self.record_current(now);
            return;
        }
    }

    fn advance(&mut self, now: u64) {
        let Some(run) = self.run.as_mut() else { return };
        if run.single || run.idx + 1 >= run.steps.len() {
            self.finish_run(now);
            return;
        }
        run.idx += 1;
        self.begin_current(now);
    }

    fn finish_run(&mut self, now: u64) {
        let Some(run) = self.run.take() else { return };
        if !run.single {
            let failed = run.steps.iter().any(|s| s.core().state == StepState::Failed);
            let wr = match run.wizard {
                WizardId::Ground => &mut self.state.ground,
                WizardId::Air => &mut self.state.air,
            };
            wr.state = if failed { WizardState::Failed } else { WizardState::Done };
            if !failed {
                wr.completed_at = Some(iso_utc(self.epoch_offset_ms + now));
            }
        }
        if run.wizard == WizardId::Air {
            // A plane ends airborne: return home, then hand back.
            if let Some(a) = self.adapter() {
                if self.fc.armed && !self.fc.on_ground() {
                    let plane = a.class == Class::Fixedwing
                        || (a.class == Class::Vtol
                            && self.fc.vtol_state == Some(ados_protocol::mavlink::ardupilotmega::MavVtolState::MAV_VTOL_STATE_FW));
                    self.abort_flight = Some(AbortFlight { hop: flight::Hop::default(), plane });
                } else {
                    self.release_tuning();
                }
            }
        }
        self.wizard_dirty = true;
        self.persist(now);
    }

    fn fail_current(&mut self, blocker: &str, now: u64) {
        if let Some(run) = self.run.as_mut() {
            if let Some(s) = run.steps.get_mut(run.idx) {
                s.core_mut().fail(blocker);
                offer_skip(s.as_mut(), false);
            }
        }
        self.record_current(now);
        self.wizard_dirty = true;
    }

    fn abort_air(&mut self, reason: &'static str, takeover: bool, now: u64) {
        let Some(adapter) = self.adapter() else { return };
        tracing::warn!(reason, takeover, "fc_setup_air_abort");
        let ground_done = self.state.ground.state == WizardState::Done;
        if let Some(run) = self.run.as_mut() {
            let Engine { fc, params, out, state, inputs, snapshot_body, .. } = self;
            let mut cx = Cx {
                now,
                wizard: run.wizard,
                fc,
                params,
                out,
                data: &mut state.data,
                adapter,
                inputs,
                ground_done,
                snapshot: snapshot_body.as_deref(),
            };
            if let Some(s) = run.steps.get_mut(run.idx) {
                s.abort(&mut cx);
                s.core_mut().fail(&format!("setup.blocker.aborted_{reason}"));
            }
        }
        self.record_current(now);
        if let Some(run) = self.run.take() {
            if !run.single {
                let wr = match run.wizard {
                    WizardId::Ground => &mut self.state.ground,
                    WizardId::Air => &mut self.state.air,
                };
                wr.state = WizardState::Aborted;
            }
        }
        if takeover {
            // The operator has the sticks: the pilot holds.
            self.tuning = Tuning::None;
            if self.pilot_tuning {
                self.out.effect(Effect::ExitTuning { operator: true });
            }
        } else if self.fc.armed && !self.fc.on_ground() {
            let plane = adapter.class == Class::Fixedwing
                || self.fc.vtol_state == Some(ados_protocol::mavlink::ardupilotmega::MavVtolState::MAV_VTOL_STATE_FW);
            self.abort_flight = Some(AbortFlight { hop: flight::Hop::default(), plane });
        } else {
            self.release_tuning();
        }
        self.wizard_dirty = true;
        self.persist(now);
    }

    // ---------------------------------------------------------------- requests

    fn wizard_blockers(&self, w: WizardId, now: u64) -> Vec<String> {
        let mut b = Vec::new();
        if !self.fc.link_alive(now) {
            b.push("setup.blocker.no_fc".into());
            return b;
        }
        match self.adapter() {
            None => b.push("setup.blocker.detecting".into()),
            Some(a) if a.fw == Fw::Other => b.push("setup.blocker.unsupported_firmware".into()),
            _ => {}
        }
        if w == WizardId::Air && self.state.ground.state != WizardState::Done {
            b.push("setup.blocker.ground_not_done".into());
        }
        if w == WizardId::Ground && self.fc.armed {
            b.push("setup.blocker.armed".into());
        }
        if self.running() {
            b.push("setup.blocker.wizard_running".into());
        }
        b
    }

    fn new_run(&self, w: WizardId) -> Option<Run> {
        let a = self.adapter()?;
        let steps = a.steps(w).iter().filter_map(|id| steps::make(id)).collect();
        Some(Run { wizard: w, steps, idx: 0, single: false })
    }

    pub fn start(&mut self, w: WizardId, now: u64) -> Value {
        let b = self.wizard_blockers(w, now);
        if let Some(first) = b.first() {
            return reply_err(first.trim_start_matches("setup.blocker."));
        }
        let Some(run) = self.new_run(w) else { return reply_err("detecting") };
        self.run = Some(run);
        if w == WizardId::Air {
            self.forget_tuning_box();
        }
        let wr = match w {
            WizardId::Ground => &mut self.state.ground,
            WizardId::Air => &mut self.state.air,
        };
        wr.state = WizardState::Running;
        wr.steps.clear();
        self.begin_current(now);
        self.persist(now);
        json!({"ok": true, "wizard": self.wizard_view(w, now)})
    }

    /// An air run that starts on the ground takes its tuning box from where
    /// it takes off this time, never a box kept from an earlier session
    /// (another field, or a GPS fix taken before it settled).
    fn forget_tuning_box(&mut self) {
        if !self.fc.armed {
            self.state.data.box_center = None;
            self.state.data.box_latlon = None;
        }
    }

    pub fn run_step(&mut self, w: WizardId, step: &str, now: u64) -> Value {
        let mut b = self.wizard_blockers(w, now);
        b.retain(|x| x != "setup.blocker.ground_not_done" || w == WizardId::Air && step != "preflight");
        if let Some(first) = b.first() {
            return reply_err(first.trim_start_matches("setup.blocker."));
        }
        let Some(a) = self.adapter() else { return reply_err("detecting") };
        if !a.steps(w).contains(&step) {
            return reply_err("unknown_step");
        }
        if w == WizardId::Air && step != "preflight" {
            // A lone air step still needs the preflight conditions.
            let pf = {
                let Engine { fc, params, out, state, inputs, .. } = self;
                let cx = Cx {
                    now,
                    wizard: w,
                    fc,
                    params,
                    out,
                    data: &mut state.data,
                    adapter: a,
                    inputs,
                    ground_done: true,
                    snapshot: None,
                };
                steps::preflight::blockers(&cx)
            };
            if let Some(first) = pf.first() {
                return reply_err(first.trim_start_matches("setup.blocker."));
            }
        }
        let Some(s) = steps::make(step) else { return reply_err("unknown_step") };
        if w == WizardId::Air {
            self.forget_tuning_box();
        }
        self.run = Some(Run { wizard: w, steps: vec![s], idx: 0, single: true });
        self.begin_current(now);
        let view = self.current_view();
        json!({"ok": true, "step": view})
    }

    pub fn action(&mut self, w: WizardId, step: &str, action: &str, args: &Map<String, Value>, now: u64) -> Value {
        let Some(adapter) = self.adapter() else { return reply_err("detecting") };
        if action == "abort" {
            return self.abort(w, now);
        }
        if action == "revert" {
            let name = args.get("snapshot").and_then(Value::as_str).map(str::to_string);
            return self.revert(name.as_deref(), now);
        }
        // Every other action but skip and retry drives the FC: with no FC
        // heard from (one that did not come back from a reboot, say) it
        // would "complete" against the cached parameters or time out later.
        if !matches!(action, "skip" | "retry") && !self.fc.link_alive(now) {
            return reply_err("no_fc");
        }
        let ground_done = self.state.ground.state == WizardState::Done;
        let tuning = self.tuning;
        let res = {
            let Some(run) = self.run.as_mut() else { return reply_err("not_running") };
            if run.wizard != w {
                return reply_err("not_running");
            }
            let Some(s) = run.steps.get_mut(run.idx) else { return reply_err("not_running") };
            if s.id() != step {
                return reply_err("not_current_step");
            }
            if action == "skip" {
                if !s.optional() || matches!(s.core().state, StepState::Running) && s.flight() {
                    return reply_err("not_skippable");
                }
                s.core_mut().skip("operator");
                Ok(())
            } else if action == "retry" && s.core().state != StepState::Failed {
                Err("not_failed")
            } else if s.flight() && tuning != Tuning::Held && (action == "start" || action == "resume_yaw") {
                Err("tuning_pending")
            } else {
                let Engine { fc, params, out, state, inputs, snapshot_body, .. } = self;
                let mut cx = Cx {
                    now,
                    wizard: w,
                    fc,
                    params,
                    out,
                    data: &mut state.data,
                    adapter,
                    inputs,
                    ground_done,
                    snapshot: snapshot_body.as_deref(),
                };
                let r = if action == "retry" {
                    s.retry(&mut cx);
                    Ok(())
                } else {
                    s.action(action, args, &mut cx)
                };
                offer_skip(s.as_mut(), action == "retry");
                r
            }
        };
        match res {
            Err(e) => reply_err(e),
            Ok(()) => {
                self.wizard_dirty = true;
                self.record_current(now);
                let view = self.current_view();
                let done = view.as_ref().is_some_and(|v| matches!(v.state, StepState::Done | StepState::Skipped));
                if done {
                    self.advance(now);
                }
                json!({"ok": true, "step": view})
            }
        }
    }

    pub fn abort(&mut self, w: WizardId, now: u64) -> Value {
        let matches = self.run.as_ref().is_some_and(|r| r.wizard == w);
        if !matches {
            return reply_err("not_running");
        }
        if w == WizardId::Air && self.fc.armed {
            self.abort_air("operator_abort", false, now);
        } else {
            let Some(adapter) = self.adapter() else { return reply_err("detecting") };
            if let Some(mut run) = self.run.take() {
                let Engine { fc, params, out, state, inputs, .. } = self;
                let mut cx = Cx {
                    now,
                    wizard: w,
                    fc,
                    params,
                    out,
                    data: &mut state.data,
                    adapter,
                    inputs,
                    ground_done: true,
                    snapshot: None,
                };
                if let Some(s) = run.steps.get_mut(run.idx) {
                    if !s.core().state.is_terminal() {
                        s.abort(&mut cx);
                        s.core_mut().fail("setup.blocker.cancelled");
                    }
                }
                self.run = Some(run);
                self.record_current(now);
                let run = self.run.take().expect("run");
                if !run.single {
                    let wr = match w {
                        WizardId::Ground => &mut self.state.ground,
                        WizardId::Air => &mut self.state.air,
                    };
                    wr.state = WizardState::Aborted;
                }
            }
            self.release_tuning();
            self.persist(now);
        }
        self.wizard_dirty = true;
        json!({"ok": true, "wizard": self.wizard_view(w, now)})
    }

    /// Restore a snapshot (default: the setup snapshot), disarmed only.
    pub fn revert(&mut self, name: Option<&str>, now: u64) -> Value {
        if self.fc.armed {
            return reply_err("armed");
        }
        if self.run.as_ref().is_some_and(|r| !r.single || r.wizard == WizardId::Air) && self.fc.armed {
            return reply_err("wizard_running");
        }
        let Some(uid) = self.uid.clone() else { return reply_err("detecting") };
        let name = name.map(str::to_string).or_else(|| self.state.data.snapshot.clone());
        let Some(name) = name else { return reply_err("no_snapshot") };
        let Some(body) = self.store.read_snapshot(&uid, &name) else { return reply_err("no_snapshot") };
        let n = self.params.restore(&body, |_| true);
        let _ = now;
        json!({"ok": true, "restored": n, "snapshot": name})
    }

    pub fn request(&mut self, op: &str, body: &Value, now: u64) -> Value {
        let wiz = || body.get("wizard").and_then(Value::as_str).and_then(WizardId::parse);
        let empty = Map::new();
        match op {
            "setup.list" => serde_json::to_value(self.view(now)).unwrap_or(Value::Null),
            "setup.start" => match wiz() {
                Some(w) => self.start(w, now),
                None => reply_err("bad_wizard"),
            },
            "setup.run_step" => match (wiz(), body.get("step").and_then(Value::as_str)) {
                (Some(w), Some(s)) => self.run_step(w, s, now),
                _ => reply_err("bad_args"),
            },
            "setup.action" => {
                let (Some(w), Some(s), Some(a)) =
                    (wiz(), body.get("step").and_then(Value::as_str), body.get("action").and_then(Value::as_str))
                else {
                    return reply_err("bad_args");
                };
                let args = body.get("args").and_then(Value::as_object).unwrap_or(&empty).clone();
                self.action(w, s, a, &args, now)
            }
            "setup.abort" => match wiz() {
                Some(w) => self.abort(w, now),
                None => reply_err("bad_wizard"),
            },
            "setup.revert" => {
                let name = body.get("snapshot").and_then(Value::as_str);
                self.revert(name, now)
            }
            _ => reply_err("unknown_op"),
        }
    }

    // ---------------------------------------------------------------- views

    /// The running step, as surfaces see it.
    pub fn current_view(&self) -> Option<StepView> {
        let run = self.run.as_ref()?;
        let s = run.steps.get(run.idx)?;
        Some(s.core().view(run.wizard, s.id()))
    }

    fn wizard_view(&self, w: WizardId, now: u64) -> WizardView {
        let wr = match w {
            WizardId::Ground => &self.state.ground,
            WizardId::Air => &self.state.air,
        };
        let ids: Vec<&str> = self.adapter().map(|a| a.steps(w).to_vec()).unwrap_or_default();
        let live = self.run.as_ref().filter(|r| r.wizard == w);
        let steps = ids
            .iter()
            .map(|id| {
                if let Some(s) = live.and_then(|r| r.steps.iter().find(|s| s.id() == *id)) {
                    if s.core().state != StepState::Pending || live.is_some_and(|r| r.steps.get(r.idx).is_some_and(|c| c.id() == *id)) {
                        return s.core().view(w, id);
                    }
                }
                let mut v = step::StepCore::default().view(w, id);
                if let Some(rec) = wr.steps.get(*id) {
                    if let Some(st) = rec.get("state").and_then(|s| serde_json::from_value(s.clone()).ok()) {
                        v.state = st;
                    }
                    v.result = rec.get("result").and_then(Value::as_object).cloned();
                    if let Some(b) = rec.get("blockers").and_then(|b| serde_json::from_value(b.clone()).ok()) {
                        v.blockers = b;
                    }
                }
                v
            })
            .collect();
        let blockers = if wr.state == WizardState::Running { Vec::new() } else { self.wizard_blockers(w, now) };
        WizardView {
            id: w,
            state: wr.state,
            available: blockers.is_empty(),
            blockers,
            completed_at: wr.completed_at.clone(),
            steps,
        }
    }

    pub fn view(&self, now: u64) -> SetupView {
        let a = self.adapter();
        let mut report_steps = Map::new();
        for wr in [&self.state.ground, &self.state.air] {
            for (k, v) in &wr.steps {
                if let Some(r) = v.get("result").filter(|r| !r.is_null()) {
                    report_steps.insert(k.clone(), r.clone());
                }
            }
        }
        SetupView {
            v: 1,
            uid: self.uid.clone(),
            uid_source: self.uid_source,
            firmware: a.map(|a| a.fw.as_str().to_string()),
            firmware_version: self.fc.version_string(),
            vehicle_class: a.map(|a| a.class.as_str().to_string()),
            running: self.run.as_ref().and_then(|r| {
                r.steps.get(r.idx).map(|s| Running { wizard: r.wizard, step: s.id().to_string() })
            }),
            wizards: vec![self.wizard_view(WizardId::Ground, now), self.wizard_view(WizardId::Air, now)],
            report: Report {
                date: self.state.ground.completed_at.clone().or(self.state.updated_at.clone()),
                firmware: a.map(|a| a.fw.as_str().to_string()),
                firmware_version: self.fc.version_string(),
                vehicle_class: a.map(|a| a.class.as_str().to_string()),
                steps: report_steps,
                snapshots: self.uid.as_deref().map(|u| self.store.snapshots(u)).unwrap_or_default(),
            },
        }
    }

    /// Stream frames due now: the running step at 5 Hz, and the wizard when
    /// its state changed.
    pub fn frames(&mut self, now: u64) -> Vec<StreamFrame> {
        let mut out = Vec::new();
        if self.wizard_dirty {
            self.wizard_dirty = false;
            let w = self.run.as_ref().map(|r| r.wizard);
            for id in [WizardId::Ground, WizardId::Air] {
                if w.is_none() || w == Some(id) {
                    out.push(StreamFrame::Wizard { ts_ms: self.epoch_offset_ms + now, data: self.wizard_view(id, now) });
                }
            }
        }
        if now.saturating_sub(self.last_stream_ms) >= STREAM_PERIOD_MS {
            if let Some(v) = self.current_view() {
                self.last_stream_ms = now;
                out.push(StreamFrame::Step { ts_ms: self.epoch_offset_ms + now, data: v });
            }
        }
        out
    }

    /// The `setup_needed` readiness message, `None` when setup is complete.
    pub fn setup_needed(&self, now: u64) -> Option<String> {
        if let Some(t) = self
            .fc
            .texts
            .iter()
            .rev()
            .filter(|(_, t, _)| now.saturating_sub(*t) <= PREARM_WINDOW_MS)
            .map(|(_, _, s)| s)
            .find(|s| statustext::needs_calibration(s))
        {
            return Some(t.clone());
        }
        self.uid.as_ref()?;
        (self.state.ground.state != WizardState::Done).then(|| "Aircraft setup not completed".to_string())
    }

    pub fn wizard_state(&self, w: WizardId) -> WizardState {
        match w {
            WizardId::Ground => self.state.ground.state,
            WizardId::Air => self.state.air.state,
        }
    }

    pub fn data(&self) -> &WizardData {
        &self.state.data
    }
}
