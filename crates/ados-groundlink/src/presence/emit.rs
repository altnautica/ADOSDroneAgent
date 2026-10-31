use super::*;

/// Emit a PresenceBeacon to `wfb_tx_control`'s loopback ingress every
/// `PRESENCE_CADENCE`. `channel` is read fresh each tick through the supplied
/// closure so a channel change between ticks is reflected. Returns only on a
/// fatal socket-bind error or task cancellation.
pub async fn emit_loop<F>(channel_fn: F) -> std::io::Result<()>
where
    F: Fn() -> u8 + Send,
{
    let device_id = ados_protocol::identity::device_id(None);
    if device_id.is_empty() {
        tracing::warn!("ground_presence_emit_no_device_id");
    }
    // Bind an ephemeral source port; we only ever send.
    let sock = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).await?;
    let target = (std::net::Ipv4Addr::LOCALHOST, PRESENCE_EMIT_PORT);
    tracing::info!(device_id = %device_id, cadence_s = 10, "ground_presence_emit_started");

    loop {
        let (pair_key, _) = resolve_pair_key();
        // With no readable key there is nothing to sign a beacon with that a
        // peer should trust; stay silent until the key reads cleanly.
        if let Some(pair_key) = pair_key {
            let beacon = build_presence_beacon(
                &device_id,
                // GS role (role byte 0x02). `role_drone = false`.
                false,
                channel_fn(),
                0, // rssi unknown on the emit side
                now_unix_ms(),
                &pair_key,
            );
            if let Err(e) = sock.send_to(&beacon, target).await {
                tracing::debug!(error = %e, "presence_emit_send_failed");
            }
        }
        tokio::time::sleep(PRESENCE_CADENCE).await;
    }
}

/// Hop-supervisor snapshot persist cadence (5 s, matching the Python listener:
/// the GCS chart polls at 1 Hz but does not need sub-second hop-history
/// freshness).
pub const HOP_PERSIST_CADENCE: Duration = Duration::from_secs(5);

/// Build the hop-supervisor JSON payload from a snapshot, stamping
/// `wall_time_unix` so a cross-process reader can age the file. Pure so the
/// shape is unit-testable without the filesystem: the snapshot fields plus
/// `wall_time_unix`.
pub fn hop_supervisor_payload(snap: &HopSnapshot) -> serde_json::Value {
    let mut v = serde_json::to_value(snap).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert("wall_time_unix".to_string(), serde_json::json!(now_unix()));
        // Sidecar schema version (best-effort drift signal for readers). Shared
        // with the drone-side hop supervisor via the one const.
        obj.insert(
            "version".to_string(),
            serde_json::json!(ados_radio::paths::HOP_SUPERVISOR_SIDECAR_VERSION),
        );
    }
    v
}

/// Persist the GS-side hop-supervisor snapshot to `/run/ados/hop-supervisor.json`
/// on the `HOP_PERSIST_CADENCE`, sourcing the hop-follow history from the shared
/// presence cache. Writes one immediate snapshot on entry (so the on-box
/// channel-hops page reads a valid file before the first beacon) and one every
/// cadence tick thereafter. The drone supervisor and the GS listener both target
/// this single file; a given rig runs only one of them so there is no
/// contention. Best-effort: an I/O error is logged and the loop continues.
/// Returns only on task cancellation.
pub async fn hop_supervisor_persist_loop(cache: GsPresenceCache, band: String) {
    use std::path::Path;
    let path = Path::new(crate::paths::HOP_SUPERVISOR_JSON);
    loop {
        let snap = cache.hop_snapshot(&band);
        let payload = hop_supervisor_payload(&snap);
        if let Err(e) = ados_protocol::sidecar::write_json_atomic(path, &payload, 0o644) {
            tracing::debug!(error = %e, "ground_hop_supervisor_persist_failed");
        }
        tokio::time::sleep(HOP_PERSIST_CADENCE).await;
    }
}

/// Build the `linked-peers.json` payload from the fresh peer set, stamping
/// `version` (a reader drift signal) + `wall_time_unix` (so a cross-process
/// reader can age the whole file, not just the per-peer `last_seen_unix`). Pure
/// so the shape is unit-testable without the filesystem.
pub fn linked_peers_payload(peers: &[LinkedPeer]) -> serde_json::Value {
    serde_json::json!({
        "version": LINKED_PEERS_SIDECAR_VERSION,
        "wall_time_unix": now_unix(),
        "peers": peers,
    })
}

/// Persist the linked-peers list to `/run/ados/linked-peers.json` on the
/// [`LINKED_PEERS_PERSIST_CADENCE`], pruning stale peers each tick (via
/// [`GsPresenceCache::linked_peers`]). Writes one immediate snapshot on entry so
/// the heartbeat reads a valid (empty) file before the first beacon, then one
/// every cadence tick. Best-effort: an I/O error is logged and the loop
/// continues. Returns only on task cancellation. Only the direct receive plane
/// runs the presence listener, so only it writes this file — no contention.
pub async fn linked_peers_persist_loop(cache: GsPresenceCache) {
    use std::path::PathBuf;
    let owned = PathBuf::from(crate::paths::run_path("linked-peers.json"));
    loop {
        let peers = cache.linked_peers();
        let payload = linked_peers_payload(&peers);
        if let Err(e) = ados_protocol::sidecar::write_json_atomic(&owned, &payload, 0o644) {
            tracing::debug!(error = %e, "ground_linked_peers_persist_failed");
        }
        tokio::time::sleep(LINKED_PEERS_PERSIST_CADENCE).await;
    }
}
