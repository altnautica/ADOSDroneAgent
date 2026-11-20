//! The drone's hop gating state, its history ring and the respawn-channel rule.

use std::time::{Duration, Instant};

use super::{now_unix, PeerPresence, PEER_STALE_SECS};

/// One entry in the hop history ring (the `hop-supervisor.json` `history` list).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HopHistoryEntry {
    pub at: f64,
    pub from: u8,
    pub to: u8,
    pub trigger: String,
    pub ok: bool,
}

/// Gating state — the drone only hops after the first peer ACK is received.
#[derive(Debug, Default)]
pub struct HopState {
    /// Whether a peer has ever been seen on this link.
    pub(super) was_linked: bool,
    /// Monotonic instant of the last peer beacon received.
    pub(super) peer_last_seen: Option<Instant>,
    /// Wall-clock unix of the last peer beacon (for `peer-presence.json`).
    peer_last_seen_unix: Option<f64>,
    /// Decoded peer (device-id / role / channel / rssi) from the last beacon.
    peer: Option<PeerPresence>,
    /// Current operating channel.
    pub channel: u8,
    /// Home (rendezvous) channel from config — never modified at runtime.
    pub home_channel: u8,
    /// Last hop instant, for reactive cooldown (30s).
    last_hop_at: Option<Instant>,
    /// Wall-clock unix of the last hop (for `hop-supervisor.json`).
    last_hop_at_unix: Option<f64>,
    /// Hop history ring (last 32 kept on read).
    history: Vec<HopHistoryEntry>,
    /// Whether a return to the rendezvous channel has succeeded since the peer
    /// was last heard. Cleared by every peer beacon.
    returned_home_this_outage: bool,
}

/// The monotonic clock every hop gate reads: tokio's, which is the system
/// monotonic clock in the service and a virtual one under a paused runtime (the
/// deterministic simulation in `tests/dst`), so peer ages and the hop cooldown
/// advance with the simulated time the rest of the supervisor runs on.
fn mono_now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// Time since `t` on [`mono_now`]'s clock.
fn since(t: Instant) -> Duration {
    mono_now().saturating_duration_since(t)
}

impl HopState {
    pub fn new(home_channel: u8) -> Self {
        Self {
            channel: home_channel,
            home_channel,
            ..Default::default()
        }
    }

    /// Record a verified PresenceBeacon from the peer (drives the link gate +
    /// the `peer-presence.json` sidecar).
    pub fn on_peer_beacon(&mut self, presence: PeerPresence) {
        self.was_linked = true;
        self.peer_last_seen = Some(mono_now());
        self.peer_last_seen_unix = Some(now_unix());
        self.peer = Some(presence);
        self.returned_home_this_outage = false;
    }

    /// The decoded peer, if one has been seen.
    pub fn peer(&self) -> Option<&PeerPresence> {
        self.peer.as_ref()
    }

    /// Wall-clock unix of the last peer beacon (for the sidecar).
    pub fn peer_last_seen_unix(&self) -> Option<f64> {
        self.peer_last_seen_unix
    }

    /// Wall-clock unix of the last hop (for the sidecar).
    pub fn last_hop_at_unix(&self) -> Option<f64> {
        self.last_hop_at_unix
    }

    /// The last 32 hop-history entries (the sidecar `history` list).
    pub fn history(&self) -> &[HopHistoryEntry] {
        let n = self.history.len();
        &self.history[n.saturating_sub(32)..]
    }

    /// Record a verified HopAck from the peer. The peer heard this side's
    /// announce and answered on this side's channel, so it is as present as a
    /// beacon would show: the link gate and the peer's age are refreshed (the
    /// decoded peer record stays the last beacon's). Without this a peer that
    /// had just acked a hop could read as stale, and the same tick's return
    /// home took the drone off the channel the peer was following it to.
    pub fn on_peer_ack(&mut self) {
        self.was_linked = true;
        self.peer_last_seen = Some(mono_now());
        self.peer_last_seen_unix = Some(now_unix());
        self.returned_home_this_outage = false;
    }

    /// True if the peer has been silent for more than PEER_STALE_SECS.
    pub fn peer_is_stale(&self) -> bool {
        self.peer_last_seen
            .map(|t| since(t).as_secs_f64() > PEER_STALE_SECS)
            .unwrap_or(false)
    }

    /// True if a peer beacon was decoded within the last `secs` seconds.
    ///
    /// Used to skip the periodic `iw scan` while the link is healthy: the scan
    /// locks the radio for several seconds and drops `wfb_tx` frames, so when the
    /// control plane heard the peer recently the rescan is pure waste. `false`
    /// when no peer has ever been seen.
    pub fn peer_fresh_within(&self, secs: f64) -> bool {
        self.peer_last_seen
            .map(|t| since(t).as_secs_f64() < secs)
            .unwrap_or(false)
    }

    /// True if a periodic hop is allowed (link established, peer fresh <60s,
    /// 30s reactive cooldown met).
    pub fn can_hop(&self) -> bool {
        if !self.was_linked {
            return false;
        }
        if self.peer_is_stale() {
            return false;
        }
        if let Some(t) = self.last_hop_at {
            if since(t).as_secs() < 30 {
                return false;
            }
        }
        true
    }

    /// Should we return to the rendezvous channel? True once the peer has been
    /// gone for more than [`PEER_STALE_SECS`] after ever being linked, until a
    /// return succeeds.
    ///
    /// Not only when the recorded channel is off home: the record says where
    /// this side last committed, not where the radio is, and a peer that went
    /// silent may be waiting on rendezvous after a hop that half-completed. One
    /// return per outage re-asserts home on the radio itself; while off home it
    /// keeps retrying until it lands.
    pub fn should_return_home(&self) -> bool {
        self.was_linked
            && self.peer_is_stale()
            && (self.channel != self.home_channel || !self.returned_home_this_outage)
    }

    /// Record the outcome of a return to the rendezvous channel. A success ends
    /// the outage's return; a failure leaves it due on the next check.
    pub fn record_return_home(&mut self, ok: bool) {
        self.record_hop(self.home_channel, "return_home", ok);
        if ok {
            self.returned_home_this_outage = true;
        }
    }

    /// Record the channel a radio-group bring-up landed on ([`respawn_channel`]:
    /// the committed hop while the peer is fresh, else the rendezvous home).
    /// The record follows the radio. Left on the old hop channel after a
    /// bring-up on home, it outlived the outage: once the peer was heard on
    /// home again nothing corrected it, so the next respawn tuned back to the
    /// old hop channel and a failed hop's call-back named a channel nobody was
    /// on.
    pub fn record_bring_up(&mut self, channel: u8) {
        self.channel = channel;
    }

    /// True if a REACTIVE hop is allowed: the link was established and the 30s
    /// reactive cooldown has elapsed. Unlike [`can_hop`], this does NOT gate on
    /// peer-freshness — a degrading link (the reactive trigger) is exactly when
    /// the peer may be going stale, and fleeing the bad channel is the point.
    pub fn reactive_allowed(&self) -> bool {
        if !self.was_linked {
            return false;
        }
        match self.last_hop_at {
            Some(t) => since(t).as_secs() >= 30,
            None => true,
        }
    }

    /// Record a hop with an explicit trigger label + outcome (for the history
    /// ring). `from` is the channel before the hop.
    pub fn record_hop(&mut self, new_channel: u8, trigger: &str, ok: bool) {
        let from = self.channel;
        let now = now_unix();
        if ok {
            self.channel = new_channel;
            self.last_hop_at = Some(mono_now());
            self.last_hop_at_unix = Some(now);
        }
        self.history.push(HopHistoryEntry {
            at: now,
            from,
            to: new_channel,
            trigger: trigger.to_string(),
            ok,
        });
        // Bound the ring so it cannot grow without limit (we only ever expose
        // the last 32, but keep the Vec from ballooning over a long flight).
        if self.history.len() > 128 {
            let drop = self.history.len() - 64;
            self.history.drain(..drop);
        }
    }
}

/// The channel a whole-group respawn brings the radio up on: the hopped
/// channel while the peer was heard within [`PEER_STALE_SECS`] of `now` (it is
/// waiting there, and tuning home would split the pair), otherwise the
/// rendezvous home (the same return-home rule the hop loop applies).
pub fn respawn_channel(hop: &HopState, now: Instant) -> u8 {
    let peer_fresh = hop
        .peer_last_seen
        .is_some_and(|t| now.saturating_duration_since(t).as_secs_f64() <= PEER_STALE_SECS);
    if peer_fresh {
        hop.channel
    } else {
        hop.home_channel
    }
}
