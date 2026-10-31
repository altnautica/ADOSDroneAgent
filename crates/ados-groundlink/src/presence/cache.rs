use super::*;

impl GsPresenceCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared peer state, recovering it if a holder panicked: every field
    /// is a plain overwrite, so a poisoned guard still holds consistent data.
    fn state(&self) -> std::sync::MutexGuard<'_, PeerState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Record a verified inbound beacon (writer side, from the listener).
    ///
    /// When the announced channel differs from where the receiver last followed
    /// the peer, a channel-follow entry is appended to the hop history ring (the
    /// GS-side equivalent of an actuated hop: the receiver tracks the channel the
    /// transmitter advertises). The ring is trimmed to the last `HOP_HISTORY_CAP`
    /// entries, matching the Python listener. A PresenceBeacon carries no hop
    /// trigger, so its follow entries are labelled "periodic".
    pub(super) fn record_peer(
        &self,
        device_id: String,
        role: String,
        channel: u8,
        rssi_dbm: i8,
        slot: u8,
    ) {
        let mut s = self.state();
        let prev_channel = s.peer_channel;
        let now = now_unix();
        // The multi-peer map: upsert this peer keyed by its device-id so a ground
        // station relaying several drones reports all of them. Keyed insert also
        // means a re-heard peer refreshes (not duplicates) its entry.
        s.peers.insert(
            device_id.clone(),
            LinkedPeer {
                device_id: device_id.clone(),
                role: role.clone(),
                channel,
                rssi_dbm,
                last_seen_unix: now,
                slot,
                // Beacons carry no name; the auxiliary lane supplies it at read
                // time, so this stays absent rather than being invented here.
                name: None,
            },
        );
        // The scalar fields track the FRESHEST peer (this beacon), the watchdog's
        // presence + channel-follow signal — unchanged single-peer behaviour.
        s.peer_device_id = Some(device_id);
        s.peer_role = Some(role);
        s.peer_channel = Some(channel);
        s.peer_rssi_dbm = Some(rssi_dbm);
        s.peer_last_seen_unix = Some(now);
        if prev_channel != Some(channel) {
            Self::push_follow(&mut s, prev_channel, channel, "periodic");
        }
    }

    /// Replace the set of identities learned from the auxiliary lane.
    ///
    /// Called with the current fresh set, so an identity whose peer stopped
    /// speaking disappears rather than lingering as a confident label.
    pub fn set_aux_identities(&self, identities: Vec<(String, Option<String>)>) {
        let mut s = self.state();
        s.aux_identities = identities.into_iter().collect();
    }

    /// The fresh set of decoded WFB peers (last beacon within
    /// [`LINKED_PEER_STALE_AFTER_S`]), newest-first. Prunes stale entries from
    /// the map as a side effect so a peer that stops beaconing is dropped, never
    /// republished as a stale confident entry. Empty until a peer is heard.
    pub fn linked_peers(&self) -> Vec<LinkedPeer> {
        let mut s = self.state();
        let now = now_unix();
        s.peers
            .retain(|_, p| (now - p.last_seen_unix) <= LINKED_PEER_STALE_AFTER_S);
        let aux = s.aux_identities.clone();
        let mut out: Vec<LinkedPeer> = s.peers.values().cloned().collect();

        // Enrich each beacon-derived row with what the auxiliary lane learned.
        //
        // A beacon carries a device id, but a node with no persistent id emits an
        // empty one, and a row with an empty id is dropped by every downstream
        // reader — so the peer vanishes entirely despite being audibly present.
        // The identity frame is the only place the real id can come from in that
        // case, so it is adopted here.
        //
        // Only when there is exactly ONE identity to adopt. With several peers
        // and no id on the beacon there is no way to tell which is which, and a
        // guess would attach one node's identity to another node's signal.
        let sole_aux_id = (aux.len() == 1).then(|| aux.iter().next().expect("len is 1"));
        for peer in out.iter_mut() {
            if peer.device_id.is_empty() {
                if let Some((id, name)) = sole_aux_id {
                    peer.device_id = id.clone();
                    peer.name = name.clone();
                }
                continue;
            }
            if let Some(name) = aux.get(&peer.device_id) {
                peer.name = name.clone();
            }
        }

        // Freshest peer first, so a single-drone reader that takes `[0]` gets the
        // most-recent decode.
        out.sort_by(|a, b| {
            b.last_seen_unix
                .partial_cmp(&a.last_seen_unix)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    /// Record a verified HopAnnounce (writer side, from the listener).
    ///
    /// A HopAnnounce is the drone telling the receiver which channel to follow
    /// to and why (its `trigger`). Unlike a PresenceBeacon it carries no device
    /// identity, RSSI, or liveness signal, so it must NOT reset
    /// `peer_last_seen_unix` (the watchdog's presence-age gate is beacon-driven)
    /// and must NOT clobber the peer identity learned from a prior beacon. It
    /// only updates the channel to follow and appends a follow entry with the
    /// announce's real trigger. This is the back-fill: existing identity is
    /// preserved, the channel-follow is recorded with the correct trigger label.
    pub(super) fn record_hop_announce(&self, channel: u8, trigger: &str) {
        let mut s = self.state();
        let prev_channel = s.peer_channel;
        if prev_channel != Some(channel) {
            s.peer_channel = Some(channel);
            Self::push_follow(&mut s, prev_channel, channel, trigger);
        }
    }

    /// Record an announce this station refused to follow, for the snapshot.
    pub(super) fn record_hop_refusal(&self, channel: u8, reason: &'static str) {
        let mut s = self.state();
        s.last_refusal = Some(HopRefusal {
            at: now_unix(),
            channel,
            reason,
        });
    }

    /// Record which key the control plane is authenticating under.
    pub(super) fn set_key_status(&self, status: &'static str) {
        self.state().key_status = Some(status);
    }

    /// Append a channel-follow entry to the bounded hop-history ring (shared by
    /// the beacon and HopAnnounce writers). `from` is the prior channel (0 when
    /// unknown, matching the Python listener) and `trigger` is the follow label.
    fn push_follow(s: &mut PeerState, prev_channel: Option<u8>, channel: u8, trigger: &str) {
        let now = now_unix();
        s.hop_history.push(HopFollowEntry {
            at: now,
            // The first beacon has no prior channel; record 0 (the Python
            // listener uses 0 for an unknown `from`).
            from: prev_channel.unwrap_or(0),
            to: channel,
            trigger: trigger.to_string(),
            ok: true,
        });
        if s.hop_history.len() > HOP_HISTORY_CAP {
            let trim = s.hop_history.len() - HOP_HISTORY_CAP;
            s.hop_history.drain(0..trim);
        }
        s.last_hop_at = now;
    }

    /// The hop-supervisor snapshot in the Python `HopListener.snapshot()` shape.
    /// `band` is the configured radio band the receive plane is sweeping; the
    /// drone-only thresholds are `null` on the listener side and `source` is
    /// `"listener"`.
    pub fn hop_snapshot(&self, band: &str) -> HopSnapshot {
        let s = self.state();
        HopSnapshot {
            enabled: true,
            band: band.to_string(),
            hop_period_seconds: None,
            loss_threshold_percent: None,
            rssi_threshold_dbm: None,
            last_hop_at: s.last_hop_at,
            history: s.hop_history.clone(),
            source: "listener",
            last_refusal: s.last_refusal.clone(),
            key_status: s.key_status,
        }
    }

    /// The peer's last announced channel (the watchdog's beacon-guided hint).
    pub fn peer_channel(&self) -> Option<u8> {
        self.state().peer_channel
    }

    /// Wall-clock unix of the last verified beacon (None until one is seen).
    pub fn peer_last_seen_unix(&self) -> Option<f64> {
        self.state().peer_last_seen_unix
    }
}

impl PresenceCache for GsPresenceCache {
    /// Seconds since the last verified beacon, or `None` when none seen. Clamped
    /// at zero so a wall-clock step backwards never yields a negative age.
    fn presence_age_s(&self) -> Option<f64> {
        let last = self.state().peer_last_seen_unix?;
        if last <= 0.0 {
            return None;
        }
        Some((now_unix() - last).max(0.0))
    }

    fn announced_channel(&self) -> Option<u8> {
        self.peer_channel()
    }
}
