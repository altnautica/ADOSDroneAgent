use super::*;

/// Listen for inbound control frames on every registered slot's control port,
/// verify the HMAC, and update `cache`. Two frame classes share each port
/// (`wfb_rx_control` re-emits both): a 51-byte HopAnnounce and a 68-byte
/// PresenceBeacon. The listener length-gates first, then dispatches:
///
/// * **HopAnnounce (51 B):** verify the HMAC, then echo the verbatim frame back
///   as a HopAck to `wfb_tx_control`'s loopback ingress so the drone's "acked"
///   gate goes true and its epoch-synced channel flip fires (without the echo
///   the drone never coordinates a hop). The announce's channel + real trigger
///   are recorded as a channel-follow, leaving the beacon-driven presence
///   identity/liveness untouched.
/// * **PresenceBeacon (68 B):** verify the HMAC, drop a frame carrying our own
///   device-id (the self-pair guard), then record the peer.
///
/// One socket per slot, one reader task each, all under this single generation:
/// the cache, the self-pair guard and the restart accounting stay singular
/// while the receive plane fans out per drone. Every bind happens before any
/// reader starts, so a port already held (a leaked previous generation) is a
/// fatal error the supervisor backs off and retries rather than a half-bound
/// listener that silently hears only some of the fleet.
///
/// Returns only on a fatal bind error, or when any slot's reader ends.
pub async fn listen_loop(
    cache: GsPresenceCache,
    follower: Option<HopFollower>,
    slots: &[u8],
) -> std::io::Result<()> {
    let own_device_id = ados_protocol::identity::device_id(None);
    let mut socks = Vec::with_capacity(slots.len());
    for &slot in slots {
        let port = presence_listen_port(slot);
        socks.push((
            slot,
            UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).await?,
        ));
    }
    if socks.is_empty() {
        return Err(std::io::Error::other(
            "presence listener asked to bind zero fleet slots",
        ));
    }
    tracing::info!(
        slots = ?slots,
        "ground_presence_listen_started"
    );

    let mut readers = tokio::task::JoinSet::new();
    let mut bound: std::collections::BTreeSet<u8> = std::collections::BTreeSet::new();
    for (slot, sock) in socks {
        bound.insert(slot);
        readers.spawn(listen_on_slot(
            sock,
            slot,
            cache.clone(),
            follower.clone(),
            own_device_id.clone(),
        ));
    }

    // Pick up slots issued after this generation started.
    //
    // The set above comes from one registry read, and a drone can be paired long
    // after the listener is running: the pair route deliberately skips
    // re-installing the receive unit when the fleet key is unchanged, which is
    // the normal case for the second and every subsequent drone. The receive
    // plane already reconciles its own processes, so without this the new
    // drone's control frames are decoded onto a port nobody is bound to. Its
    // HopAnnounce is never echoed, so its coordinated channel hop never fires;
    // it never reaches the valid-packet watchdog's presence input, so a healthy
    // link can be cold-swept; and it never appears in the linked-peers surface,
    // so a GCS paired to this node never enrols it. All silently.
    let mut tick = tokio::time::interval(FLEET_RECONCILE_INTERVAL);
    tick.tick().await; // the first tick completes immediately

    loop {
        tokio::select! {
            // A reader only ends on a fault, so the first completion ends the
            // whole generation; the remaining readers are aborted when the
            // JoinSet drops, and the supervisor re-binds every slot afresh.
            _ = readers.join_next() => {
                return Err(std::io::Error::other(
                    "a presence slot reader ended; re-binding the whole listener",
                ));
            }
            _ = tick.tick() => {
                let want = wfb_rx::fleet_slots(&FleetRegistry::load(std::path::Path::new(
                    FLEET_REGISTRY_PATH,
                )));
                let missing: Vec<u8> =
                    want.into_iter().filter(|s| !bound.contains(s)).collect();
                for slot in missing {
                    let port = presence_listen_port(slot);
                    // A late bind is NOT fatal. The up-front binds are fatal on
                    // purpose — a port already held means a leaked generation,
                    // and half a fleet heard is worse than a clean retry. That
                    // reasoning does not carry here: propagating an EADDRINUSE
                    // from one newly-paired slot would tear down every healthy
                    // reader for a slot that was working seconds ago. Retry it
                    // on the next tick instead and leave the rest alone.
                    match UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
                        Ok(sock) => {
                            bound.insert(slot);
                            readers.spawn(listen_on_slot(
                                sock,
                                slot,
                                cache.clone(),
                                follower.clone(),
                                own_device_id.clone(),
                            ));
                            tracing::info!(slot, port, "ground_presence_slot_bound");
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e, slot, port,
                                "ground_presence_slot_bind_failed; retrying next tick"
                            );
                        }
                    }
                }
            }
        }
    }
}

/// One slot's receive loop. Never returns under normal operation.
pub(super) async fn listen_on_slot(
    sock: UdpSocket,
    slot: u8,
    cache: GsPresenceCache,
    follower: Option<HopFollower>,
    own_device_id: String,
) {
    let ack_target = (std::net::Ipv4Addr::LOCALHOST, HOP_ACK_ECHO_PORT);
    let mut buf = [0u8; 256];
    loop {
        // A recv error must NOT end the listener: this loop is the sole writer of
        // the watchdog's peer-presence cache, so if it died the cache would
        // freeze, presence would age out, and the valid-packet watchdog would
        // fall through to a cold-sweep/teardown on a paired-but-idle link. Log a
        // transient error and read again, keeping the cache fed.
        let (len, _addr) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, slot, "ground_presence_recv_failed");
                continue;
            }
        };
        let (pair_key, status) = resolve_pair_key();
        cache.set_key_status(status);
        let Some(pair_key) = pair_key else {
            // The key file exists but cannot be read: nothing on the control
            // plane can be authenticated, so nothing is acted on.
            continue;
        };
        // Only a hop frame needs the fleet size, and it is read fresh: a drone
        // paired a moment ago turns a single-drone station into a fleet.
        let fleet_slots = if len == HOP_FRAME_LEN {
            wfb_rx::fleet_slots(&FleetRegistry::load(std::path::Path::new(
                FLEET_REGISTRY_PATH,
            )))
            .len()
        } else {
            0
        };
        handle_control_frame(
            &sock,
            &buf[..len],
            &pair_key,
            &cache,
            &own_device_id,
            ack_target,
            follower.as_ref(),
            ControlContext { slot, fleet_slots },
        )
        .await;
    }
}

/// Fixed interval between listener re-binds after a fatal socket error or a
/// panic in the listen path.
///
/// Flat, with no ceiling and no attempt cap: the ladder this replaces doubled to
/// 30 s, and the presence input is what tells a ground station where the drone
/// actually is — a re-bind that is minutes late is a rendezvous the operator has
/// to drive by hand. The interval floor is what keeps a hard-failing bind from
/// busy-spinning.
pub(super) const LISTEN_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Sidecar carrying the listener-supervisor's restart accounting so a flapping
/// presence listener is observable cross-process (the REST/heartbeat layer reads
/// the GS sidecars). Lives on tmpfs under the run dir, honouring the same
/// `ADOS_RUN_DIR` override the rest of the Contract-E sidecars use.
pub(super) const PRESENCE_LISTENER_SIDECAR_NAME: &str = "ground-presence-listener.json";

/// Persist the listener-health snapshot to the GS sidecar. Best-effort: an I/O
/// error is logged and discarded so sidecar trouble never stalls the supervisor.
/// The run-dir path resolves via `run_path` (honouring the `ADOS_RUN_DIR`
/// override); the write is delegated to `write_listener_health_to` so tests can
/// target an explicit temp path without mutating process-global env.
pub(super) fn write_listener_health(health: &ListenerHealth) {
    write_listener_health_to(
        std::path::Path::new(&crate::paths::run_path(PRESENCE_LISTENER_SIDECAR_NAME)),
        health,
    );
}

/// Persist the listener-health snapshot to an explicit sidecar path. The path
/// seam that lets a test write into its own temp dir without touching
/// `ADOS_RUN_DIR`. Best-effort with the same swallow-and-log contract as
/// [`write_listener_health`].
pub(super) fn write_listener_health_to(path: &std::path::Path, health: &ListenerHealth) {
    if let Err(e) = ados_protocol::sidecar::write_json_atomic(path, health, 0o644) {
        tracing::debug!(error = %e, "ground_presence_listener_sidecar_failed");
    }
}

/// Supervise the presence listener for the whole service lifetime, binding one
/// control port per registered fleet `slot`.
///
/// `listen_loop` only returns on a fatal socket error (its per-slot recv loops
/// continue over transient errors), so a return is a genuine fault: the
/// supervisor re-binds every slot after a fixed `LISTEN_RETRY_INTERVAL`. The
/// listener handle is awaited rather than dropped, so a panic in the listen path
/// surfaces as a `JoinError` here (logged + counted) instead of being silently
/// swallowed. The restart accounting is published to a GS sidecar so a flapping
/// listener is visible to the REST/heartbeat layer — one sidecar for the whole
/// fleet, because there is one listener generation covering every slot.
///
/// This loop itself never returns; spawn it for the service lifetime.
pub async fn listen_supervisor(
    cache: GsPresenceCache,
    follower: Option<HopFollower>,
    slots: Vec<u8>,
) {
    // No `backoff` state: every re-bind waits exactly LISTEN_RETRY_INTERVAL.
    let mut health = ListenerHealth {
        starts: 0,
        restarts: 0,
        panics: 0,
        last_exit: "starting".to_string(),
        started_at_unix: now_unix(),
    };

    loop {
        health.starts = health.starts.saturating_add(1);
        health.started_at_unix = now_unix();
        write_listener_health(&health);
        tracing::info!(
            starts = health.starts,
            restarts = health.restarts,
            "ground_presence_listener_spawning"
        );

        let handle = {
            let cache = cache.clone();
            let follower = follower.clone();
            let slots = slots.clone();
            tokio::spawn(async move { listen_loop(cache, follower, &slots).await })
        };

        // Await the generation so a panic is observed (not dropped). A clean
        // return is a fatal bind error path; a JoinError is a panic.
        match handle.await {
            Ok(Ok(())) => {
                // listen_loop's recv path never returns Ok today, but treat a
                // future clean shutdown as a re-spawn trigger rather than leaving
                // the cache without a writer.
                health.last_exit = "loop_returned_ok".to_string();
                tracing::warn!("ground_presence_listener_returned");
            }
            Ok(Err(e)) => {
                health.last_exit = format!("bind_error: {e}");
                tracing::error!(error = %e, "ground_presence_listener_bind_failed");
            }
            Err(join_err) => {
                health.panics = health.panics.saturating_add(1);
                health.last_exit = format!("panic: {join_err}");
                tracing::error!(error = %join_err, "ground_presence_listener_panicked");
            }
        }

        health.restarts = health.restarts.saturating_add(1);
        write_listener_health(&health);

        tracing::warn!(
            retry_in_s = LISTEN_RETRY_INTERVAL.as_secs(),
            restarts = health.restarts,
            "ground_presence_listener_retrying"
        );
        tokio::time::sleep(LISTEN_RETRY_INTERVAL).await;
    }
}
