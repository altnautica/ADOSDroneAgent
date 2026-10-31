//! Pilot-in-command arbiter FSM.
//!
//! Manages which client currently holds PIC (pilot-in-command) authority for
//! the ground station. Exactly one client at a time may send flight-critical
//! inputs to the flight controller; others see a read-only view until they
//! request and are granted a transfer.
//!
//! State machine:
//!
//! ```text
//! unclaimed  --claim(cid)-->             claimed(cid)
//! claimed(A) --claim(B, confirm_token)-> claimed(B)
//! claimed(A) --claim(B, force=true)----> claimed(B)
//! claimed(A) --release(A)--------------> unclaimed
//! claimed(A) --on_pic_disconnected(A)-> unclaimed
//! ```
//!
//! Covers the five claim cases with their exact outcome shapes (an HTTP layer can map `needs_confirm`
//! / `status` to 409 / 403 / 410), the confirm-token TTL, the heartbeat
//! session watchdog auto-release, gamepad auto-claim, and the monotonic claim
//! counter. Time comes from an injectable [`Clock`] so the TTL and watchdog are
//! unit-testable without sleeping.

use std::collections::HashMap;

use crate::eventbus::{PicEvent, PicEventBus, PicEventKind};

/// Confirm-token lifetime. A short window forces the taking client to act
/// intentionally and prevents replay long after the original UI warning.
pub const CONFIRM_TTL_SECONDS: f64 = 2.0;

/// Clients holding PIC must heartbeat at least this often or the watchdog
/// auto-releases their claim, preventing stale PIC state after a tab closes
/// without calling release.
pub const HEARTBEAT_TIMEOUT_SECONDS: f64 = 30.0;

/// Cadence at which the session watchdog checks the last heartbeat age. Short
/// enough that a state mirror refreshed on each tick never looks stale while
/// the arbiter is idle but healthy.
pub const WATCHDOG_INTERVAL_SECONDS: f64 = 2.0;

/// A monotonic, injectable time source.
///
/// `monotonic()` is fractional seconds from an arbitrary epoch (used for TTL
/// and heartbeat-age math; only differences are meaningful). `wall_ms()` is
/// unix milliseconds stamped onto emitted events. Production wiring uses
/// [`SystemClock`]; tests use a manually advanced clock so token expiry and the
/// watchdog can be exercised without real time passing.
pub trait Clock: Send + Sync {
    /// Monotonic seconds.
    fn monotonic(&self) -> f64;
    /// Unix wall-clock milliseconds.
    fn wall_ms(&self) -> i64;
}

/// Real wall + monotonic clock.
#[derive(Debug, Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn monotonic(&self) -> f64 {
        use std::time::Instant;
        // A process-lifetime base so successive reads are monotonic and only
        // their difference matters.
        use std::sync::OnceLock;
        static BASE: OnceLock<Instant> = OnceLock::new();
        let base = *BASE.get_or_init(Instant::now);
        base.elapsed().as_secs_f64()
    }

    fn wall_ms(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// The current PIC state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PicState {
    /// Nobody holds PIC.
    Unclaimed,
    /// A client holds PIC.
    Claimed,
}

impl PicState {
    /// The lowercase wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            PicState::Unclaimed => "unclaimed",
            PicState::Claimed => "claimed",
        }
    }
}

/// The outcome of a [`PicArbiter::claim`] call. Variants carry exactly the
/// fields an HTTP layer serializes; `status` is the HTTP code the rejection
/// variants map to.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    /// Fresh claim from `unclaimed`.
    Fresh {
        /// The client now holding PIC.
        claimed_by: String,
        /// The monotonic claim counter after this call.
        claim_counter: u64,
    },
    /// Same client re-claimed; nothing changed.
    Idempotent {
        /// The client now holding PIC.
        claimed_by: String,
        /// The monotonic claim counter after this call.
        claim_counter: u64,
    },
    /// Force takeover (logged at WARN); always wins.
    Forced {
        /// The client now holding PIC.
        claimed_by: String,
        /// The monotonic claim counter after this call.
        claim_counter: u64,
        /// The client that held PIC before, if any.
        previous_pic: Option<String>,
    },
    /// Confirm-token matched and was unexpired; transfer completed.
    Transferred {
        /// The client now holding PIC.
        claimed_by: String,
        /// The monotonic claim counter after this call.
        claim_counter: u64,
        /// The client control was transferred from.
        transferred_from: Option<String>,
    },
    /// A confirm token was supplied but missing or expired. -> HTTP 409.
    InvalidConfirmToken {
        /// The client currently holding PIC, if any.
        current_pic: Option<String>,
        /// The HTTP status this rejection maps to.
        status: u16,
    },
    /// Already claimed, no token, no force. -> HTTP 409.
    AlreadyClaimed {
        /// The client currently holding PIC, if any.
        current_pic: Option<String>,
        /// The HTTP status this rejection maps to.
        status: u16,
    },
}

impl ClaimOutcome {
    /// Whether the claim succeeded (the client now holds PIC).
    pub fn claimed(&self) -> bool {
        matches!(
            self,
            ClaimOutcome::Fresh { .. }
                | ClaimOutcome::Idempotent { .. }
                | ClaimOutcome::Forced { .. }
                | ClaimOutcome::Transferred { .. }
        )
    }

    /// Whether the GCS should surface the "Take control" confirm prompt
    pub fn needs_confirm(&self) -> bool {
        matches!(
            self,
            ClaimOutcome::InvalidConfirmToken { .. } | ClaimOutcome::AlreadyClaimed { .. }
        )
    }

    /// The HTTP status an HTTP layer returns (200 on success, 409 on the
    /// confirm-needed rejections).
    pub fn http_status(&self) -> u16 {
        match self {
            ClaimOutcome::InvalidConfirmToken { status, .. }
            | ClaimOutcome::AlreadyClaimed { status, .. } => *status,
            _ => 200,
        }
    }
}

/// Outcome of [`PicArbiter::release`].
#[derive(Debug, Clone, PartialEq)]
pub enum ReleaseOutcome {
    /// The caller released PIC.
    Released {
        /// The client that held PIC before, if any.
        previous_pic: Option<String>,
    },
    /// Caller does not currently hold PIC. -> HTTP 403.
    NotCurrentPic {
        /// The client currently holding PIC, if any.
        current_pic: Option<String>,
        /// The HTTP status this rejection maps to.
        status: u16,
    },
}

/// Outcome of [`PicArbiter::heartbeat`].
#[derive(Debug, Clone, PartialEq)]
pub enum HeartbeatOutcome {
    /// The heartbeat was accepted.
    Ok {
        /// The client now holding PIC.
        claimed_by: Option<String>,
        /// The monotonic claim counter after this call.
        claim_counter: u64,
        /// Monotonic seconds of the accepted heartbeat.
        last_heartbeat_ts: f64,
    },
    /// No active claim from this client. -> HTTP 410.
    NoActiveClaim {
        /// The client currently holding PIC, if any.
        current_pic: Option<String>,
        /// The HTTP status this rejection maps to.
        status: u16,
    },
}

/// Outcome of [`PicArbiter::on_pic_disconnected`].
#[derive(Debug, Clone, PartialEq)]
pub enum DisconnectOutcome {
    /// The named client held PIC and it was cleared, or PIC was already free.
    Disconnected,
    /// Another client holds PIC; nothing changed. -> HTTP 409.
    NotHolder {
        /// The client currently holding PIC, if any.
        current_pic: Option<String>,
        /// The HTTP status this rejection maps to.
        status: u16,
    },
}

/// A point-in-time snapshot of the arbiter state.
#[derive(Debug, Clone, PartialEq)]
pub struct PicStateSnapshot {
    /// Claimed or unclaimed.
    pub state: PicState,
    /// The client now holding PIC.
    pub claimed_by: Option<String>,
    /// Monotonic seconds when the current claim started.
    pub claimed_since: Option<f64>,
    /// The monotonic claim counter after this call.
    pub claim_counter: u64,
    /// The gamepad bound to the current PIC, if any.
    pub primary_gamepad_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ConfirmToken {
    token: String,
    expires_at: f64,
}

/// The PIC arbiter. Single-threaded by contract: the owner serializes
/// calls (hold the arbiter behind a `Mutex`). Time is injected via
/// [`Clock`].
pub struct PicArbiter {
    state: PicState,
    claimed_by: Option<String>,
    claimed_since: Option<f64>,
    claim_counter: u64,
    primary_gamepad_id: Option<String>,
    /// The client id the primary gamepad's auto-claim was issued for. A claim
    /// by any other holder unbinds the gamepad.
    primary_gamepad_hint: Option<String>,
    last_heartbeat_ts: Option<f64>,
    confirm_tokens: HashMap<String, ConfirmToken>,
    bus: PicEventBus,
    clock: Box<dyn Clock>,
}

mod arbiter;

#[cfg(test)]
mod tests;
