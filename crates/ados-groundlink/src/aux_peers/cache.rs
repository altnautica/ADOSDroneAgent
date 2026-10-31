use super::*;

impl PeerRecord {
    /// The newest arrival time of any frame, for the whole-peer prune.
    fn last_seen(&self) -> f64 {
        self.status_at.max(self.identity_at)
    }
}

impl AuxPeerCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Note the flight-controller system id seen on `slot`.
    ///
    /// Returns the set of OTHER slots currently presenting the same id, so the
    /// caller can report a collision the first time it becomes observable. It
    /// returns rather than logs because the decision of how loudly to report
    /// belongs to the consumer, which knows whether this is the first sighting.
    pub fn observe_system_id(&self, slot: u8, system_id: u8) -> Vec<u8> {
        self.observe_system_id_at(slot, system_id, now_unix())
    }

    /// [`Self::observe_system_id`] with an injected clock, for tests.
    pub fn observe_system_id_at(&self, slot: u8, system_id: u8, now: f64) -> Vec<u8> {
        let mut map = self.system_ids.lock().unwrap_or_else(|e| e.into_inner());
        let previous = map.insert(slot, (system_id, now));
        // Nothing to say when this slot has not changed what it presents and
        // was not stale; the collision, if any, was already reported.
        if let Some((id, seen)) = previous {
            if id == system_id && now - seen <= SYSTEM_ID_STALE_AFTER_S {
                return Vec::new();
            }
        }
        map.iter()
            .filter(|(other_slot, (other_id, seen))| {
                **other_slot != slot
                    && *other_id == system_id
                    && now - *seen <= SYSTEM_ID_STALE_AFTER_S
            })
            .map(|(other_slot, _)| *other_slot)
            .collect()
    }

    /// Every slot's currently-observed flight-controller system id, in slot
    /// order, fresh entries only.
    pub fn system_ids_by_slot(&self) -> Vec<(u8, u8)> {
        self.system_ids_by_slot_at(now_unix())
    }

    fn system_ids_by_slot_at(&self, now: f64) -> Vec<(u8, u8)> {
        self.system_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, (_, seen))| now - *seen <= SYSTEM_ID_STALE_AFTER_S)
            .map(|(s, (i, _))| (*s, *i))
            .collect()
    }

    /// System ids that two or more fresh slots present at once, ascending.
    pub fn conflicting_system_ids(&self, now: f64) -> Vec<u8> {
        let mut counts: BTreeMap<u8, usize> = BTreeMap::new();
        for (_, id) in self.system_ids_by_slot_at(now) {
            *counts.entry(id).or_default() += 1;
        }
        counts
            .into_iter()
            .filter(|(_, n)| *n > 1)
            .map(|(id, _)| id)
            .collect()
    }

    /// The `fleet_system_ids` sidecar block: one row per slot with a fresh
    /// flight-controller system id.
    fn fleet_system_ids_payload(&self, now: f64) -> Vec<Value> {
        self.system_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, (_, seen))| now - *seen <= SYSTEM_ID_STALE_AFTER_S)
            .map(|(slot, (id, seen))| {
                json!({"slot": slot, "fc_system_id": id, "seen_at_unix": seen})
            })
            .collect()
    }

    /// Record a decoded status frame.
    ///
    /// Returns `false` when the frame was dropped for arriving out of order, so
    /// the caller can count it rather than silently discarding it.
    pub fn record_status(&self, status: NodeStatus) -> bool {
        self.record_status_at(status, now_unix())
    }

    /// [`Self::record_status`] with an injected clock, for tests.
    pub fn record_status_at(&self, status: NodeStatus, now: f64) -> bool {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(status.id.clone()).or_default();

        // Reject a frame older than the one held, UNLESS what is held has aged
        // past the stale window: a producer that restarted also restarted its
        // sequence, and refusing its frames forever would leave the peer
        // permanently blank.
        if let Some(held) = &entry.status {
            let held_is_fresh = now - entry.status_at <= STATUS_STALE_AFTER_S;
            if held_is_fresh && !seq_is_newer(status.sq, held.sq) {
                entry.out_of_order += 1;
                return false;
            }
        }
        entry.status = Some(status);
        entry.status_at = now;
        entry.status_frames += 1;
        Self::prune_locked(&mut map, now);
        true
    }

    /// Record a decoded identity frame.
    pub fn record_identity(&self, identity: NodeIdentity) {
        self.record_identity_at(identity, now_unix());
    }

    /// [`Self::record_identity`] with an injected clock, for tests.
    pub fn record_identity_at(&self, identity: NodeIdentity, now: f64) {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(identity.id.clone()).or_default();
        entry.identity = Some(identity);
        entry.identity_at = now;
        Self::prune_locked(&mut map, now);
    }

    /// Drop peers with no frame inside the window, then enforce the cap by
    /// evicting the stalest. Called under the lock after every insert.
    fn prune_locked(map: &mut BTreeMap<String, PeerRecord>, now: f64) {
        map.retain(|_, r| now - r.last_seen() <= PEER_STALE_AFTER_S);
        while map.len() > MAX_PEERS {
            // Evict the least-recently-heard peer. A live peer is never the
            // stalest, so the cap sheds noise before it sheds signal.
            let Some(oldest) = map
                .iter()
                .min_by(|a, b| {
                    a.1.last_seen()
                        .partial_cmp(&b.1.last_seen())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            map.remove(&oldest);
        }
    }

    /// The device ids of peers whose identity is still fresh, with their names.
    ///
    /// This is what lets the node answer "who is my peer" from the lane, and it
    /// deliberately keys on the IDENTITY frame's freshness: an identity is only
    /// worth reporting if the peer that claimed it is still speaking.
    pub fn fresh_identities(&self, now: f64) -> Vec<(String, Option<String>)> {
        let map = self.inner.lock().unwrap();
        map.values()
            .filter(|r| now - r.identity_at <= PEER_STALE_AFTER_S)
            .filter_map(|r| r.identity.as_ref())
            .map(|i| (i.id.clone(), i.nm.clone()))
            .collect()
    }

    /// Build the published peer list.
    ///
    /// Each entry carries its own ages and an explicit `status_fresh` flag. A
    /// peer whose status has gone stale keeps its identity (which does not
    /// expire the same way) but its `status` block is OMITTED rather than served
    /// as a current reading.
    pub fn peers_payload(&self, now: f64) -> Vec<Value> {
        let map = self.inner.lock().unwrap();
        map.values()
            .filter(|r| now - r.last_seen() <= PEER_STALE_AFTER_S)
            .map(|r| {
                let status_fresh = r.status.is_some() && now - r.status_at <= STATUS_STALE_AFTER_S;
                let mut entry = json!({
                    "status_fresh": status_fresh,
                    "status_frames": r.status_frames,
                    "out_of_order": r.out_of_order,
                });
                let obj = entry.as_object_mut().expect("built as an object");

                if let Some(i) = &r.identity {
                    obj.insert("device_id".into(), json!(i.id));
                    obj.insert("name".into(), json!(i.nm));
                    obj.insert("profile".into(), json!(i.pr));
                    obj.insert("agent_version".into(), json!(i.ver));
                    obj.insert("identity_at_unix".into(), json!(r.identity_at));
                    obj.insert("identity_age_s".into(), json!(round2(now - r.identity_at)));
                }
                if let Some(s) = &r.status {
                    // The device id is on the status frame too, so a peer that
                    // has sent status but no identity yet is still identified.
                    obj.entry("device_id").or_insert(json!(s.id));
                    obj.insert("status_at_unix".into(), json!(r.status_at));
                    obj.insert("status_age_s".into(), json!(round2(now - r.status_at)));
                    obj.insert("seq".into(), json!(s.sq));
                    if status_fresh {
                        obj.insert("status".into(), expand_status(s));
                    }
                }
                entry
            })
            .collect()
    }

    /// The full sidecar document.
    ///
    /// `fleet_system_ids` and `conflicting_system_ids` are what the fleet
    /// roster and the ground station's uplink read: a system id two linked
    /// aircraft share cannot be commanded safely, so the uplink holds every
    /// frame addressed to it while the conflict lasts.
    pub fn sidecar_payload(&self, now: f64) -> Value {
        json!({
            "version": AUX_PEERS_SIDECAR_VERSION,
            "wall_time_unix": now,
            "status_stale_after_s": STATUS_STALE_AFTER_S,
            "peers": self.peers_payload(now),
            "fleet_system_ids": self.fleet_system_ids_payload(now),
            "conflicting_system_ids": self.conflicting_system_ids(now),
        })
    }
}
