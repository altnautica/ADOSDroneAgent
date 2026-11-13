//! Take control answered from the flight controller. The request puts the FC
//! in GUIDED; the operator owns the vehicle, and the action is answered ok,
//! only once the FC confirms the mode: an accepting `COMMAND_ACK`, or a
//! HEARTBEAT in GUIDED when the ack is lost. A refusing ack, or no GUIDED
//! within [`TAKE_ANSWER_MS`], answers a refusal carrying the FC's own reason,
//! and the owner stays where it was.

use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavResult, COMMAND_ACK_DATA};
use ados_protocol::pilot::{ActionResult, Owner};

use super::{Out, Pilot};

/// The FC must have answered within this long; a surface gives up on an
/// action after 3 s.
pub const TAKE_ANSWER_MS: u64 = 2500;

/// Operator actions that replace a Take control still waiting on the FC.
const SUPERSEDES: &[&str] = &[
    "land",
    "rth",
    "pause",
    "kill",
    "resume",
    "hand_to_fc",
    "goto",
    "vtol",
    "control_mode",
];

/// A GUIDED request for Take control, not yet answered by the FC.
#[derive(Debug, Clone)]
struct Pending {
    /// The deferred reply: its token and result, once `on_request` deferred it.
    reply: Option<(u64, ActionResult)>,
    sent_ms: u64,
    /// The owner when the operator asked. Another owner by the answer means
    /// someone else took the vehicle meanwhile.
    owner: Owner,
    /// The FC refused the request in its COMMAND_ACK.
    refused: bool,
}

#[derive(Debug, Default)]
pub(super) struct Take {
    pending: Option<Pending>,
    next_token: u64,
}

impl Take {
    pub(super) fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn start(&mut self, sent_ms: u64, owner: Owner) {
        self.pending = Some(Pending {
            reply: None,
            sent_ms,
            owner,
            refused: false,
        });
    }

    /// Hold back `result` when it is the ok of a Take control that now
    /// waits on the FC: `Ok(token)` to answer later, else `Err(result)`.
    pub(super) fn defer(&mut self, result: ActionResult) -> Result<u64, ActionResult> {
        match self.pending.as_mut() {
            Some(p) if p.reply.is_none() && result.ok => {
                self.next_token += 1;
                p.reply = Some((self.next_token, result));
                Ok(self.next_token)
            }
            _ => Err(result),
        }
    }
}

impl Pilot {
    /// An operator action other than Take control: one that flies or
    /// changes mode answers a waiting Take control `busy`.
    pub(super) fn take_superseded(&mut self, name: &str) {
        let kind = name.split(':').next().unwrap_or(name);
        if self.take.pending() && SUPERSEDES.contains(&kind) {
            self.answer_take(Err("busy".into()));
        }
    }

    /// The FC's COMMAND_ACK. Only a DO_SET_MODE answer nobody else could
    /// have asked for counts: another mode command of the pilot's, or a
    /// ground station's seen by the router, makes it ambiguous.
    pub(super) fn take_ack(&mut self, ack: &COMMAND_ACK_DATA, now_ms: u64) {
        let Some(p) = self.take.pending.as_ref() else {
            return;
        };
        let guided = self.family().guided();
        let ours = self.own_mode == Some((guided, p.sent_ms));
        let client = self
            .client_mode
            .is_some_and(|(_, t)| t.saturating_add(super::CLIENT_MODE_WINDOW_MS) > p.sent_ms);
        if ack.command != MavCmd::MAV_CMD_DO_SET_MODE || !ours || client {
            return;
        }
        match ack.result {
            MavResult::MAV_RESULT_ACCEPTED => self.check_take(now_ms, true),
            MavResult::MAV_RESULT_IN_PROGRESS => {}
            _ => {
                if let Some(p) = self.take.pending.as_mut() {
                    p.refused = true;
                }
            }
        }
    }

    /// Answer a waiting Take control from what the FC shows at `now_ms`:
    /// called on each FC HEARTBEAT, on an accepting ack (`accepted`) and on
    /// the tick (for the deadline).
    pub(super) fn check_take(&mut self, now_ms: u64, accepted: bool) {
        let Some(p) = self.take.pending.as_ref() else {
            return;
        };
        let heartbeat = self.fc.heartbeat_ms == now_ms;
        let late = now_ms.saturating_sub(p.sent_ms) >= TAKE_ANSWER_MS;
        let verdict = if self.owner() != p.owner {
            let why = if self.fc_failsafe { "failsafe" } else { "busy" };
            Err(why.to_owned())
        } else if accepted || (heartbeat && self.in_guided()) {
            Ok(())
        } else if (heartbeat && p.refused) || late {
            // The FC's STATUSTEXT may trail its ack: it is read at the next
            // HEARTBEAT, or at the deadline.
            let reason = self.fc_mode_text(p.sent_ms);
            Err(match reason {
                Some(t) => format!("fc_refused:{t}"),
                None if p.refused => "fc_refused".to_owned(),
                None => "fc_no_answer".to_owned(),
            })
        } else {
            return;
        };
        if verdict.is_ok() {
            self.took_control(now_ms);
        }
        self.answer_take(verdict);
    }

    /// The disarm ends a waiting Take control.
    pub(super) fn take_disarmed(&mut self) {
        if self.take.pending() {
            self.answer_take(Err("disarmed".into()));
        }
    }

    /// The FC's newest warning about a mode change since `since_ms`.
    fn fc_mode_text(&self, since_ms: u64) -> Option<String> {
        self.fc
            .statustext
            .iter()
            .rev()
            .take_while(|s| s.at_ms >= since_ms)
            .find(|s| s.v.1.to_ascii_lowercase().contains("mode"))
            .map(|s| s.v.1.clone())
    }

    /// A refusal also gives up the pilot's claim on its GUIDED request: should
    /// the FC still enter GUIDED, the pilot handles it like any GUIDED it did
    /// not ask for (adopted under `ados`, else the FC's), instead of counting
    /// it as a request nobody now stands behind.
    fn answer_take(&mut self, verdict: Result<(), String>) {
        let Some(p) = self.take.pending.take() else {
            return;
        };
        if verdict.is_err() && self.own_mode == Some((self.family().guided(), p.sent_ms)) {
            self.own_mode = None;
        }
        let Some((token, mut result)) = p.reply else {
            return;
        };
        result.ok = verdict.is_ok();
        result.err = verdict.err();
        self.out.push(Out::Answer(token, result));
    }
}
