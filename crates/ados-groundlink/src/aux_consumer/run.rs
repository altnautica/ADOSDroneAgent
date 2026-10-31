use super::*;

/// Receive-buffer size asked of the kernel for an aux consumer socket.
///
/// The drone end of this same lane already asks for this; the ground end did
/// not, so the two ends of one lane disagreed about how much burst they could
/// absorb. A batched datagram or a response-fragment burst arriving while the
/// single read loop is mid-dispatch survives only if the kernel holds it, and a
/// drop there happens below the application: `recv_errors` never sees it, so
/// the loss is invisible on both sides.
pub(super) const AUX_RECV_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Ask the kernel for a larger receive buffer, and log what it actually gave.
///
/// The kernel clamps the request to `rmem_max` and reports back double what it
/// kept (its own bookkeeping overhead), so the obtained value is logged rather
/// than assumed. Failure is non-fatal: the default buffer still works, it just
/// tolerates a smaller burst.
pub(super) fn set_recv_buffer(sock: &UdpSocket) {
    let opts = socket2::SockRef::from(sock);
    if let Err(e) = opts.set_recv_buffer_size(AUX_RECV_BUFFER_BYTES) {
        tracing::warn!(
            error = %e,
            requested = AUX_RECV_BUFFER_BYTES,
            "ground_aux_consumer_rcvbuf_set_failed"
        );
        return;
    }
    match opts.recv_buffer_size() {
        Ok(actual) => tracing::info!(
            requested = AUX_RECV_BUFFER_BYTES,
            actual,
            "ground_aux_consumer_rcvbuf"
        ),
        Err(e) => tracing::warn!(error = %e, "ground_aux_consumer_rcvbuf_read_failed"),
    }
}

/// Read the lane's loopback port until cancelled or the socket fails fatally.
///
/// Returns `Err` only when the port could not be bound; the caller supervises
/// the retry. `sinks` carries the destination for each channel, so a test can
/// point the consumer at a stand-in seam; a sink is `None` on a rig where that
/// consumer is not running, and a datagram for it is counted and dropped rather
/// than silently swallowed.
pub async fn run_aux_consumer(
    slot: u8,
    listen_port: u16,
    sinks: AuxSinksOwned,
    counters: AuxCounters,
    peers: AuxPeerCache,
    cancel: Shutdown,
) -> std::io::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, listen_port));
    let sock = UdpSocket::bind(addr).await?;
    set_recv_buffer(&sock);
    tracing::info!(slot, port = listen_port, "ground_aux_consumer_listening");

    // Borrowed once, not per datagram: the set is fixed for the life of a
    // generation and this is the hot receive path.
    let borrowed = sinks.borrow();

    let mut gate = SeamGate::default();
    let mut buf = vec![0u8; BUF_SIZE];
    let mut last_report = AuxCountersSnapshot::default();
    let mut report_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + COUNTER_REPORT_INTERVAL,
        COUNTER_REPORT_INTERVAL,
    );
    report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Consecutive read errors with no intervening success. A datagram sent to a
    // closed peer can surface as an error on the next read; that is transient
    // and self-clears. A run of them with no successes means the socket cannot
    // be read at all, so a short sleep keeps a hard failure from spinning the
    // CPU while the caller decides what to do.
    let mut consecutive_errors: u32 = 0;

    loop {
        tokio::select! {
            biased;
            _ = cancel.wait() => break,
            _ = report_tick.tick() => {
                last_report = report(&counters, last_report);
            }
            received = sock.recv_from(&mut buf) => match received {
                Ok((n, _from)) => {
                    consecutive_errors = 0;
                    dispatch(slot, &buf[..n], &borrowed, &counters, &mut gate, &peers).await;
                }
                Err(e) => {
                    counters.bump(&counters.0.recv_errors);
                    consecutive_errors += 1;
                    tracing::debug!(error = %e, "ground_aux_recv_error");
                    if consecutive_errors >= 5 {
                        tracing::warn!(
                            error = %e,
                            consecutive = consecutive_errors,
                            "ground_aux_recv_failing"
                        );
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            },
        }
    }

    report(&counters, last_report);
    tracing::info!("ground_aux_consumer_stopped");
    Ok(())
}

/// Supervise the consumer for the service lifetime, re-binding on a fixed
/// interval when the port cannot be taken.
///
/// The port is bound once for the whole service rather than per receive
/// generation: the generations respawn on link loss, and re-binding a UDP port
/// on that cadence risks losing the bind to its own lingering socket. A bind
/// failure is counted, so a consumer that never started listening is visible on
/// the sidecar rather than merely absent from it.
pub async fn supervise_aux_consumer(
    slot: u8,
    listen_port: u16,
    sinks: AuxSinksOwned,
    counters: AuxCounters,
    peers: AuxPeerCache,
    cancel: Shutdown,
) {
    // No `backoff` state: every re-bind waits exactly BIND_RETRY_INTERVAL.
    loop {
        match run_aux_consumer(
            slot,
            listen_port,
            sinks.clone(),
            counters.clone(),
            peers.clone(),
            cancel.clone(),
        )
        .await
        {
            // A clean return means cancellation; nothing left to supervise.
            Ok(()) => return,
            Err(e) => {
                counters.bump(&counters.0.bind_failures);
                tracing::warn!(
                    port = listen_port,
                    error = %e,
                    retry_s = BIND_RETRY_INTERVAL.as_secs(),
                    "ground_aux_consumer_bind_failed"
                );
            }
        }
        tokio::select! {
            _ = cancel.wait() => return,
            _ = tokio::time::sleep(BIND_RETRY_INTERVAL) => {}
        }
    }
}
