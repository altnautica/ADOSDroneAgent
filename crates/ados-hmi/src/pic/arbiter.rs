//! `PicArbiter` state transitions, watchdog and confirm tokens.

use super::*;

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
            primary_gamepad_hint: None,
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

    /// Transition to claimed(client_id). Bumps the monotonic counter. A new
    /// holder other than the one the primary gamepad was bound for unbinds the
    /// gamepad, so its later unplug cannot drop someone else's claim.
    fn issue_claim(&mut self, client_id: &str) -> u64 {
        if self.primary_gamepad_hint.as_deref() != Some(client_id) {
            self.clear_primary_gamepad();
        }
        let now = self.now();
        self.state = PicState::Claimed;
        self.claimed_by = Some(client_id.to_string());
        self.claimed_since = Some(now);
        self.last_heartbeat_ts = Some(now);
        self.claim_counter += 1;
        self.claim_counter
    }

    fn clear_primary_gamepad(&mut self) {
        self.primary_gamepad_id = None;
        self.primary_gamepad_hint = None;
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
        self.clear_primary_gamepad();
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
    /// client. The owner calls this every
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
    /// working control loop with no request round-trip. Records the gamepad as the
    /// primary bound to this PIC. No-op if PIC is already held.
    pub fn on_gamepad_connected(&mut self, device_id: &str, client_id_hint: &str) {
        if self.state == PicState::Unclaimed {
            self.primary_gamepad_hint = Some(client_id_hint.to_string());
            let counter = self.issue_claim(client_id_hint);
            self.primary_gamepad_id = Some(device_id.to_string());
            self.emit(
                PicEventKind::Claimed,
                Some(client_id_hint.to_string()),
                counter,
            );
        }
    }

    /// The gamepad bound to the current claim, if any.
    pub fn primary_gamepad_id(&self) -> Option<&str> {
        self.primary_gamepad_id.as_deref()
    }

    /// Handle a PIC client disconnect (WS drop or gamepad removal) for
    /// `client_id`, the holder the caller believes it is dropping. Clears PIC
    /// only when `client_id` holds it; another holder gets
    /// [`DisconnectOutcome::NotHolder`] and keeps the claim. FC failsafe is
    /// configured on the FC itself. No-op when PIC is not held.
    pub fn on_pic_disconnected(&mut self, client_id: &str) -> DisconnectOutcome {
        if self.state != PicState::Claimed {
            self.clear_primary_gamepad();
            return DisconnectOutcome::Disconnected;
        }
        if self.claimed_by.as_deref() != Some(client_id) {
            return DisconnectOutcome::NotHolder {
                current_pic: self.claimed_by.clone(),
                status: 409,
            };
        }
        let previous = self.claimed_by.clone();
        self.state = PicState::Unclaimed;
        self.claimed_by = None;
        self.claimed_since = None;
        self.clear_primary_gamepad();
        let counter = self.claim_counter;
        tracing::warn!(
            previous_pic = ?previous,
            claim_counter = counter,
            "pic disconnected; FC failsafe config governs RC_LOSS"
        );
        self.emit(PicEventKind::Disconnected, previous, counter);
        DisconnectOutcome::Disconnected
    }
}

impl Default for PicArbiter {
    fn default() -> Self {
        Self::new()
    }
}

/// 16 random bytes rendered as 32 lowercase hex chars, as a confirm token.
fn random_hex_16() -> String {
    let mut buf = [0u8; 16];
    // getrandom only fails on a platform without an entropy source, which the
    // agent targets never are; fall back to a time-seeded mix rather than
    // panicking inside the arbiter.
    if getrandom::fill(&mut buf).is_err() {
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
