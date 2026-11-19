use std::time::Duration;

use super::*;

#[test]
fn feed_peers_bind_apart() {
    let base: SocketAddr = "127.0.0.1:14650".parse().unwrap();
    let ports = [FeedPeer::Pilot, FeedPeer::Logd, FeedPeer::Plugins].map(|p| p.addr(base).port());
    assert_eq!(ports, [14650, 14651, 14652]);
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
    tokio::spawn(relay(listen, socket.clone()));
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
