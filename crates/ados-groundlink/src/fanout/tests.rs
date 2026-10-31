use super::*;
use crate::fleet::FleetRegistry;
use std::time::Duration;

/// A registry file holding `ids` on slots 1..=n, and the temp dir that owns
/// it. `allocate` issues the lowest free slot first, so `ids[0]` lands on
/// slot 1.
fn registry_with(dir: &Path, ids: &[&str]) -> PathBuf {
    let path = dir.join("fleet.json");
    let mut reg = FleetRegistry::default();
    for id in ids {
        reg.allocate(id).expect("a slot must be issuable");
    }
    reg.persist(&path).unwrap();
    path
}

/// Reserve an ephemeral UDP port and give it straight back, so the caller
/// knows a free address without holding it.
async fn free_port() -> u16 {
    let s = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = s.local_addr().unwrap().port();
    drop(s);
    port
}

/// Drive `payload` at `port` on a tight cadence until dropped. UDP gives no
/// delivery guarantee and the fan-out's bind races the first send, so every
/// test that asserts on delivery resends.
async fn drive(port: u16, payload: &'static [u8]) {
    let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port).into();
    loop {
        let _ = sender.send_to(payload, addr).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn the_fan_out_forwards_the_hero_slot_not_the_lowest_registered_one() {
    // The operator-visible bug: with two drones registered, selecting the
    // one on slot 2 as hero promoted it to full video while the ground
    // station kept forwarding slot 1 — a different aircraft, and by then a
    // 1 fps thumbnail. Both slots transmit here, and the assertion is on
    // WHICH stream reaches the downstream consumers.
    let dir = tempfile::tempdir().unwrap();
    let registry_path = registry_with(dir.path(), &["drone-a", "drone-b"]);
    let hero_path = dir.path().join("fleet-hero.json");
    crate::fleet_hero::write_hero_to(&hero_path, 2, "drone-b").unwrap();

    // Stand-ins for the mediamtx ingest and the LCD tap.
    let consumer = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let lcd = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let targets = [consumer.local_addr().unwrap(), lcd.local_addr().unwrap()];

    // Stand-ins for the two slots' video egress ports.
    let slot1_port = free_port().await;
    let slot2_port = free_port().await;
    let port_for = move |slot: u8| match slot {
        1 => slot1_port,
        2 => slot2_port,
        other => panic!("no test port for slot {other}"),
    };

    let counters = FanoutCounters::new();
    let fanout = {
        let counters = counters.clone();
        tokio::spawn(async move {
            let _ = run_repointing_fanout(
                || resolve_fanout_slot(1, &hero_path, &registry_path),
                port_for,
                &targets,
                &counters,
                Duration::from_millis(25),
            )
            .await;
        })
    };

    // Both drones are transmitting; only the hero's stream may come through.
    let read_one = async {
        let mut buf = [0u8; 64];
        let n = consumer.recv(&mut buf).await.unwrap();
        buf[..n].to_vec()
    };
    let got = tokio::select! {
        _ = drive(slot1_port, b"slot-1-stream") => unreachable!("the driver never returns"),
        _ = drive(slot2_port, b"slot-2-stream") => unreachable!("the driver never returns"),
        res = tokio::time::timeout(Duration::from_secs(5), read_one) => {
            res.expect("nothing reached the downstream consumer")
        }
    };
    assert_eq!(
        got, b"slot-2-stream",
        "the fan-out forwarded the wrong drone: the hero is on slot 2"
    );

    fanout.abort();
}

#[tokio::test]
async fn the_fan_out_follows_the_hero_when_the_selection_changes() {
    // A hero change must reach the fan-out, or the first selection an
    // operator makes on a fleet that auto-promoted its lowest slot would
    // never take effect.
    let dir = tempfile::tempdir().unwrap();
    let registry_path = registry_with(dir.path(), &["drone-a", "drone-b"]);
    let hero_path = dir.path().join("fleet-hero.json");

    let consumer = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let targets = [consumer.local_addr().unwrap()];

    let slot1_port = free_port().await;
    let slot2_port = free_port().await;
    let port_for = move |slot: u8| match slot {
        1 => slot1_port,
        2 => slot2_port,
        other => panic!("no test port for slot {other}"),
    };

    let watch_path = hero_path.clone();
    let fanout = tokio::spawn(async move {
        let _ = run_repointing_fanout(
            || resolve_fanout_slot(1, &watch_path, &registry_path),
            port_for,
            &targets,
            &FanoutCounters::new(),
            Duration::from_millis(25),
        )
        .await;
    });

    let exercise = async {
        let mut buf = [0u8; 64];
        // No selection published: the fallback (lowest registered slot).
        loop {
            let n = consumer.recv(&mut buf).await.unwrap();
            if &buf[..n] == b"slot-1-stream" {
                break;
            }
        }
        // Now select the drone on slot 2, exactly as the hero route does.
        crate::fleet_hero::write_hero_to(&hero_path, 2, "drone-b").unwrap();
        // The fan-out must re-point; the slot-1 driver is still running, so
        // this only passes if it actually rebound.
        loop {
            let n = consumer.recv(&mut buf).await.unwrap();
            if &buf[..n] == b"slot-2-stream" {
                break;
            }
        }
    };

    tokio::select! {
        _ = drive(slot1_port, b"slot-1-stream") => unreachable!("the driver never returns"),
        _ = drive(slot2_port, b"slot-2-stream") => unreachable!("the driver never returns"),
        res = tokio::time::timeout(Duration::from_secs(5), exercise) => {
            res.expect("the fan-out never followed the new hero");
        }
    }

    fanout.abort();
}

#[test]
fn no_published_selection_serves_the_generation_primary() {
    // The boot state, and the permanent state of a single-drone fleet that
    // is never asked to choose. Falling back to the primary is what keeps
    // the shipped one-drone product streaming.
    let dir = tempfile::tempdir().unwrap();
    let registry_path = registry_with(dir.path(), &["drone-a", "drone-b"]);
    let hero_path = dir.path().join("fleet-hero.json");
    assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 1);

    // A malformed file is not a licence to point the fan-out somewhere
    // arbitrary; it reads as no selection.
    std::fs::write(&hero_path, b"{ truncated").unwrap();
    assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 1);
}

#[test]
fn a_selection_whose_drone_has_left_the_fleet_falls_back() {
    // Slot numbers are reissued. A selection left behind by a drone that has
    // unpaired must not keep the fan-out listening on a port nothing
    // transmits to, nor follow whichever drone inherited the slot.
    let dir = tempfile::tempdir().unwrap();
    let registry_path = registry_with(dir.path(), &["drone-a", "drone-b"]);
    let hero_path = dir.path().join("fleet-hero.json");

    // Registered, on the slot it claims: honoured.
    crate::fleet_hero::write_hero_to(&hero_path, 2, "drone-b").unwrap();
    assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 2);

    // Gone from the fleet entirely.
    crate::fleet_hero::write_hero_to(&hero_path, 2, "drone-departed").unwrap();
    assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 1);

    // Registered, but on a different slot than the one published.
    crate::fleet_hero::write_hero_to(&hero_path, 1, "drone-b").unwrap();
    assert_eq!(resolve_fanout_slot(1, &hero_path, &registry_path), 1);
}

#[test]
fn the_rebind_backoff_is_bounded_and_non_zero() {
    // A port that cannot be bound must not spin the reactor, and one that is
    // momentarily unbindable must come back quickly.
    assert!(!REBIND_BACKOFF.is_zero());
    assert!(REBIND_BACKOFF <= Duration::from_secs(2));
    assert!(!HERO_POLL_INTERVAL.is_zero());
}

#[tokio::test]
async fn fan_out_delivers_each_datagram_to_both_targets() {
    // Bind the two downstream consumers on ephemeral ports first so we know
    // their addresses for the targets list.
    let mediamtx = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let lcd = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let mediamtx_addr = mediamtx.local_addr().unwrap();
    let lcd_addr = lcd.local_addr().unwrap();

    // Listen on an ephemeral port (stand-in for 5599) so the test never
    // collides with a real fan-out or another parallel test.
    let listen = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let listen_addr = listen.local_addr().unwrap();
    drop(listen); // free it for run_fanout to bind.

    let targets = [mediamtx_addr, lcd_addr];
    let counters = FanoutCounters::new();
    let counters_task = counters.clone();
    let fanout = tokio::spawn(async move {
        // Ignore the never-Ok result; the task is aborted at test end.
        let _ = run_fanout(listen_addr, &targets, &counters_task).await;
    });

    // UDP gives no delivery guarantee and the fan-out's bind may land after
    // our first send, so resend on a short cadence until each consumer has
    // seen the payload. The fan-out spawn task is racing us; a handful of
    // datagrams is plenty.
    let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let payload = b"the-rtp-payload";
    let resend = async {
        loop {
            let _ = sender.send_to(payload, listen_addr).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };

    // Both consumers should receive the datagram while the resender runs.
    let recv_both = async {
        let mut mbuf = [0u8; 64];
        let mut lbuf = [0u8; 64];
        let mlen = mediamtx.recv(&mut mbuf).await.unwrap();
        assert_eq!(&mbuf[..mlen], payload);
        let llen = lcd.recv(&mut lbuf).await.unwrap();
        assert_eq!(&lbuf[..llen], payload);
    };

    tokio::select! {
        _ = resend => unreachable!("resender never returns"),
        res = tokio::time::timeout(Duration::from_secs(5), recv_both) => {
            res.expect("consumers timed out waiting for fan-out delivery");
        }
    }

    // The shared counter saw the forwarded datagrams (the cross-process
    // signal the sidecar surfaces). At least the ones both consumers read.
    assert!(
        counters.forwarded() >= 1,
        "fan-out forwarded counter did not advance"
    );

    fanout.abort();
}

#[tokio::test]
async fn empty_targets_is_rejected() {
    let listen: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 0).into();
    let err = run_fanout(listen, &[], &FanoutCounters::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}

#[test]
fn error_backoff_is_bounded_and_non_zero() {
    // The backoff guard must be a small, non-zero, finite sleep so a wedged
    // recv socket neither busy-spins (zero) nor stalls a recoverable
    // generation (too long). It only engages after a sustained error run.
    const { assert!(ERROR_BACKOFF_THRESHOLD >= 1) };
    assert!(!ERROR_BACKOFF.is_zero());
    assert!(ERROR_BACKOFF <= Duration::from_secs(1));
}

#[tokio::test]
async fn fan_out_survives_a_target_that_starts_down_then_comes_up() {
    // A downstream consumer that is not yet listening can make a `send_to`
    // fail and, on the next read, surface a recv error on the local socket.
    // The loop must NOT end on that: it must keep forwarding so that once the
    // consumer comes up it receives the stream. This proves the recv loop is
    // resilient rather than dying on the first transient error.

    // Bind one live consumer up front; leave the second target pointed at a
    // port we deliberately do not bind until partway through.
    let live = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let live_addr = live.local_addr().unwrap();

    // Reserve, then free, a port for the "down" consumer so we know its
    // address but nothing is listening there at first.
    let down_reserved = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let down_addr = down_reserved.local_addr().unwrap();
    drop(down_reserved);

    let listen = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let listen_addr = listen.local_addr().unwrap();
    drop(listen);

    let targets = [live_addr, down_addr];
    let fanout = tokio::spawn(async move {
        let _ = run_fanout(listen_addr, &targets, &FanoutCounters::new()).await;
    });

    let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let payload = b"the-rtp-payload";

    // Drive traffic while the down target is closed; the live consumer keeps
    // receiving regardless, proving the loop did not die.
    let resend = async {
        loop {
            let _ = sender.send_to(payload, listen_addr).await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    };

    let live_keeps_receiving = async {
        let mut buf = [0u8; 64];
        // Receive several datagrams across the window where the other target
        // is down, then bind that target and confirm it starts receiving too.
        for _ in 0..3 {
            let n = live.recv(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], payload);
        }
        // Now bring the previously-down consumer up and confirm the still-alive
        // loop delivers to it.
        let down = UdpSocket::bind(down_addr).await.unwrap();
        let n = tokio::time::timeout(Duration::from_secs(3), down.recv(&mut buf))
            .await
            .expect("recovered target never received after coming up")
            .unwrap();
        assert_eq!(&buf[..n], payload);
    };

    tokio::select! {
        _ = resend => unreachable!("resender never returns"),
        res = tokio::time::timeout(Duration::from_secs(6), live_keeps_receiving) => {
            res.expect("fan-out stopped forwarding to the live consumer");
        }
    }

    fanout.abort();
}

#[test]
fn an_idle_ground_station_with_no_drone_paired_is_not_a_stall() {
    // Nothing has ever been forwarded on this bind, so a flat counter is the
    // correct reading, not a fault. If this ever reported Stalled the
    // watchdog would rebind the socket of every ground station that is
    // simply powered on ahead of its aircraft — including forever, since the
    // condition never clears on its own.
    let d = fanout_decision(0, 0, Duration::from_secs(600), 0, FANOUT_STALL_WINDOW);
    assert_eq!(d.forward, ForwardState::AwaitingFirstDatagram);
    assert_eq!(d.drain, DrainState::Draining);
}

#[test]
fn an_advancing_forward_count_is_healthy() {
    // The live-video case: the counter moved since the last sample.
    let d = fanout_decision(1_000, 4_500, Duration::ZERO, 0, FANOUT_STALL_WINDOW);
    assert_eq!(d.forward, ForwardState::Advancing { datagrams: 3_500 });

    // Flat, but only just: inside the window this is still healthy, because
    // decoded output is allowed the odd hiccup without a rebind.
    let d = fanout_decision(
        4_500,
        4_500,
        FANOUT_STALL_WINDOW - Duration::from_millis(1),
        0,
        FANOUT_STALL_WINDOW,
    );
    assert_eq!(d.forward, ForwardState::WithinStallWindow);
}

#[test]
fn a_forward_count_frozen_for_the_window_after_forwarding_is_stalled() {
    // The real fault: this bind DID forward video, and then the count froze
    // for the whole window. At thousands of datagrams a second that is not
    // jitter, and the frozen value must travel with the verdict so the log
    // names it.
    let d = fanout_decision(9_001, 9_001, FANOUT_STALL_WINDOW, 0, FANOUT_STALL_WINDOW);
    assert_eq!(d.forward, ForwardState::Stalled { frozen_at: 9_001 });
    // The stall is a forward-path fault only: a fan-out with nothing to send
    // drops nothing, so the drain axis must stay clean.
    assert_eq!(d.drain, DrainState::Draining);
}

#[test]
fn climbing_drops_are_reported_as_their_own_degradation() {
    // A consumer that is not draining is a DIFFERENT fault from a flat
    // forward count: the fan-out is reading and relaying correctly, so
    // rebinding its socket would fix nothing. The two must be separable.
    let d = fanout_decision(
        1_000,
        5_000,
        Duration::ZERO,
        FANOUT_DROP_DEGRADED_DELTA,
        FANOUT_STALL_WINDOW,
    );
    assert_eq!(
        d.drain,
        DrainState::NotDraining {
            drops: FANOUT_DROP_DEGRADED_DELTA
        }
    );
    assert_eq!(d.forward, ForwardState::Advancing { datagrams: 4_000 });

    // A consumer restarting sheds a handful of datagrams; that is not a
    // sustained climb and must not be reported as one.
    let d = fanout_decision(1_000, 5_000, Duration::ZERO, 3, FANOUT_STALL_WINDOW);
    assert_eq!(d.drain, DrainState::Draining);
}

#[test]
fn the_stall_window_is_far_above_the_hops_packet_cadence() {
    // The window has to be orders of magnitude longer than the real
    // inter-datagram gap (sub-millisecond at tens of Mbps) so a flat counter
    // across it is unambiguous, and short enough that a frozen picture is
    // acted on rather than watched.
    assert!(FANOUT_STALL_WINDOW >= Duration::from_secs(1));
    assert!(FANOUT_STALL_WINDOW <= Duration::from_secs(10));
    // Several samples per window, or a stall is caught a window late.
    assert!(FANOUT_SAMPLE_INTERVAL * 3 <= FANOUT_STALL_WINDOW);
}

#[tokio::test]
async fn the_watchdog_returns_the_frozen_count_once_forwarding_stops() {
    // The wiring seam: a generation that forwarded and then froze must make
    // the watchdog return, which is what drives the rebind in
    // `run_repointing_fanout`.
    let counters = FanoutCounters::new();
    counters.forwarded.fetch_add(42, Ordering::Relaxed);
    let frozen = tokio::time::timeout(
        Duration::from_secs(5),
        watch_for_fanout_stall(
            &counters,
            0,
            Duration::from_millis(200),
            Duration::from_millis(20),
        ),
    )
    .await
    .expect("the watchdog never reported the stalled fan-out");
    assert_eq!(frozen, 42, "the stall must name the frozen counter value");
}

#[tokio::test]
async fn the_watchdog_stays_quiet_while_nothing_has_arrived_and_while_forwarding() {
    // Two non-faults the watchdog must never act on, because acting on
    // either would rebind a working (or idle) socket in a loop.
    let counters = FanoutCounters::new();

    // (1) No datagram has ever arrived on this bind.
    let idle = tokio::time::timeout(
        Duration::from_millis(600),
        watch_for_fanout_stall(
            &counters,
            0,
            Duration::from_millis(100),
            Duration::from_millis(10),
        ),
    )
    .await;
    assert!(
        idle.is_err(),
        "an idle ground station was reported as a stalled fan-out"
    );

    // (2) The counter keeps advancing, as it does with video flowing.
    let bump = {
        let counters = counters.clone();
        tokio::spawn(async move {
            loop {
                counters.forwarded.fetch_add(1_000, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };
    let flowing = tokio::time::timeout(
        Duration::from_millis(600),
        watch_for_fanout_stall(
            &counters,
            counters.forwarded(),
            Duration::from_millis(100),
            Duration::from_millis(10),
        ),
    )
    .await;
    bump.abort();
    assert!(
        flowing.is_err(),
        "a fan-out that is forwarding was reported as stalled"
    );
}

#[tokio::test]
async fn the_input_socket_absorbs_a_burst_the_default_buffer_would_drop() {
    // The regression this pins: the port dropped the 4 MB `SO_RCVBUF` the
    // Python fan-out set, so a burst arriving while nothing is draining the
    // socket is discarded by the kernel BELOW the application, invisible to
    // every counter. Asserted behaviourally rather than by size, because
    // Linux doubles the request and caps it at `rmem_max`: a clamped buffer
    // still absorbs far more than the default.
    const DATAGRAMS: usize = 1_200;
    let payload = [0xABu8; 1_400];

    let sock = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    set_recv_buffer(&sock);
    let addr = sock.local_addr().unwrap();

    let sender = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    // Nothing reads while the burst lands, exactly like the read loop being
    // busy in `send_to` — the kernel buffer is the only thing holding it.
    for _ in 0..DATAGRAMS {
        sender.send_to(&payload, addr).await.unwrap();
    }

    let mut buf = [0u8; 2_048];
    let mut received = 0usize;
    while received < DATAGRAMS {
        match tokio::time::timeout(Duration::from_millis(200), sock.recv(&mut buf)).await {
            Ok(Ok(n)) => {
                assert_eq!(n, payload.len());
                received += 1;
            }
            _ => break,
        }
    }

    // The default buffer (~208 KB on Linux, ~768 KB on macOS) cannot hold
    // 1.68 MB of datagrams; the 4 MB request can. 700 is comfortably above
    // what any stock default holds and below the full burst, so the
    // assertion survives a kernel clamp without asserting a size.
    assert!(
        received >= 700,
        "only {received}/{DATAGRAMS} datagrams survived the burst: the receive buffer is at the kernel default"
    );
}

#[tokio::test]
async fn the_output_socket_send_buffer_request_takes_effect() {
    // The send side cannot be observed by absorbing a burst, so pin what is
    // observable: the request moved the socket off its default. The exact
    // value is deliberately not asserted — Linux doubles the request for its
    // own bookkeeping and caps it at `wmem_max`.
    let fresh = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let default = socket2::SockRef::from(&fresh).send_buffer_size().unwrap();

    let sized = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    set_send_buffer(&sized);
    let actual = socket2::SockRef::from(&sized).send_buffer_size().unwrap();

    assert!(
        actual > default,
        "the send buffer stayed at the kernel default ({default} -> {actual})"
    );
}
