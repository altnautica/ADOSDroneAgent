//! Bind state machine + session record.
//!
//! The wall-clock fields (`started_at`/`finished_at`) are ISO-8601 second-precision
//! UTC; the phase clocks (`phase_entered_at`, `last_frame_at`) are monotonic
//! seconds-since-process-start floats, exactly as Python stores `time.monotonic()`.

use std::sync::OnceLock;
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use super::BindRole;

/// The nine bind states. `#[serde(rename_all = "snake_case")]` renders the same
/// wire strings as the Python StrEnum values (`"opening_tunnel"`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindState {
    Idle,
    OpeningTunnel,
    WaitingPeer,
    TransferringKeys,
    ApplyingKeys,
    RestartingServices,
    Paired,
    Failed,
    Aborted,
}

impl BindState {
    /// The wire string (matches the Python StrEnum value).
    pub fn as_str(&self) -> &'static str {
        match self {
            BindState::Idle => "idle",
            BindState::OpeningTunnel => "opening_tunnel",
            BindState::WaitingPeer => "waiting_peer",
            BindState::TransferringKeys => "transferring_keys",
            BindState::ApplyingKeys => "applying_keys",
            BindState::RestartingServices => "restarting_services",
            BindState::Paired => "paired",
            BindState::Failed => "failed",
            BindState::Aborted => "aborted",
        }
    }

    /// Terminal states — the radio is back under normal-unit control (or never
    /// left it). `is_bind_active()` is the negation of this.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BindState::Idle | BindState::Paired | BindState::Failed | BindState::Aborted
        )
    }

    /// The non-terminal set the peer-presence poller loops on.
    pub fn is_active(&self) -> bool {
        !self.is_terminal()
    }

    /// The no-progress budget for THIS phase, in seconds. The session watchdog is
    /// a no-progress detector, not one fixed global timer: it re-arms on every
    /// transition to `phase_entered_at + budget(state)`, so a healthy bind whose
    /// phases legitimately sum past any single budget never trips, while a phase
    /// parked past its own budget (a genuine wedge) fires with the real phase
    /// named. Terminal states return `None` (nothing left to watch).
    ///
    /// Budgets mirror the per-phase timeouts: tunnel bring-up
    /// (`TUNNEL_WAIT_TIMEOUT`), the unanswered-rendezvous wedge window
    /// (`WAITING_PEER_WATCHDOG`), the key transfer + apply window
    /// (`KEY_TRANSFER_TIMEOUT` each — once a peer connects the exchange completes
    /// in seconds), and the service-restart budget (`RESTART_TIMEOUT`).
    pub fn watchdog_budget(&self) -> Option<std::time::Duration> {
        match self {
            BindState::OpeningTunnel => Some(super::TUNNEL_WAIT_TIMEOUT),
            BindState::WaitingPeer => Some(super::WAITING_PEER_WATCHDOG),
            BindState::TransferringKeys | BindState::ApplyingKeys => {
                Some(super::KEY_TRANSFER_TIMEOUT)
            }
            BindState::RestartingServices => Some(super::RESTART_TIMEOUT),
            BindState::Idle | BindState::Paired | BindState::Failed | BindState::Aborted => None,
        }
    }
}

/// Monotonic seconds since the first call (process start). Matches Python's
/// `time.monotonic()` shape: a stable float, process-relative.
pub fn now_monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// Current UTC timestamp, `YYYY-MM-DDTHH:MM:SS+00:00` — byte-identical to
/// Python `datetime.now(UTC).isoformat(timespec="seconds")`.
pub fn iso_now() -> String {
    use time::macros::format_description;
    const FMT: &[time::format_description::FormatItem<'_>] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]+00:00");
    time::OffsetDateTime::now_utc()
        .format(FMT)
        .unwrap_or_default()
}

/// Outcome of preparing the injection interface(s) for monitor mode just before
/// the bind unit starts. Surfaced on the bind sentinel so the pairing UI can show
/// *why* a bind is failing (a managed-mode injection iface radiates nothing, so
/// the peer never appears) instead of a bare timeout, and mirrored to the logging
/// store as a durable `radio.bind_precheck` event for offline RCA.
#[derive(Debug, Clone)]
pub struct BindPrecheck {
    /// True when every injection candidate reached verified monitor mode.
    pub ok: bool,
    /// Bland reason code when not ok: `iface_not_found` | `monitor_unverified`.
    pub reason: Option<&'static str>,
    /// The readback operating mode of the injection iface (`monitor` | `managed`
    /// | `unknown`). The single most diagnostic field — `managed` means no RF.
    pub injection_mode: String,
    /// Whether NetworkManager enumerates the injection iface. The legacy
    /// wfb-server `nmcli device show` precheck aborted when this was false;
    /// informational now the precheck is skipped, but it predicts that failure
    /// mode if the skip is ever reverted.
    pub nm_enumerable: bool,
    /// The injection interface the summary describes, when one was found.
    pub iface: Option<String>,
}

/// One bind attempt. Cloned cheaply for status snapshots + the sentinel write.
#[derive(Debug, Clone)]
pub struct BindSession {
    pub session_id: String,
    pub role: BindRole,
    pub state: BindState,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub fingerprint: Option<String>,
    pub peer_device_id: Option<String>,
    pub source: String,
    /// Monotonic float stamped on every transition (for `phase_age_s`).
    pub phase_entered_at: Option<f64>,
    /// Monotonic float stamped when the bind TUN RX counter last advanced.
    pub last_frame_at: Option<f64>,
    pub last_rssi_dbm: Option<i32>,
    /// The injection-iface monitor-mode preparation result for this session,
    /// stamped once the prep runs (None until then). Additive on the sentinel.
    pub bind_precheck: Option<BindPrecheck>,
    /// Whether the success path proved a real peer participated (decoded
    /// traffic on the bind tunnel, and for the drone a key file freshly
    /// deposited this session). None until the gate runs; false is stamped on
    /// the no-peer failure so the UI can name the cause. Additive on the
    /// sentinel.
    pub peer_verified: Option<bool>,
}

impl BindSession {
    /// Open a fresh session in `Idle`. `started_at` is stamped now; the first
    /// `transition()` sets `phase_entered_at`.
    pub fn new(role: BindRole, source: &str, peer_device_id: Option<String>) -> Self {
        Self {
            session_id: new_session_id(),
            role,
            state: BindState::Idle,
            started_at: iso_now(),
            finished_at: None,
            error: None,
            fingerprint: None,
            peer_device_id,
            source: source.to_string(),
            phase_entered_at: None,
            last_frame_at: None,
            last_rssi_dbm: None,
            bind_precheck: None,
            peer_verified: None,
        }
    }

    /// Move to `new_state` and stamp the monotonic phase clock. Centralising
    /// the pairing of `state` + `phase_entered_at` keeps the two in lock-step.
    pub fn transition(&mut self, new_state: BindState) {
        self.state = new_state;
        self.phase_entered_at = Some(now_monotonic());
    }

    /// The REST/sentinel snapshot — same key set + ordering as Python
    /// `BindSession.to_dict()`, with `phase_age_s` / `last_frame_age_s` derived
    /// from the monotonic clocks.
    pub fn to_json(&self) -> Value {
        let now = now_monotonic();
        let phase_age_s = self.phase_entered_at.map(|t| (now - t).max(0.0));
        let last_frame_age_s = self.last_frame_at.map(|t| (now - t).max(0.0));
        json!({
            "session_id": self.session_id,
            "role": self.role.as_str(),
            "state": self.state.as_str(),
            "phase": self.state.as_str(),
            "phase_entered_at": self.phase_entered_at,
            "phase_age_s": phase_age_s,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "error": self.error,
            "fingerprint": self.fingerprint,
            "peer_device_id": self.peer_device_id,
            "source": self.source,
            "last_frame_at_s": self.last_frame_at,
            "last_rssi_dbm": self.last_rssi_dbm,
            "last_frame_age_s": last_frame_age_s,
            // Injection-iface monitor-mode prep result (additive; null until the
            // prep runs). Lets the pairing UI explain a stuck bind at a glance.
            "bind_precheck_ok": self.bind_precheck.as_ref().map(|p| p.ok),
            "bind_precheck_reason": self.bind_precheck.as_ref().and_then(|p| p.reason),
            "injection_mode": self.bind_precheck.as_ref().map(|p| p.injection_mode.clone()),
            "nm_enumerable": self.bind_precheck.as_ref().map(|p| p.nm_enumerable),
            // Whether the success gate proved a real peer (additive; null until
            // the gate runs). A Paired session always carries true; a failed
            // session carries false when the wire protocol completed without
            // evidence of a peer (the solo/self-bind case).
            "peer_verified": self.peer_verified,
        })
    }
}

/// A uuid4-shaped random identifier (`8-4-4-4-12` lowercase hex). The Python
/// side uses `uuid.uuid4()`; only uniqueness + the `[:8]` prefix matter.
fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    // Best-effort: a fallback to a monotonic-derived seed keeps the id unique
    // even if the OS RNG is briefly unavailable (it never is on the target).
    if getrandom::fill(&mut bytes).is_err() {
        let seed = now_monotonic().to_bits().to_le_bytes();
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = seed[i % seed.len()] ^ (i as u8);
        }
    }
    let h = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests;
