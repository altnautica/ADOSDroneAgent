//! The attitude rung: body-rate/thrust `SET_ATTITUDE_TARGET` out.
//!
//! The lane for a second way to fly ArduPilot (and, once G3 passes, any
//! attitude-ready flight controller). It runs the pure control laws in
//! `ados-rate-control`, turns the resulting [`AttitudeCommand`] into a
//! `SET_ATTITUDE_TARGET` (id 82), and writes it through the FC in this router.
//!
//! # Request precedence — one writer
//!
//! This is a PRECEDENCE RUNG, never a second writer racing the send path. The
//! active level is decided on each tick by a pure function and only the winner
//! is transmitted; the losing source's values are never put on the wire. The
//! human/PIC claim wins mid-command (G2's model): `resolve_authority` over the
//! PIC verdict decides, so a human holding PIC suppresses the injector, and a
//! dead or absent arbiter fails SAFE to the human hold rather than handing the
//! lane to an injector on a missing verdict.
//!
//! # Own-state freshness gate
//!
//! A stale own-attitude fix SUPPRESSES, it does not command. The swarm loop
//! gets this wrong (it keeps commanding on a frozen fix); this rung refuses:
//! once the FC's own attitude/position report is older than
//! [`OWN_ATTITUDE_STALE`], the rung emits nothing and lets the FC hold.
//!
//! # The write path
//!
//! Outbound frames go through [`FcConnection::send_bytes_bounded`] with a wall
//! budget: a timeout drops the writer and asks for a reconnect rather than
//! leaving a partial frame on the wire for the next one to corrupt. This is the
//! discipline the swarm path's plain `send_msg` (a FIFO writer mutex) does not
//! carry: bounded write, so a stalled link cannot silently accumulate.
//!
//! # Gate discipline
//!
//! Until the G3 gate (a real Betaflight FC) passes, this rung has only added a
//! second way to fly ArduPilot: the code ships inert (no live producer drives
//! it unless a rate injector claims the lane), and the G3 test is written
//! FAILING-FIRST and left `#[ignore]`d. Hardware proof is unproven. No live
//! attitude command is shipped to any airframe.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use ados_hid::pic_view::{self, resolve_authority, Authority, ChannelSourceMode, PicView};
use ados_protocol::mavlink::{serialize_v2, AttitudeSetpoint, MavHeader};
use ados_rate_control::AttitudeCommand;
use tokio::sync::{watch, Mutex, Notify};

use super::FcConnection;
use crate::state::VehicleState;

/// The rate-loop cadence: 20 Hz. Chosen as a rate comfortably above the FC's
/// setpoint-decay window (a guided/attitude command stream is a heartbeat; a
/// gap of roughly three seconds decays, so this is far inside).
pub const RATE_PERIOD: Duration = Duration::from_millis(50);

/// The wall budget on any individual FC write before it is treated as a
/// wedged link and reconnected.
pub const FC_WRITE_TIMEOUT: Duration = Duration::from_millis(50);

/// How old this vehicle's OWN attitude/position report may be before the rung
/// refuses to command. Mirrors `ados_swarmbus::OWN_STATE_STALE`: a frozen fix
/// that stays plausible is worse than an empty one, so the window is tight —
/// one second.
pub const OWN_ATTITUDE_STALE: Duration = Duration::from_secs(1);

/// How old a live rate command may be before it is no longer trusted to fly.
pub const RATE_COMMAND_STALE: Duration = Duration::from_millis(500);

/// MAVLink ids for own source identity on the wire. Component 191 is
/// `MAV_COMP_ID_ONBOARD_COMPUTER`, the convention the rest of the router uses.
const OWN_SYSTEM_ID: u8 = 1;
const OWN_COMPONENT_ID: u8 = 191;
const FC_TARGET_SYSTEM: u8 = 1;
const FC_TARGET_COMPONENT: u8 = 1;

/// The outcome of one rung tick's gate. Only [`CommandVerdict::Rate`] sends; the
/// rest suppress (and account for themselves) so the lane's time is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandVerdict {
    /// Command the live attitude/body-rate set.
    Rate,
    /// A human (non-injector) holds PIC, or the arbiter is not reporting: the
    /// pilot wins; the injector's values are never transmitted.
    SuppressHuman,
    /// The vehicle is not armed.
    SuppressDisarmed,
    /// The own-attitude fix is stale: the freshness gate suppresses.
    SuppressFreshness,
    /// No live rate command is present to fly.
    SuppressNoCommand,
}

impl CommandVerdict {
    /// The precedence wire string for the status snapshot.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Rate => "rate-command",
            Self::SuppressHuman => "human-hold",
            Self::SuppressDisarmed => "disarmed",
            Self::SuppressFreshness => "attitude-stale",
            Self::SuppressNoCommand => "no-command",
        }
    }
}

/// Decide, per tick, whether the rate lane commands. Pure: no I/O, no clock.
///
/// * `armed` — unarmed vehicles are never commanded.
/// * `fix_age` — the age of the vehicle's own attitude/position report; past
///   [`OWN_ATTITUDE_STALE`] the freshness gate suppresses (never commands).
/// * `pic` — the PIC arbiter's verdict. `None` (absent/stale/malformed) fails
///   safe to the human hold; a non-rate-injector holder is the human path and
///   wins.
/// * `verified_rate_injector` — the ATTESTED identity of the live rate
///   injector (a credential, never a caller-supplied label).
/// * `rate_live` — whether a fresh rate command is actually present.
pub fn command_verdict(
    armed: bool,
    fix_age: Option<Duration>,
    pic: Option<&PicView>,
    verified_rate_injector: Option<&str>,
    rate_live: bool,
) -> CommandVerdict {
    if !armed {
        return CommandVerdict::SuppressDisarmed;
    }
    // Own-state freshness gate: a stale attitude fix SUPPRESSES, it does not
    // command. This is the mistake the swarm loop makes and the one not to
    // repeat here.
    match fix_age {
        Some(age) if age <= OWN_ATTITUDE_STALE => {}
        _ => return CommandVerdict::SuppressFreshness,
    }
    if !rate_live {
        return CommandVerdict::SuppressNoCommand;
    }
    // Request precedence: the PIC arbiter's holder decides. A dead/hung
    // arbiter (pic == None) fails SAFE to the human hold; only a fresh
    // unclaimed report or a claim held BY the rate injector lets the lane fly.
    match resolve_authority(ChannelSourceMode::Hybrid, pic, verified_rate_injector) {
        Authority::Inject => CommandVerdict::Rate,
        Authority::Hid => CommandVerdict::SuppressHuman,
    }
}

/// The atomics status block the rung publishes for the state snapshot.
#[derive(Debug)]
pub struct AttitudeSetpointStatus {
    setpoints_emitted: AtomicU64,
    ticks_suppressed: AtomicU64,
    freshness_suppressions: AtomicU64,
    /// The verdict of the most recent tick, as a [`CommandVerdict`] discriminant
    /// (or [`UNKNOWN_VERDICT`] before the first tick).
    verdict: AtomicU8,
}

/// The pre-tick / unknown verdict discriminant, so a not-yet-ticked status reads
/// honest rather than as whichever variant happens to be discriminant 0.
const UNKNOWN_VERDICT: u8 = u8::MAX;

impl Default for AttitudeSetpointStatus {
    fn default() -> Self {
        Self {
            setpoints_emitted: AtomicU64::new(0),
            ticks_suppressed: AtomicU64::new(0),
            freshness_suppressions: AtomicU64::new(0),
            verdict: AtomicU8::new(UNKNOWN_VERDICT),
        }
    }
}

impl AttitudeSetpointStatus {
    pub fn setpoints_emitted(&self) -> u64 {
        self.setpoints_emitted.load(Ordering::Relaxed)
    }
    pub fn ticks_suppressed(&self) -> u64 {
        self.ticks_suppressed.load(Ordering::Relaxed)
    }
    pub fn freshness_suppressions(&self) -> u64 {
        self.freshness_suppressions.load(Ordering::Relaxed)
    }
    /// The wire string of the most recent verdict.
    pub fn verdict_wire(&self) -> &'static str {
        let v = self.verdict.load(Ordering::Relaxed);
        match CommandVerdict::from_u8(v) {
            Some(x) => x.as_wire(),
            None => "unknown",
        }
    }
    pub fn publish(
        &self,
        verdict: CommandVerdict,
        counters: ados_rate_control::AttitudeControlCounters,
    ) {
        self.verdict.store(verdict as u8, Ordering::Relaxed);
        self.setpoints_emitted
            .store(counters.setpoints_emitted, Ordering::Relaxed);
        self.ticks_suppressed
            .store(counters.ticks_suppressed, Ordering::Relaxed);
        self.freshness_suppressions
            .store(counters.freshness_suppressions, Ordering::Relaxed);
    }
}

impl CommandVerdict {
    fn from_u8(v: u8) -> Option<CommandVerdict> {
        match v {
            0 => Some(CommandVerdict::Rate),
            1 => Some(CommandVerdict::SuppressHuman),
            2 => Some(CommandVerdict::SuppressDisarmed),
            3 => Some(CommandVerdict::SuppressFreshness),
            4 => Some(CommandVerdict::SuppressNoCommand),
            _ => None,
        }
    }
}

/// Run the attitude rung until cancelled.
///
/// Returns immediately when `enabled` is false: an operator who has not turned
/// the lane on pays for no timer and no task.
pub async fn run(
    fc: Arc<FcConnection>,
    state: Arc<Mutex<VehicleState>>,
    enabled: bool,
    pic_path: String,
    rate_rx: watch::Receiver<Option<(AttitudeCommand, String, Instant)>>,
    status: Arc<AttitudeSetpointStatus>,
    cancel: Arc<Notify>,
) {
    if !enabled {
        tracing::debug!("attitude_setpoint_disabled");
        return;
    }
    tracing::info!("attitude_setpoint_started");
    control_loop(fc, state, pic_path, rate_rx, status, cancel).await;
}

/// Tick the gate and, when the rate lane wins, send the setpoint with a bounded
/// write. The gate and the send share the tick so a precedence change takes
/// effect on the same cadence — never between a fresh decision and a stale send.
async fn control_loop(
    fc: Arc<FcConnection>,
    state: Arc<Mutex<VehicleState>>,
    pic_path: String,
    mut rate_rx: watch::Receiver<Option<(AttitudeCommand, String, Instant)>>,
    status: Arc<AttitudeSetpointStatus>,
    cancel: Arc<Notify>,
) {
    let mut counters = ados_rate_control::AttitudeControlCounters::default();
    let mut tick = tokio::time::interval(RATE_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = cancel.notified() => return,
            _ = tick.tick() => {}
        }
        let now = Instant::now();

        let (armed, fix_age) = {
            let s = state.lock().await;
            (
                s.armed,
                s.position_at.map(|t| now.saturating_duration_since(t)),
            )
        };

        // The PIC verdict, staleness-gated against its sidecar mtime. `None`
        // (absent/stale/malformed/backward-clock) reads as "not reporting" and
        // fails safe to the human hold.
        let pic = pic_view::read_pic_view(std::path::Path::new(&pic_path), SystemTime::now());

        // The newest live rate command, its ATTESTED injector identity, and the
        // gate verdict. The watch guard is scoped here and dropped before any
        // `.await`, so the (non-Send) lock guard never spans a suspend point.
        let (cmd, verdict) = {
            let borrowed = rate_rx.borrow_and_update();
            let (cmd, verified, cmd_at) = match borrowed.as_ref() {
                Some((c, id, at)) => (*c, Some(id.as_str()), *at),
                None => (AttitudeCommand::neutral(), None, now),
            };
            let rate_live = now.saturating_duration_since(cmd_at) <= RATE_COMMAND_STALE;
            let verdict = command_verdict(armed, fix_age, pic.as_ref(), verified, rate_live);
            (cmd, verdict)
        };

        match verdict {
            CommandVerdict::Rate => {
                counters.setpoints_emitted += 1;
                send_attitude(&fc, &cmd).await;
            }
            CommandVerdict::SuppressFreshness => {
                counters.freshness_suppressions += 1;
                counters.ticks_suppressed += 1;
            }
            _ => {
                counters.ticks_suppressed += 1;
            }
        }
        status.publish(verdict, counters);
    }
}

/// Pack an [`AttitudeCommand`] into a `SET_ATTITUDE_TARGET` and write it to the
/// FC with a bounded write.
///
/// The command is a body-rate/thrust command: the quaternion is ignored
/// (`ATTITUDE_IGNORE`), the three body rates and thrust are active. Building
/// through [`AttitudeSetpoint`] validates finiteness and thrust range on this
/// side of the wire; serializing through `ados_protocol` gives the id 82 frame
/// with `ATTITUDE_TARGET_CRC_EXTRA` (49); writing through
/// [`FcConnection::send_bytes_bounded`] bounds the write so a wedged link is
/// reconnected rather than left mid-frame.
async fn send_attitude(fc: &Arc<FcConnection>, cmd: &AttitudeCommand) {
    // Body-rate command: the three body rates and thrust are active; the
    // quaternion is ignored (ATTITUDE_IGNORE = 128).
    let sp = AttitudeSetpoint {
        type_mask: 128, // ATTITUDE_TARGET_TYPEMASK_ATTITUDE_IGNORE
        q: [1.0, 0.0, 0.0, 0.0],
        body_roll_rate: cmd.body_roll_rate,
        body_pitch_rate: cmd.body_pitch_rate,
        body_yaw_rate: cmd.body_yaw_rate,
        thrust: cmd.thrust,
    };
    let msg = match sp.build_message(FC_TARGET_SYSTEM, FC_TARGET_COMPONENT) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "attitude_setpoint_rejected");
            return;
        }
    };
    let header = MavHeader {
        system_id: OWN_SYSTEM_ID,
        component_id: OWN_COMPONENT_ID,
        sequence: 0,
    };
    let bytes = match serialize_v2(header, &msg) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "attitude_serialize_failed");
            return;
        }
    };
    if !fc.send_bytes_bounded(&bytes, FC_WRITE_TIMEOUT).await {
        tracing::warn!("attitude_write_timed_out");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_fix() -> Option<Duration> {
        Some(Duration::from_millis(10))
    }

    fn no_pic() -> Option<&'static PicView> {
        None
    }

    #[test]
    fn freshness_gate_suppresses_a_stale_fix() {
        // A stale own-attitude fix SUPPRESSES, even with a live rate command
        // and no PIC claim — this is the gate that never commands on a frozen fix.
        let stale = Some(OWN_ATTITUDE_STALE + Duration::from_secs(2));
        assert_eq!(
            command_verdict(true, stale, None, Some("rate-1"), true),
            CommandVerdict::SuppressFreshness
        );
        // An absent fix is equally stale.
        assert_eq!(
            command_verdict(true, None, None, Some("rate-1"), true),
            CommandVerdict::SuppressFreshness
        );
    }

    #[test]
    fn a_disarmed_vehicle_is_never_commanded() {
        assert_eq!(
            command_verdict(false, fresh_fix(), None, Some("rate-1"), true),
            CommandVerdict::SuppressDisarmed
        );
    }

    #[test]
    fn no_live_command_suppresses() {
        assert_eq!(
            command_verdict(true, fresh_fix(), None, Some("rate-1"), false),
            CommandVerdict::SuppressNoCommand
        );
    }

    #[test]
    fn a_fresh_unclaimed_pic_lets_the_rate_lane_fly() {
        let unclaimed = PicView::default();
        assert_eq!(
            command_verdict(true, fresh_fix(), Some(&unclaimed), Some("rate-1"), true),
            CommandVerdict::Rate
        );
    }

    #[test]
    fn the_rate_injector_holding_pic_keeps_the_lane() {
        let claimed = PicView {
            claimed: true,
            holder: Some("rate-1".into()),
        };
        assert_eq!(
            command_verdict(true, fresh_fix(), Some(&claimed), Some("rate-1"), true),
            CommandVerdict::Rate
        );
    }

    #[test]
    fn a_human_holder_wins_mid_command() {
        let human = PicView {
            claimed: true,
            holder: Some("hdmi-kiosk".into()),
        };
        assert_eq!(
            command_verdict(true, fresh_fix(), Some(&human), Some("rate-1"), true),
            CommandVerdict::SuppressHuman
        );
    }

    #[test]
    fn a_dead_arbiter_fails_safe_to_the_human_hold() {
        // No PIC report at all: hybrid fails SAFE. The rate lane must not fly
        // on a missing verdict, even with a live command and a claimed identity.
        assert_eq!(
            command_verdict(true, fresh_fix(), no_pic(), Some("rate-1"), true),
            CommandVerdict::SuppressHuman
        );
    }

    #[tokio::test]
    async fn a_rate_verdict_sends_an_id_82_frame() {
        // Round-trip the exact frame the rung would write: id 82, a body-rate
        // command, through the shared builder. This proves the wire bytes are
        // a valid SET_ATTITUDE_TARGET whether or not a rig is attached.
        let cmd = AttitudeCommand {
            body_roll_rate: 1.0,
            body_pitch_rate: -0.5,
            body_yaw_rate: 0.2,
            thrust: 0.6,
        };
        let sp = AttitudeSetpoint {
            type_mask: 128, // ATTITUDE_IGNORE: body-rate command
            q: [1.0, 0.0, 0.0, 0.0],
            body_roll_rate: cmd.body_roll_rate,
            body_pitch_rate: cmd.body_pitch_rate,
            body_yaw_rate: cmd.body_yaw_rate,
            thrust: cmd.thrust,
        };
        sp.validate().expect("a body-rate command validates");
        let msg = sp.build_message(1, 1).expect("builds");
        let header = MavHeader {
            system_id: 1,
            component_id: 191,
            sequence: 0,
        };
        let bytes = serialize_v2(header, &msg).expect("serializes");
        assert_eq!(bytes[7], 82, "message id low byte is 82");
        assert_eq!(bytes[8], 0);
        assert_eq!(bytes[9], 0);
        let (_h, decoded) = ados_protocol::mavlink::parse_v2(&bytes).expect("parses");
        match decoded {
            ados_protocol::mavlink::MavMessage::SET_ATTITUDE_TARGET(d) => {
                assert_eq!(d.body_roll_rate, 1.0);
                assert_eq!(d.body_pitch_rate, -0.5);
                assert_eq!(d.body_yaw_rate, 0.2);
                assert_eq!(d.thrust, 0.6);
            }
            other => panic!("expected SET_ATTITUDE_TARGET, got {other:?}"),
        }
    }

    #[test]
    fn the_status_block_round_trips_verdicts() {
        let s = AttitudeSetpointStatus::default();
        assert_eq!(s.verdict_wire(), "unknown");
        for v in [
            CommandVerdict::Rate,
            CommandVerdict::SuppressHuman,
            CommandVerdict::SuppressDisarmed,
            CommandVerdict::SuppressFreshness,
            CommandVerdict::SuppressNoCommand,
        ] {
            s.publish(v, ados_rate_control::AttitudeControlCounters::default());
            assert_eq!(s.verdict_wire(), v.as_wire(), "{v:?}");
        }
        s.publish(
            CommandVerdict::SuppressFreshness,
            ados_rate_control::AttitudeControlCounters {
                setpoints_emitted: 1,
                ticks_suppressed: 4,
                freshness_suppressions: 2,
            },
        );
        assert_eq!(s.setpoints_emitted(), 1);
        assert_eq!(s.ticks_suppressed(), 4);
        assert_eq!(s.freshness_suppressions(), 2);
    }
}
