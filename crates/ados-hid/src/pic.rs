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
//! claimed(A) --on_pic_disconnected()---> unclaimed
//! ```
//!
//! Ports `src/ados/services/ground_station/pic_arbiter.py`: the five claim
//! cases with their exact outcome shapes (the REST layer maps `needs_confirm` /
//! `status` to HTTP 409 / 403 / 410), the confirm-token TTL, the heartbeat
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

/// Cadence at which the session watchdog checks the last heartbeat age.
pub const WATCHDOG_INTERVAL_SECONDS: f64 = 5.0;

/// A monotonic, injectable time source.
///
/// `monotonic()` is fractional seconds from an arbitrary epoch (used for TTL
/// and heartbeat-age math; only differences are meaningful). `wall_ms()` is
/// unix milliseconds stamped onto emitted events. Production wiring uses
/// [`SystemClock`]; tests use a manually advanced clock so token expiry and the
/// watchdog can be exercised without real time passing.
pub trait Clock: Send + Sync {
    fn monotonic(&self) -> f64;
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
    Unclaimed,
    Claimed,
}

impl PicState {
    pub fn as_str(self) -> &'static str {
        match self {
            PicState::Unclaimed => "unclaimed",
            PicState::Claimed => "claimed",
        }
    }
}

/// The outcome of a [`PicArbiter::claim`] call. Variants carry exactly the
/// fields the REST layer serializes; `status` is the HTTP code the rejection
/// variants map to.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    /// Fresh claim from `unclaimed`.
    Fresh {
        claimed_by: String,
        claim_counter: u64,
    },
    /// Same client re-claimed; nothing changed.
    Idempotent {
        claimed_by: String,
        claim_counter: u64,
    },
    /// Force takeover (logged at WARN); always wins.
    Forced {
        claimed_by: String,
        claim_counter: u64,
        previous_pic: Option<String>,
    },
    /// Confirm-token matched and was unexpired; transfer completed.
    Transferred {
        claimed_by: String,
        claim_counter: u64,
        transferred_from: Option<String>,
    },
    /// A confirm token was supplied but missing or expired. -> HTTP 409.
    InvalidConfirmToken {
        current_pic: Option<String>,
        status: u16,
    },
    /// Already claimed, no token, no force. -> HTTP 409.
    AlreadyClaimed {
        current_pic: Option<String>,
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
    /// (`needs_confirm` on the Python side).
    pub fn needs_confirm(&self) -> bool {
        matches!(
            self,
            ClaimOutcome::InvalidConfirmToken { .. } | ClaimOutcome::AlreadyClaimed { .. }
        )
    }

    /// The HTTP status the REST layer returns (200 on success, 409 on the
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
    Released {
        previous_pic: Option<String>,
    },
    /// Caller does not currently hold PIC. -> HTTP 403.
    NotCurrentPic {
        current_pic: Option<String>,
        status: u16,
    },
}

/// Outcome of [`PicArbiter::heartbeat`].
#[derive(Debug, Clone, PartialEq)]
pub enum HeartbeatOutcome {
    Ok {
        claimed_by: Option<String>,
        claim_counter: u64,
        last_heartbeat_ts: f64,
    },
    /// No active claim from this client. -> HTTP 410.
    NoActiveClaim {
        current_pic: Option<String>,
        status: u16,
    },
}

/// A point-in-time snapshot of the arbiter state.
#[derive(Debug, Clone, PartialEq)]
pub struct PicStateSnapshot {
    pub state: PicState,
    pub claimed_by: Option<String>,
    pub claimed_since: Option<f64>,
    pub claim_counter: u64,
    pub primary_gamepad_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ConfirmToken {
    token: String,
    expires_at: f64,
}

/// The PIC arbiter. Single-threaded by contract: the owning daemon serializes
/// calls (the Python version takes `self._lock` around every mutation; here the
/// caller holds the arbiter behind its own `Mutex`). Time is injected via
/// [`Clock`].
pub struct PicArbiter {
    state: PicState,
    claimed_by: Option<String>,
    claimed_since: Option<f64>,
    claim_counter: u64,
    primary_gamepad_id: Option<String>,
    last_heartbeat_ts: Option<f64>,
    confirm_tokens: HashMap<String, ConfirmToken>,
    bus: PicEventBus,
    clock: Box<dyn Clock>,
}

impl PicArbiter {
    /// Build an arbiter with the real system clock.
    pub fn new() -> Self {
        Self::with_clock(Box::new(SystemClock))
    }

    /// Build an arbiter with an injected clock (tests).
    pub fn with_clock(clock: Box<dyn Clock>) -> Self {
        Self {
            state: PicState::Unclaimed,
            claimed_by: None,
            claimed_since: None,
            claim_counter: 0,
            primary_gamepad_id: None,
            last_heartbeat_ts: None,
            confirm_tokens: HashMap::new(),
            bus: PicEventBus::new(),
            clock,
        }
    }

    /// The event bus other tasks subscribe to.
    pub fn bus(&self) -> &PicEventBus {
        &self.bus
    }

    fn now(&self) -> f64 {
        self.clock.monotonic()
    }

    fn now_ms(&self) -> i64 {
        self.clock.wall_ms()
    }

    fn purge_expired_tokens(&mut self) {
        let now = self.now();
        self.confirm_tokens.retain(|_, t| t.expires_at >= now);
    }

    /// Transition to claimed(client_id). Bumps the monotonic counter.
    fn issue_claim(&mut self, client_id: &str) -> u64 {
        let now = self.now();
        self.state = PicState::Claimed;
        self.claimed_by = Some(client_id.to_string());
        self.claimed_since = Some(now);
        self.last_heartbeat_ts = Some(now);
        self.claim_counter += 1;
        self.claim_counter
    }

    fn emit(&self, kind: PicEventKind, client_id: Option<String>, claim_counter: u64) {
        self.bus.publish(PicEvent {
            kind,
            client_id,
            claim_counter,
            timestamp_ms: self.now_ms(),
        });
    }

    /// Attempt to claim PIC for `client_id`.
    ///
    /// The five cases follow `pic_arbiter.py` exactly:
    /// 1. unclaimed -> grant ([`ClaimOutcome::Fresh`]);
    /// 2. same client -> idempotent;
    /// 3. `force` -> takeover (WARN);
    /// 4. matching unexpired confirm token -> transfer (token popped);
    /// 5. otherwise -> already-claimed, 409, needs-confirm.
    pub fn claim(
        &mut self,
        client_id: &str,
        confirm_token: Option<&str>,
        force: bool,
    ) -> ClaimOutcome {
        self.purge_expired_tokens();

        // Case 1: nobody holds PIC. Immediate claim.
        if self.state == PicState::Unclaimed {
            let counter = self.issue_claim(client_id);
            self.emit(PicEventKind::Claimed, Some(client_id.to_string()), counter);
            return ClaimOutcome::Fresh {
                claimed_by: client_id.to_string(),
                claim_counter: counter,
            };
        }

        // Case 2: PIC held, same client re-claims. Idempotent.
        if self.claimed_by.as_deref() == Some(client_id) {
            return ClaimOutcome::Idempotent {
                claimed_by: client_id.to_string(),
                claim_counter: self.claim_counter,
            };
        }

        // Case 3: force takeover. Always wins, logged at WARN.
        if force {
            let previous = self.claimed_by.clone();
            let counter = self.issue_claim(client_id);
            tracing::warn!(
                previous_pic = ?previous,
                new_pic = client_id,
                claim_counter = counter,
                "pic force takeover"
            );
            self.emit(PicEventKind::Claimed, Some(client_id.to_string()), counter);
            return ClaimOutcome::Forced {
                claimed_by: client_id.to_string(),
                claim_counter: counter,
                previous_pic: previous,
            };
        }

        // Case 4: confirm-token flow.
        if let Some(token) = confirm_token {
            let valid = self
                .confirm_tokens
                .get(client_id)
                .map(|stored| stored.token == token && stored.expires_at >= self.now())
                .unwrap_or(false);
            if valid {
                let previous = self.claimed_by.clone();
                self.confirm_tokens.remove(client_id);
                let counter = self.issue_claim(client_id);
                self.emit(PicEventKind::Claimed, Some(client_id.to_string()), counter);
                return ClaimOutcome::Transferred {
                    claimed_by: client_id.to_string(),
                    claim_counter: counter,
                    transferred_from: previous,
                };
            }
            return ClaimOutcome::InvalidConfirmToken {
                current_pic: self.claimed_by.clone(),
                status: 409,
            };
        }

        // Case 5: already claimed, no token, no force. 409.
        ClaimOutcome::AlreadyClaimed {
            current_pic: self.claimed_by.clone(),
            status: 409,
        }
    }

    /// Release PIC if `client_id` currently holds it.
    pub fn release(&mut self, client_id: &str) -> ReleaseOutcome {
        if self.state != PicState::Claimed || self.claimed_by.as_deref() != Some(client_id) {
            return ReleaseOutcome::NotCurrentPic {
                current_pic: self.claimed_by.clone(),
                status: 403,
            };
        }
        let previous = self.claimed_by.clone();
        self.state = PicState::Unclaimed;
        self.claimed_by = None;
        self.claimed_since = None;
        let counter = self.claim_counter;
        self.emit(PicEventKind::Released, previous.clone(), counter);
        ReleaseOutcome::Released {
            previous_pic: previous,
        }
    }

    /// Current arbiter state snapshot.
    pub fn get_state(&self) -> PicStateSnapshot {
        PicStateSnapshot {
            state: self.state,
            claimed_by: self.claimed_by.clone(),
            claimed_since: self.claimed_since,
            claim_counter: self.claim_counter,
            primary_gamepad_id: self.primary_gamepad_id.clone(),
        }
    }

    /// Mint a 32-char hex confirm token for `client_id`, bound to the
    /// requesting client and valid for [`CONFIRM_TTL_SECONDS`]. Only a
    /// subsequent `claim(client_id, Some(token), false)` from the same client
    /// within the window completes the transfer.
    pub fn create_confirm_token(&mut self, client_id: &str) -> String {
        let token = random_hex_16();
        self.purge_expired_tokens();
        self.confirm_tokens.insert(
            client_id.to_string(),
            ConfirmToken {
                token: token.clone(),
                expires_at: self.now() + CONFIRM_TTL_SECONDS,
            },
        );
        token
    }

    /// Record a heartbeat for the active PIC holder.
    pub fn heartbeat(&mut self, client_id: &str) -> HeartbeatOutcome {
        if self.state != PicState::Claimed || self.claimed_by.as_deref() != Some(client_id) {
            return HeartbeatOutcome::NoActiveClaim {
                current_pic: self.claimed_by.clone(),
                status: 410,
            };
        }
        let ts = self.now();
        self.last_heartbeat_ts = Some(ts);
        HeartbeatOutcome::Ok {
            claimed_by: self.claimed_by.clone(),
            claim_counter: self.claim_counter,
            last_heartbeat_ts: ts,
        }
    }

    /// One watchdog tick: if PIC is held and the last heartbeat is older than
    /// [`HEARTBEAT_TIMEOUT_SECONDS`], auto-release and return the released
    /// client. The owning daemon calls this every
    /// [`WATCHDOG_INTERVAL_SECONDS`]; pulling the cadence out makes the timeout
    /// testable without sleeping.
    pub fn watchdog_tick(&mut self) -> Option<String> {
        if self.state != PicState::Claimed {
            return None;
        }
        let (Some(holder), Some(last)) = (self.claimed_by.clone(), self.last_heartbeat_ts) else {
            return None;
        };
        let age = self.now() - last;
        if age > HEARTBEAT_TIMEOUT_SECONDS {
            match self.release(&holder) {
                ReleaseOutcome::Released { .. } => Some(holder),
                ReleaseOutcome::NotCurrentPic { .. } => None,
            }
        } else {
            None
        }
    }

    /// Auto-claim PIC for `client_id_hint` if nobody holds it. Called on a
    /// gamepad hotplug-connect event so the single-operator bench rig has a
    /// working control loop with no REST round-trip. Records the gamepad as the
    /// primary bound to this PIC. No-op if PIC is already held.
    pub fn on_gamepad_connected(&mut self, device_id: &str, client_id_hint: &str) {
        if self.state == PicState::Unclaimed {
            let counter = self.issue_claim(client_id_hint);
            self.primary_gamepad_id = Some(device_id.to_string());
            self.emit(
                PicEventKind::Claimed,
                Some(client_id_hint.to_string()),
                counter,
            );
        }
    }

    /// Clear the primary-gamepad binding (e.g. when that gamepad is removed).
    pub fn primary_gamepad_id(&self) -> Option<&str> {
        self.primary_gamepad_id.as_deref()
    }

    /// Handle a PIC client disconnect (WS drop or gamepad removal). Clears PIC
    /// state and publishes a disconnected event. FC failsafe is configured on
    /// the FC itself; this hook is where an explicit RTL trigger would layer in
    /// a later chunk. No-op if PIC is not held.
    pub fn on_pic_disconnected(&mut self) {
        if self.state != PicState::Claimed {
            return;
        }
        let previous = self.claimed_by.clone();
        self.state = PicState::Unclaimed;
        self.claimed_by = None;
        self.claimed_since = None;
        let counter = self.claim_counter;
        tracing::warn!(
            previous_pic = ?previous,
            claim_counter = counter,
            "pic disconnected; FC failsafe config governs RC_LOSS"
        );
        self.emit(PicEventKind::Disconnected, previous, counter);
    }
}

impl Default for PicArbiter {
    fn default() -> Self {
        Self::new()
    }
}

/// 16 random bytes rendered as 32 lowercase hex chars, matching Python
/// `secrets.token_hex(16)`.
fn random_hex_16() -> String {
    let mut buf = [0u8; 16];
    // getrandom only fails on a platform without an entropy source, which the
    // agent targets never are; fall back to a time-seeded mix rather than
    // panicking inside the arbiter.
    if getrandom::getrandom(&mut buf).is_err() {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((seed >> (i % 16 * 8)) as u8) ^ (i as u8);
        }
    }
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// A manually advanced clock. `monotonic()` is the stored seconds value;
    /// `wall_ms()` is derived from it so emitted timestamps move with the test.
    #[derive(Clone, Default)]
    struct FakeClock {
        secs_millis: Arc<AtomicU64>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                secs_millis: Arc::new(AtomicU64::new(0)),
            }
        }
        fn advance(&self, secs: f64) {
            let add = (secs * 1000.0) as u64;
            self.secs_millis.fetch_add(add, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn monotonic(&self) -> f64 {
            self.secs_millis.load(Ordering::SeqCst) as f64 / 1000.0
        }
        fn wall_ms(&self) -> i64 {
            self.secs_millis.load(Ordering::SeqCst) as i64
        }
    }

    fn arbiter() -> (PicArbiter, FakeClock) {
        let clock = FakeClock::new();
        let arb = PicArbiter::with_clock(Box::new(clock.clone()));
        (arb, clock)
    }

    #[test]
    fn case1_fresh_claim_from_unclaimed() {
        let (mut a, _c) = arbiter();
        let out = a.claim("op-a", None, false);
        assert_eq!(
            out,
            ClaimOutcome::Fresh {
                claimed_by: "op-a".into(),
                claim_counter: 1,
            }
        );
        assert!(out.claimed());
        assert_eq!(out.http_status(), 200);
        assert_eq!(a.get_state().state, PicState::Claimed);
        assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
    }

    #[test]
    fn case2_same_client_is_idempotent() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        let out = a.claim("op-a", None, false);
        assert_eq!(
            out,
            ClaimOutcome::Idempotent {
                claimed_by: "op-a".into(),
                claim_counter: 1,
            }
        );
        // Counter did NOT advance on the idempotent re-claim.
        assert_eq!(a.get_state().claim_counter, 1);
    }

    #[test]
    fn case3_force_takeover_wins() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        let out = a.claim("op-b", None, true);
        assert_eq!(
            out,
            ClaimOutcome::Forced {
                claimed_by: "op-b".into(),
                claim_counter: 2,
                previous_pic: Some("op-a".into()),
            }
        );
        assert!(out.claimed());
        assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-b"));
    }

    #[test]
    fn case4_confirm_token_match_transfers() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        let token = a.create_confirm_token("op-b");
        assert_eq!(token.len(), 32);
        let out = a.claim("op-b", Some(&token), false);
        assert_eq!(
            out,
            ClaimOutcome::Transferred {
                claimed_by: "op-b".into(),
                claim_counter: 2,
                transferred_from: Some("op-a".into()),
            }
        );
        assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-b"));
        // Token is single-use: a second attempt with the same token fails.
        a.claim("op-a", None, true); // op-a takes it back via force
        let out2 = a.claim("op-b", Some(&token), false);
        assert!(matches!(out2, ClaimOutcome::InvalidConfirmToken { .. }));
    }

    #[test]
    fn case5_already_claimed_no_token_409() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        let out = a.claim("op-b", None, false);
        assert_eq!(
            out,
            ClaimOutcome::AlreadyClaimed {
                current_pic: Some("op-a".into()),
                status: 409,
            }
        );
        assert!(!out.claimed());
        assert!(out.needs_confirm());
        assert_eq!(out.http_status(), 409);
        // op-a still holds it.
        assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
    }

    #[test]
    fn confirm_token_expires_after_ttl() {
        let (mut a, c) = arbiter();
        a.claim("op-a", None, false);
        let token = a.create_confirm_token("op-b");
        // Past the TTL window the token no longer transfers.
        c.advance(CONFIRM_TTL_SECONDS + 0.01);
        let out = a.claim("op-b", Some(&token), false);
        assert_eq!(
            out,
            ClaimOutcome::InvalidConfirmToken {
                current_pic: Some("op-a".into()),
                status: 409,
            }
        );
        assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
    }

    #[test]
    fn confirm_token_valid_at_edge_of_ttl() {
        let (mut a, c) = arbiter();
        a.claim("op-a", None, false);
        let token = a.create_confirm_token("op-b");
        // Just inside the window: expires_at >= now still holds.
        c.advance(CONFIRM_TTL_SECONDS - 0.01);
        let out = a.claim("op-b", Some(&token), false);
        assert!(matches!(out, ClaimOutcome::Transferred { .. }));
    }

    #[test]
    fn release_by_holder_succeeds() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        let out = a.release("op-a");
        assert_eq!(
            out,
            ReleaseOutcome::Released {
                previous_pic: Some("op-a".into()),
            }
        );
        assert_eq!(a.get_state().state, PicState::Unclaimed);
    }

    #[test]
    fn release_by_non_holder_403() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        let out = a.release("op-b");
        assert_eq!(
            out,
            ReleaseOutcome::NotCurrentPic {
                current_pic: Some("op-a".into()),
                status: 403,
            }
        );
        // op-a still holds it.
        assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
    }

    #[test]
    fn heartbeat_by_holder_ok_by_other_410() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        match a.heartbeat("op-a") {
            HeartbeatOutcome::Ok {
                claimed_by,
                claim_counter,
                ..
            } => {
                assert_eq!(claimed_by.as_deref(), Some("op-a"));
                assert_eq!(claim_counter, 1);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        let out = a.heartbeat("op-b");
        assert_eq!(
            out,
            HeartbeatOutcome::NoActiveClaim {
                current_pic: Some("op-a".into()),
                status: 410,
            }
        );
    }

    #[test]
    fn heartbeat_when_unclaimed_410() {
        let (mut a, _c) = arbiter();
        let out = a.heartbeat("op-a");
        assert_eq!(
            out,
            HeartbeatOutcome::NoActiveClaim {
                current_pic: None,
                status: 410,
            }
        );
    }

    #[test]
    fn watchdog_auto_releases_after_timeout() {
        let (mut a, c) = arbiter();
        a.claim("op-a", None, false);
        // Within timeout: no release.
        c.advance(HEARTBEAT_TIMEOUT_SECONDS - 1.0);
        assert_eq!(a.watchdog_tick(), None);
        assert_eq!(a.get_state().state, PicState::Claimed);
        // Past timeout: auto-release.
        c.advance(2.0);
        assert_eq!(a.watchdog_tick().as_deref(), Some("op-a"));
        assert_eq!(a.get_state().state, PicState::Unclaimed);
        // A second tick has nothing to release.
        assert_eq!(a.watchdog_tick(), None);
    }

    #[test]
    fn heartbeat_keeps_claim_alive_through_watchdog() {
        let (mut a, c) = arbiter();
        a.claim("op-a", None, false);
        c.advance(HEARTBEAT_TIMEOUT_SECONDS - 1.0);
        a.heartbeat("op-a"); // refreshes last_heartbeat_ts to "now"
        c.advance(HEARTBEAT_TIMEOUT_SECONDS - 1.0);
        // Total elapsed since claim is > timeout, but the heartbeat reset the
        // clock, so the holder survives.
        assert_eq!(a.watchdog_tick(), None);
        assert_eq!(a.get_state().state, PicState::Claimed);
    }

    #[test]
    fn gamepad_connect_auto_claims_when_unclaimed() {
        let (mut a, _c) = arbiter();
        a.on_gamepad_connected("evdev-0", "hdmi-kiosk");
        assert_eq!(a.get_state().state, PicState::Claimed);
        assert_eq!(a.get_state().claimed_by.as_deref(), Some("hdmi-kiosk"));
        assert_eq!(a.primary_gamepad_id(), Some("evdev-0"));
    }

    #[test]
    fn gamepad_connect_is_noop_when_claimed() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        a.on_gamepad_connected("evdev-0", "hdmi-kiosk");
        // op-a stays PIC; the gamepad did not steal it.
        assert_eq!(a.get_state().claimed_by.as_deref(), Some("op-a"));
        assert_eq!(a.get_state().claim_counter, 1);
    }

    #[test]
    fn pic_disconnect_clears_state() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false);
        a.on_pic_disconnected();
        assert_eq!(a.get_state().state, PicState::Unclaimed);
        assert!(a.get_state().claimed_by.is_none());
        // Idempotent when already unclaimed.
        a.on_pic_disconnected();
        assert_eq!(a.get_state().state, PicState::Unclaimed);
    }

    #[test]
    fn claim_counter_is_monotonic_across_transitions() {
        let (mut a, _c) = arbiter();
        a.claim("op-a", None, false); // 1
        a.claim("op-b", None, true); // 2 (force)
        a.release("op-b"); // counter unchanged
        a.claim("op-c", None, false); // 3
        assert_eq!(a.get_state().claim_counter, 3);
    }

    #[tokio::test]
    async fn claim_emits_a_bus_event() {
        let (mut a, _c) = arbiter();
        let mut rx = a.bus().subscribe();
        a.claim("op-a", None, false);
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.kind, PicEventKind::Claimed);
        assert_eq!(ev.client_id.as_deref(), Some("op-a"));
        assert_eq!(ev.claim_counter, 1);
    }

    #[test]
    fn confirm_tokens_are_unique() {
        let (mut a, _c) = arbiter();
        let t1 = a.create_confirm_token("op-a");
        let t2 = a.create_confirm_token("op-b");
        assert_ne!(t1, t2);
        assert!(t1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
