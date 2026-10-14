//! The loopback transport end to end, through an in-test relay that speaks the
//! shared-medium relay's wire format:
//!
//! - control, TCP `base`: one JSON line per connection (`register` only here).
//! - data, UDP `base + 1`: `!I channel, !B name_len, name, frame`.
//! - downlink, UDP to each registered node's address and port: `!I channel, frame`,
//!   to every `rx` registration on the channel except the sender's own node.
//!
//! Two buses of one fleet exchange beacons through the production seal, frame
//! and ingest path; a third bus on another fleet shares the channel, as a second
//! fleet shares the air, and its frames never surface from the radio.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use ados_swarmbus::crypto::SwarmCipher;
use ados_swarmbus::frame::{build_frame, ieee80211_header, radiotap_header, MAX_FRAME_LEN};
use ados_swarmbus::ingest::Ingest;
use ados_swarmbus::radio::loopback::{LoopbackConfig, LOOPBACK_CHANNEL};
use ados_swarmbus::radio::{Radio, Transport};
use ados_swarmbus::{NeighborTable, SwarmBeacon, SwarmBus};
use serde_json::Value;

const WAIT: Duration = Duration::from_secs(5);

/// One registration: node, channel, direction, downlink address.
type Reg = (String, u32, String, SocketAddr);

/// One uplink: sender node and frame.
type Uplink = (String, Vec<u8>);

/// A relay with no impairment: registrations, routing and the uplink record.
struct StubRelay {
    base: u16,
    regs: Arc<Mutex<Vec<Reg>>>,
    /// Every uplink seen: sender node and frame.
    uplinks: Arc<Mutex<Vec<Uplink>>>,
}

impl StubRelay {
    fn start() -> Self {
        let (control, data) = bind_pair();
        let base = control.local_addr().unwrap().port();
        let regs: Arc<Mutex<Vec<Reg>>> = Arc::default();
        let uplinks: Arc<Mutex<Vec<Uplink>>> = Arc::default();
        let r = regs.clone();
        std::thread::spawn(move || {
            for conn in control.incoming().flatten() {
                let peer = conn.peer_addr().unwrap().ip();
                let mut line = String::new();
                if BufReader::new(conn).read_line(&mut line).is_err() {
                    continue;
                }
                let req: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(req["op"], "register");
                let ip = req["addr"].as_str().map_or(peer, |a| a.parse().unwrap());
                let port = req["port"].as_u64().unwrap() as u16;
                r.lock().push((
                    req["node"].as_str().unwrap().to_string(),
                    req["channel"].as_u64().unwrap() as u32,
                    req["direction"].as_str().unwrap().to_string(),
                    SocketAddr::new(ip, port),
                ));
            }
        });
        let (r, u) = (regs.clone(), uplinks.clone());
        std::thread::spawn(move || {
            let out = UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut buf = [0u8; 65536];
            while let Ok((n, _)) = data.recv_from(&mut buf) {
                let dg = &buf[..n];
                let channel = u32::from_be_bytes(dg[..4].try_into().unwrap());
                let name_len = dg[4] as usize;
                let src = String::from_utf8(dg[5..5 + name_len].to_vec()).unwrap();
                let frame = &dg[5 + name_len..];
                u.lock().push((src.clone(), frame.to_vec()));
                let mut down = channel.to_be_bytes().to_vec();
                down.extend_from_slice(frame);
                for (node, ch, dir, addr) in r.lock().iter() {
                    if *ch == channel && dir == "rx" && *node != src {
                        out.send_to(&down, addr).unwrap();
                    }
                }
            }
        });
        Self {
            base,
            regs,
            uplinks,
        }
    }

    fn config(&self, node: &str) -> LoopbackConfig {
        LoopbackConfig {
            relay_host: "127.0.0.1".into(),
            relay_port: self.base,
            node: node.into(),
            node_addr: None,
            listen: "127.0.0.1:0".parse().unwrap(),
        }
    }

    /// Block until `n` registrations have landed: a registration is processed
    /// asynchronously after the open returns, on the real relay as here.
    fn await_registrations(&self, n: usize) {
        let deadline = Instant::now() + WAIT;
        while self.regs.lock().len() < n {
            assert!(Instant::now() < deadline, "registrations did not land");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// A TCP control port and a UDP data port one above it.
fn bind_pair() -> (TcpListener, UdpSocket) {
    for _ in 0..64 {
        let control = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = control.local_addr().unwrap().port();
        if port == u16::MAX {
            continue;
        }
        if let Ok(data) = UdpSocket::bind(("127.0.0.1", port + 1)) {
            return (control, data);
        }
    }
    panic!("no adjacent free port pair");
}

fn beacon(slot: u8) -> SwarmBeacon {
    SwarmBeacon {
        slot,
        seq_ms: 1234,
        lat: 129_716_000,
        lon: 776_946_000,
        alt_dm: 300,
        vx_cms: 120,
        vy_cms: -40,
        vz_cms: 0,
        status: 0,
    }
}

fn bus(relay: &StubRelay, node: &str, fleet: u16, slot: u8, key: u8) -> SwarmBus {
    let transport = Transport::Loopback(relay.config(node));
    let cipher = Arc::new(SwarmCipher::new(&[key; 32]));
    SwarmBus::open_on(&transport, "unused", fleet, slot, cipher).unwrap()
}

#[tokio::test]
async fn two_buses_exchange_beacons_through_the_relay() {
    let relay = StubRelay::start();
    let a = bus(&relay, "drone-1-swarm", 1, 1, 7);
    let b = bus(&relay, "drone-2-swarm", 1, 2, 7);
    relay.await_registrations(4);
    assert_eq!(a.iface(), "loopback:drone-1-swarm");

    a.broadcast(&beacon(1)).await.unwrap();
    let table_b = Mutex::new(NeighborTable::new(2));
    let got = tokio::time::timeout(WAIT, b.recv_into(&table_b))
        .await
        .expect("b hears a")
        .unwrap();
    assert_eq!(got, Ingest::Beacon(beacon(1)));
    assert!(table_b.lock().get(1).is_some(), "a is in b's table");

    b.broadcast(&beacon(2)).await.unwrap();
    let table_a = Mutex::new(NeighborTable::new(1));
    let got = tokio::time::timeout(WAIT, a.recv_into(&table_a))
        .await
        .expect("a hears b")
        .unwrap();
    assert_eq!(got, Ingest::Beacon(beacon(2)));

    // The relay carried the injected frame itself: radiotap, 802.11 header and
    // the 50-byte sealed beacon, on the swarm channel, from the named node.
    let uplinks = relay.uplinks.lock();
    let (src, frame) = &uplinks[0];
    assert_eq!(src, "drone-1-swarm");
    assert_eq!(frame.len(), 87);
    assert_eq!(&frame[..13], &radiotap_header(0));
    assert_eq!(&frame[13..35], &ieee80211_header(1, 0)[..22]);
    for (_, ch, _, _) in relay.regs.lock().iter() {
        assert_eq!(*ch, LOOPBACK_CHANNEL);
    }
}

/// Frames the kernel filter would reject never leave the receive call: another
/// fleet's beacon and a wfb-ng frame on the same channel are skipped, and the
/// next frame of our own fleet is returned byte-for-byte.
#[tokio::test]
async fn the_receive_filter_drops_what_the_kernel_filter_would() {
    let relay = StubRelay::start();
    let open = |node: &str, fleet| {
        Radio::open_on(&Transport::Loopback(relay.config(node)), "unused", fleet).unwrap()
    };
    let rx = open("gs-swarm", 1);
    let foreign = open("other-fleet-swarm", 2);
    let own = open("drone-1-swarm", 1);
    relay.await_registrations(6);

    let mut wfb = radiotap_header(1).to_vec();
    let mut mac = ieee80211_header(1, 0);
    mac[10..12].copy_from_slice(&0x5742u16.to_be_bytes());
    wfb.extend_from_slice(&mac);
    wfb.extend_from_slice(&[0u8; 32]);
    let theirs = build_frame(2, 0, &[0xEE; 50]);
    let ours = build_frame(1, 9, &[0x11; 50]);
    foreign.send(&theirs).unwrap();
    foreign.send(&wfb).unwrap();
    // Let the two rejected frames reach the receiver first.
    let deadline = Instant::now() + WAIT;
    while relay.uplinks.lock().len() < 2 {
        assert!(Instant::now() < deadline, "uplinks did not arrive");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(own.send(&ours).unwrap(), ours.len());

    let mut buf = [0u8; MAX_FRAME_LEN];
    let n = tokio::time::timeout(WAIT, rx.recv(&mut buf))
        .await
        .expect("our fleet's frame arrives")
        .unwrap();
    assert_eq!(&buf[..n], &ours[..]);
}
