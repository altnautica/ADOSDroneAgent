use std::time::Duration;

use super::*;

#[test]
fn feed_peers_bind_apart() {
    let base: SocketAddr = "127.0.0.1:14650".parse().unwrap();
    let ports = [
        FeedPeer::Pilot,
        FeedPeer::Logd,
        FeedPeer::Plugins,
        FeedPeer::Router,
    ]
    .map(|p| p.addr(base).port());
    assert_eq!(ports, [14650, 14651, 14652, 14653]);
}

#[test]
fn wall_config_stays_on_wall() {
    let gate =
        ados_config::simulation::read_gate_from("simulation:\n  enabled: false\n  clock: sim\n");
    let clock = from_config(&gate, FeedPeer::Pilot).unwrap();
    assert_eq!(clock.source(), "wall");
    assert_eq!(plugin_feed(&gate), None);
    let sim = ados_config::simulation::read_gate_from(
        "simulation:\n  enabled: true\n  clock: sim\n  clock_feed: 127.0.0.1:15000\n",
    );
    assert_eq!(plugin_feed(&sim), Some("127.0.0.1:15002".parse().unwrap()));
}

#[tokio::test]
async fn the_relay_forwards_ticks_to_followers() {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let listen = probe.local_addr().unwrap();
    drop(probe);
    let dir = std::env::temp_dir().join(format!("ados-time-relay-{}", std::process::id()));
    let socket = relay_socket(&dir);
    tokio::spawn(relay(listen, socket.clone(), None));
    let clock = Clock::Sim(SimFeed::follow_relay(socket));
    let sim = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let tick = wire::Tick {
        flags: 0,
        seq: 1,
        sim_ns: 70_000_000,
        grant_ns: 90_000_000,
        speed: 3.0,
    };
    // Resend until the follower has connected to the relay.
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            sim.send_to(&tick.encode(), listen).await.unwrap();
            if clock.sim_ns() == Some(70_000_000) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(got.is_ok(), "the tick never reached the subscriber");
    assert_eq!(clock.speed(), Some(3.0));
}

#[test]
fn lockstep_needs_the_sim_clock_and_the_mode() {
    let run = Path::new("/run/ados");
    let lock = ados_config::simulation::read_gate_from(
        "simulation:\n  enabled: true\n  clock: sim\n  clock_mode: lockstep\n",
    );
    assert_eq!(
        lockstep_socket(&lock, run),
        Some(PathBuf::from("/run/ados/simclock/participants.sock"))
    );
    let follow =
        ados_config::simulation::read_gate_from("simulation:\n  enabled: true\n  clock: sim\n");
    assert_eq!(lockstep_socket(&follow, run), None);
    let wall = ados_config::simulation::read_gate_from(
        "simulation:\n  enabled: true\n  clock_mode: lockstep\n",
    );
    assert_eq!(lockstep_socket(&wall, run), None);
    assert!(participant(None).is_none());
}

/// The simulator's side of one grant: send the tick, return the `ACK` that
/// answered it within `wait`, if any.
async fn grant(
    sim: &tokio::net::UdpSocket,
    to: SocketAddr,
    seq: u32,
    sim_ms: u64,
    wait: Duration,
) -> Option<wire::Ack> {
    let t = wire::Tick {
        flags: wire::FLAG_LOCKSTEP,
        seq,
        sim_ns: sim_ms * 1_000_000,
        grant_ns: (sim_ms + 20) * 1_000_000,
        speed: 0.0,
    };
    sim.send_to(&t.encode(), to).await.unwrap();
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(wait, sim.recv(&mut buf))
        .await
        .ok()?
        .unwrap();
    wire::Ack::decode(&buf[..n])
}

#[tokio::test]
async fn the_barrier_acknowledges_a_grant_once_every_participant_is_done() {
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let listen = probe.local_addr().unwrap();
    drop(probe);
    let dir = std::env::temp_dir().join(format!("ados-time-barrier-{}", std::process::id()));
    let parts = participants_socket(&dir);
    tokio::spawn(relay(listen, relay_socket(&dir), Some(parts.clone())));
    let sim = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    // With no participant yet, a grant is acknowledged at once (retry until
    // the relay has bound).
    let mut first = None;
    for seq in 1..=50 {
        first = grant(&sim, listen, seq, 0, Duration::from_millis(100)).await;
        if first.is_some() {
            break;
        }
    }
    let first = first.expect("the relay never answered");
    assert_eq!(first.parts, 0);
    assert_eq!(first.done_ns, 20_000_000);

    let pilot = Participant::connect(parts.clone());
    // Wait until the pilot has registered: a grant then waits on it.
    let mut seq = 100;
    loop {
        seq += 1;
        if grant(&sim, listen, seq, 20, Duration::from_millis(100))
            .await
            .is_none()
        {
            break;
        }
        assert!(seq < 200, "the pilot never registered");
    }
    // The held grant is released by the pilot's report past its sim time.
    pilot.done(40_000_000);
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(3), sim.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    let ack = wire::Ack::decode(&buf[..n]).unwrap();
    assert_eq!((ack.seq, ack.parts, ack.done_ns), (seq, 1, 40_000_000));
    // The next grant needs work due at 40 ms: held until the pilot reports it.
    assert!(grant(&sim, listen, seq + 1, 40, Duration::from_millis(200))
        .await
        .is_none());
    pilot.done(60_000_000);
    let n = tokio::time::timeout(Duration::from_secs(3), sim.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wire::Ack::decode(&buf[..n]).unwrap().seq, seq + 1);
    // A participant that goes away stops holding grants.
    let plugin = Participant::connect(parts);
    tokio::time::sleep(Duration::from_millis(200)).await;
    pilot.done(80_000_000);
    assert!(grant(&sim, listen, seq + 2, 60, Duration::from_millis(200))
        .await
        .is_none());
    drop(plugin);
    let n = tokio::time::timeout(Duration::from_secs(3), sim.recv(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wire::Ack::decode(&buf[..n]).unwrap().seq, seq + 2);
    let _ = std::fs::remove_dir_all(&dir);
}
