use super::*;

/// Longest wait honoured before a follow. The drone's countdown is three
/// seconds from its first announce; anything longer is a malformed or forged
/// frame, so the retune happens at the cap rather than being parked.
pub(super) const MAX_FOLLOW_WAIT: Duration = Duration::from_secs(3);

/// How long after a follow the new channel must deliver valid packets before
/// the ground station reverts to the channel it left.
pub const FOLLOW_VERIFY_WINDOW: Duration = Duration::from_secs(5);

/// Why a verified HopAnnounce is not followed, or `None` to follow it.
///
/// The ground station acks only an announce it can carry out. An ack commits
/// the drone to the hop, so acking one this station then cannot follow is what
/// split drone and ground onto different channels:
///
/// * more than one registered slot: the one receive radio serves the whole
///   fleet, so following one drone's hop strands every other drone;
/// * no resolved receive interface: there is nothing to retune;
/// * a target outside the adapter's permitted channel set (an empty set means
///   it could not be read, which does not restrict).
pub fn hop_follow_refusal(
    fleet_slots: usize,
    iface_resolved: bool,
    permitted: &std::collections::BTreeSet<u8>,
    channel: u8,
) -> Option<&'static str> {
    if fleet_slots > 1 {
        return Some("fleet_hop_refused");
    }
    if !iface_resolved {
        return Some("no_receive_iface");
    }
    if !permitted.is_empty() && !permitted.contains(&channel) {
        return Some("channel_not_permitted");
    }
    None
}

impl HopFollower {
    pub fn new(setter: Arc<dyn ChannelSetter>, radio_group: crate::wfb_rx::RadioGroup) -> Self {
        Self {
            setter,
            radio_group,
            valid: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(None)),
            verify_window: FOLLOW_VERIFY_WINDOW,
        }
    }

    /// Point the follow verification at the current receive generation's
    /// valid-decode counter.
    pub fn set_valid_counter(&self, counter: SharedValidCounter) {
        *self.valid.lock().unwrap_or_else(|e| e.into_inner()) = Some(counter);
    }

    /// The resolved primary (transmit) interface, if any. The group setter
    /// carries every retune of it to the other receive adapters.
    async fn iface(&self) -> Option<String> {
        self.radio_group.lock().await.first().cloned()
    }

    /// Claim the follow toward `channel`. False when one is already scheduled.
    fn claim(&self, channel: u8) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if *pending == Some(channel) {
            return false;
        }
        *pending = Some(channel);
        true
    }

    fn release(&self, channel: u8) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if *pending == Some(channel) {
            *pending = None;
        }
    }

    /// Retune `iface` to `channel` after `delay`, then verify. `prev` is the
    /// channel to go back to when the follow does not hold; with none known the
    /// radio stays and the valid-packet watchdog owns recovery.
    pub(super) async fn follow(
        &self,
        iface: &str,
        channel: u8,
        delay: Duration,
        prev: Option<u8>,
        cache: &GsPresenceCache,
    ) -> FollowOutcome {
        tokio::time::sleep(delay.min(MAX_FOLLOW_WAIT)).await;
        if !self.setter.set_channel(iface, channel).await {
            tracing::warn!(interface = %iface, channel, "ground_hop_follow_retune_failed");
            self.revert(iface, prev, cache).await;
            return FollowOutcome::RevertedRetuneFailed;
        }
        tracing::info!(interface = %iface, channel, "ground_hop_follow_retuned");
        let counter = self.valid.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(counter) = counter else {
            return FollowOutcome::Followed;
        };
        let before = counter.get();
        tokio::time::sleep(self.verify_window).await;
        if counter.get() > before {
            return FollowOutcome::Followed;
        }
        tracing::warn!(
            interface = %iface,
            channel,
            window_s = self.verify_window.as_secs_f64(),
            "ground_hop_follow_no_traffic_reverting"
        );
        self.revert(iface, prev, cache).await;
        FollowOutcome::RevertedNoTraffic
    }

    async fn revert(&self, iface: &str, prev: Option<u8>, cache: &GsPresenceCache) {
        let Some(prev) = prev else {
            return;
        };
        if self.setter.set_channel(iface, prev).await {
            cache.record_hop_announce(prev, "revert");
            tracing::info!(interface = %iface, channel = prev, "ground_hop_follow_reverted");
        } else {
            tracing::warn!(interface = %iface, channel = prev, "ground_hop_follow_revert_failed");
        }
    }
}

/// Dispatch one inbound control frame: length-gate, verify, then either handle
/// a 51-byte HopAnnounce or record the peer (68-byte PresenceBeacon).
/// Extracted from `listen_loop` so the dispatch is unit-testable over real
/// loopback sockets. `sock` is the listener socket the HopAck echo is sent
/// from; `ack_target` is `wfb_tx_control`'s loopback ingress. A HopAnnounce is
/// acked only when `follower` can carry it out (see [`hop_follow_refusal`]);
/// otherwise no ack is sent, so the drone stays where it is, and the reason is
/// recorded on the hop snapshot.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_control_frame(
    sock: &UdpSocket,
    frame: &[u8],
    pair_key: &[u8; 32],
    cache: &GsPresenceCache,
    own_device_id: &str,
    ack_target: (std::net::Ipv4Addr, u16),
    follower: Option<&HopFollower>,
    ctx: ControlContext,
) {
    // Length gate first: a 51-byte frame is a HopAnnounce/HopAck, a 68-byte
    // frame is a PresenceBeacon. The magic + HMAC verify inside each parser is
    // the second gate, so a 51-byte non-hop frame (or a forged one) is dropped
    // rather than mis-routed.
    match frame.len() {
        HOP_FRAME_LEN => {
            let Some((channel, trigger)) = parse_hop_announce(frame, pair_key) else {
                return;
            };
            let delay = Duration::from_millis(hop_announce_delay_ms(frame).unwrap_or(0));
            let Some(follower) = follower else {
                cache.record_hop_refusal(channel, "no_receive_iface");
                return;
            };
            let iface = follower.iface().await;
            let permitted = match iface.as_deref() {
                Some(i) => ados_radio::adapter::enabled_channels(i).await,
                None => std::collections::BTreeSet::new(),
            };
            if let Some(reason) =
                hop_follow_refusal(ctx.fleet_slots, iface.is_some(), &permitted, channel)
            {
                cache.record_hop_refusal(channel, reason);
                tracing::info!(
                    channel,
                    trigger,
                    slot = ctx.slot,
                    fleet_slots = ctx.fleet_slots,
                    reason,
                    "ground_hop_refused"
                );
                return;
            }
            let Some(iface) = iface else { return };
            // The ack commits both sides, so it goes only after the checks
            // above. It is repeated for every announce of the same hop: the
            // drone keeps announcing until one ack gets through.
            if let Err(e) = sock.send_to(frame, ack_target).await {
                tracing::debug!(error = %e, "ground_hop_ack_send_failed");
            } else {
                tracing::info!(channel, trigger, "ground_hop_ack_echoed");
            }
            if !follower.claim(channel) {
                return;
            }
            // The channel being left, for the revert, read before the cache
            // moves its hint to the new one.
            let prev = cache.peer_channel();
            cache.record_hop_announce(channel, trigger);
            // Spawned so the listener's recv loop keeps feeding the presence
            // cache through the countdown and the verification window.
            let follower = follower.clone();
            let cache = cache.clone();
            tokio::spawn(async move {
                follower.follow(&iface, channel, delay, prev, &cache).await;
                follower.release(channel);
            });
        }
        PRESENCE_FRAME_LEN => {
            let Some(peer) = parse_presence_beacon(frame, pair_key) else {
                return;
            };
            // Self-pair guard: skip a beacon that carries our own device-id (the
            // emit loop's frame can loop back via wfb_rx_control's re-emit). The
            // Python listener compares against the first 16 chars (the beacon
            // device-id field is 16 bytes).
            if !own_device_id.is_empty() {
                let own_trunc: String = own_device_id.chars().take(16).collect();
                if peer.device_id == own_trunc {
                    return;
                }
            }
            cache.record_peer(
                peer.device_id,
                peer.role,
                peer.channel,
                peer.rssi_dbm,
                ctx.slot,
            );
        }
        _ => {}
    }
}
