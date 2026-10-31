//! The bind FSM driver. Ports `BindOrchestrator` from `bind_orchestrator.py`.
//!
//! `start_local_bind` runs the session future alongside a caller cancel and the
//! global wedge watchdog under one `tokio::select!`. When cancel or the
//! watchdog wins, dropping the session future is the structural cleanup:
//! `SocatProcess::drop` `killpg`s the tunnel and the peer-poll [`AbortOnDrop`]
//! guard aborts the poller — then [`cleanup`](BindOrchestrator::cleanup) stops
//! the bind unit and restarts the normal one. Every transition writes the
//! cross-process sentinel ([`super::BIND_STATE_SENTINEL`]) so the radio service
//! and hop supervisor can answer `is_bind_active()` without an in-process
//! singleton.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::process_manager::ProcessManager;
use ados_protocol::logd::emitter::EventEmitter;
use tokio::sync::Mutex;

use super::bind_event::BindFailReason;
use super::fsm::BindSession;

mod error;
mod peer;
mod prep;
mod session;
mod stages;
mod state;

pub use state::read_sentinel_active;

/// A recoverable bind failure. `phase` names the [`BindState`](super::fsm::BindState)
/// the failure surfaced in so the GCS/LCD renders the right badge. `reason`, when
/// set, carries the precise failure cause the emitter should report instead of
/// re-deriving it from the free-text `message` — the peer-evidence gate uses
/// this so `no_peer_proof` / `stale_key` never fall through the string-matching
/// classifier (name the true cause, don't guess it).
#[derive(Debug, Clone)]
pub struct BindError {
    pub message: String,
    pub phase: Option<String>,
    pub reason: Option<BindFailReason>,
}

/// Returned by [`BindOrchestrator::start_local_bind`] when a session is already
/// in flight (the FastAPI seam maps this to a 409).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindStartError {
    Busy,
}

/// Aborts the wrapped task when dropped — guarantees the peer-presence poller
/// dies even if the session future is dropped mid-flight (cancel / watchdog).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

/// RAII clear of the orchestrator's session-in-progress flag. Set to `true` the
/// moment `start_local_bind` commits to a session and dropped (back to `false`)
/// when the function returns — including every early-return and any unwind — so
/// the flag tracks the WHOLE `start_local_bind` body, not just the data-plane-
/// active sub-states. This is what the supervisor gate reads: a bind owns the
/// radio from the instant it stops the normal unit (in `Idle`, before the first
/// `OpeningTunnel` transition) until the function unwinds its cleanup.
struct InProgressGuard(Arc<AtomicBool>);

/// Single-instance bind state machine.
pub struct BindOrchestrator {
    /// Single-flight gate. `try_lock` failing means a session is in progress.
    lock: Mutex<()>,
    /// The live session (None when idle). Shared with the peer-poll task.
    session: Arc<Mutex<Option<BindSession>>>,
    /// Out-of-band abort: the control socket's `cancel_bind` op (which arrives
    /// on a different connection than the blocked `start_bind`) notifies this so
    /// the in-flight session aborts. `start_local_bind` races it in its select.
    cancel: Arc<tokio::sync::Notify>,
    /// Whole-session-in-progress flag. `true` for the entire body of an in-flight
    /// `start_local_bind` — set the instant it commits to a session (while still
    /// in `Idle`, BEFORE the normal unit is stopped and the radio prepared) and
    /// cleared when the function returns. Distinct from [`is_active`], which is
    /// driven by the session's data-plane sub-state and is `false` during the
    /// `Idle` stop→`OpeningTunnel` setup window. The supervisor's radio-restart /
    /// usb-rehome gate reads THIS so it never re-claims the adapter mid-setup.
    session_in_progress: Arc<AtomicBool>,
    /// Ships the bind-session lifecycle events to the logging daemon. Best-effort
    /// and non-blocking; an absent daemon socket is dropped quietly. The log
    /// lines remain the always-on fallback.
    events: EventEmitter,
    /// The host service-manager backend the bind sequence drives when it stops
    /// the normal radio unit, starts the bind unit, and restarts the units on
    /// cleanup. Selected for the host OS.
    pm: Arc<dyn ProcessManager>,
}

/// The peer-evidence verdict for a completed wire exchange. Pure so the
/// invariant it encodes is unit-testable without a radio pair on a bench.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerEvidence {
    /// Both proofs held: a real peer decoded traffic this session (and, drone
    /// side, deposited a fresh key). The ONLY verdict that may reach Paired.
    Verified,
    /// No decoded peer traffic crossed the bind tunnel this session — the wire
    /// protocol ran against a stale/leaked local endpoint, not a real peer.
    NoFrames,
    /// Drone side: frames were seen but the upstream key was not refreshed by
    /// this session's transfer (a leftover from an earlier bind).
    StaleKey,
}

/// The per-iface result of preparing one injection adapter for monitor mode.
/// (Constructed only on the linux prep path + in tests; the host lib build sees
/// it solely through `summarize_precheck`'s slice param.)
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone)]
struct InjectionPrepOutcome {
    iface: String,
    /// The readback operating mode after prep (`monitor` | `managed` | `unknown`).
    injection_mode: String,
    /// Whether the readback confirmed `type monitor` within the retry budget.
    monitor_verified: bool,
    /// Whether NetworkManager enumerates the iface (the legacy precheck condition).
    nm_enumerable: bool,
}

#[cfg(test)]
mod tests;
