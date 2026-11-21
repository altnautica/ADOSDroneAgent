use super::gate::{Claimed, Dropped};
use super::*;
use crate::connection::ClientOrigin;
use ados_protocol::follow_beacon::{
    EST_POS, EST_VEL, FOLLOW_BEACON_TOPIC, ORIGIN_GROUND_LINK, ORIGIN_ON_BOX, ORIGIN_RELAY,
};
use ados_protocol::ipc::{InjectorClaim, IpcPeer};
use ados_protocol::mavlink::ardupilotmega::{FOLLOW_TARGET_DATA, HEARTBEAT_DATA};
use ados_protocol::mavlink::{serialize_v2, MavHeader};
use tokio::io::AsyncBufReadExt;

mod strip_tests;

const WS: BeaconLink = BeaconLink {
    origin: Origin::GroundLink,
    id: LinkId::Ws(3),
};
const PHONE: Claimed = Claimed {
    sysid: 255,
    compid: 190,
};

pub(super) fn fix(ts_ms: u64, acc_m: f32) -> FOLLOW_TARGET_DATA {
    FOLLOW_TARGET_DATA {
        timestamp: ts_ms,
        lat: 473_977_420,
        lon: 85_455_940,
        alt: 488.5,
        vel: [1.0, -0.5, 0.0],
        position_cov: [acc_m, 6.0, 0.0],
        est_capabilities: EST_POS | EST_VEL,
        ..FOLLOW_TARGET_DATA::DEFAULT
    }
}

pub(super) fn frame(msg: &MavMessage) -> Vec<u8> {
    serialize_v2(
        MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        },
        msg,
    )
    .unwrap()
}

fn admit(
    gate: &mut BeaconGate,
    link: BeaconLink,
    msg: &FOLLOW_TARGET_DATA,
    wall_ms: u64,
) -> Result<FollowBeacon, Dropped> {
    gate.admit(link, PHONE, msg, wall_ms, 7)
}

#[test]
fn an_authorised_fix_becomes_the_documented_beacon() {
    let mut gate = BeaconGate::default();
    let b = gate
        .admit(WS, PHONE, &fix(10_000, 4.0), 1_000, 7_000_000_000)
        .unwrap();
    assert!((b.lat - 47.397_742).abs() < 1e-9 && (b.lon - 8.545_594).abs() < 1e-9);
    assert_eq!(b.alt_m, Some(488.5));
    assert_eq!(b.vel_ned, Some([1.0, -0.5, 0.0]));
    assert_eq!((b.horiz_acc_m, b.vert_acc_m), (4.0, Some(6.0)));
    assert_eq!(b.stamp_ns, 7_000_000_000, "stamped with ADOS time");
    assert_eq!(
        (b.origin.as_str(), b.link.as_str()),
        (ORIGIN_GROUND_LINK, "ws-3")
    );
    assert_eq!((b.sysid, b.compid), (255, 190));

    // Unknowns read as unknown.
    let mut m = fix(10_200, 4.0);
    m.alt = f32::NAN;
    m.position_cov[1] = 0.0;
    m.est_capabilities = EST_POS;
    let b = admit(&mut gate, WS, &m, 1_200).unwrap();
    assert_eq!((b.alt_m, b.vel_ned, b.vert_acc_m), (None, None, None));
    assert_eq!(gate.counters.published, 2);
}

#[test]
fn the_router_stamps_origin_and_link_from_how_the_frame_arrived() {
    let plain = IpcPeer {
        conn: 12,
        ..IpcPeer::default()
    };
    assert_eq!(Origin::of_ipc(&plain), Origin::OnBox);
    let cloud = IpcPeer {
        off_box_source: true,
        ..plain.clone()
    };
    assert_eq!(Origin::of_ipc(&cloud), Origin::Relay);
    for (origin, label) in [
        (Origin::OnBox, ORIGIN_ON_BOX),
        (Origin::Relay, ORIGIN_RELAY),
    ] {
        let link = BeaconLink {
            origin,
            id: LinkId::Ipc(12),
        };
        let b = admit(&mut BeaconGate::default(), link, &fix(1, 4.0), 0).unwrap();
        assert_eq!((b.origin.as_str(), b.link.as_str()), (label, "ipc-12"));
    }
    let aux = BeaconLink {
        origin: Origin::Relay,
        id: LinkId::Aux,
    };
    assert_eq!(
        admit(&mut BeaconGate::default(), aux, &fix(1, 4.0), 0)
            .unwrap()
            .link,
        "aux"
    );
    let udp = LinkId::Udp("192.168.1.50:14550".parse().unwrap()).label();
    assert!(udp.starts_with("udp-") && !udp.contains("192"), "{udp}");
    assert_eq!(Origin::of_proxy(ClientOrigin::Trusted), Origin::GroundLink);
}

#[test]
fn unauthenticated_peers_plugins_and_the_plugin_host_are_refused() {
    let mut gate = BeaconGate::default();
    let host_link = IpcPeer {
        plugin_relay: true,
        conn: 4,
        ..IpcPeer::default()
    };
    let injector = IpcPeer {
        injector: Some(InjectorClaim {
            client_id: "uid:1201".into(),
            ticket: None,
        }),
        conn: 5,
        ..IpcPeer::default()
    };
    let anon = Origin::of_proxy(ClientOrigin::Unauthenticated);
    for origin in [Origin::of_ipc(&host_link), Origin::of_ipc(&injector), anon] {
        let link = BeaconLink {
            origin,
            id: LinkId::Ipc(4),
        };
        assert_eq!(
            admit(&mut gate, link, &fix(1, 4.0), 0),
            Err(Dropped::Origin)
        );
    }
    assert_eq!(gate.counters.dropped_origin, 3);
    assert_eq!(gate.counters.published, 0);
}

#[test]
fn a_beacon_from_the_plugin_host_link_never_reaches_the_bus() {
    let (link, mut rx) = FollowBeaconLink::new();
    let host = BeaconLink {
        origin: Origin::of_ipc(&IpcPeer {
            plugin_relay: true,
            ..IpcPeer::default()
        }),
        id: LinkId::Ipc(1),
    };
    let data = frame(&MavMessage::FOLLOW_TARGET(fix(2_000, 3.0)));
    link.observe_at(host, &data, 1_000, 1);
    assert!(rx.try_recv().is_err());
    assert_eq!(link.status()["dropped_origin"], 1);
}

#[test]
fn a_fix_without_position_or_with_poor_accuracy_is_dropped() {
    let mut gate = BeaconGate::default();
    let mut no_pos = fix(1, 4.0);
    no_pos.est_capabilities = EST_VEL;
    assert_eq!(admit(&mut gate, WS, &no_pos, 0), Err(Dropped::Position));
    let mut null_island = fix(1, 4.0);
    (null_island.lat, null_island.lon) = (0, 0);
    assert_eq!(
        admit(&mut gate, WS, &null_island, 0),
        Err(Dropped::Position)
    );
    for acc in [30.5, 0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(
            admit(&mut gate, WS, &fix(1, acc), 0),
            Err(Dropped::Accuracy),
            "{acc}"
        );
    }
    assert!(
        admit(&mut gate, WS, &fix(1, 30.0), 0).is_ok(),
        "30 m is the limit"
    );
    assert_eq!(gate.counters.dropped_accuracy, 5);
    assert_eq!(gate.counters.dropped_position, 2);
}

#[test]
fn one_link_at_a_time_at_ten_hertz() {
    let mut gate = BeaconGate::default();
    let other = BeaconLink {
        origin: Origin::GroundLink,
        id: LinkId::Tcp(9),
    };
    assert!(admit(&mut gate, WS, &fix(1_000, 4.0), 5_000).is_ok());
    assert_eq!(
        admit(&mut gate, WS, &fix(1_050, 4.0), 5_050),
        Err(Dropped::Rate)
    );
    assert!(admit(&mut gate, WS, &fix(1_100, 4.0), 5_100).is_ok());
    // Another link is refused while the first is heard, whatever ids it claims.
    assert_eq!(
        admit(&mut gate, other, &fix(9, 4.0), 5_200),
        Err(Dropped::OtherLink)
    );
    assert_eq!(
        admit(&mut gate, other, &fix(9, 4.0), 10_000),
        Err(Dropped::OtherLink)
    );
    // Five seconds of silence from the first, and the other takes over.
    assert!(admit(&mut gate, other, &fix(10, 4.0), 10_101).is_ok());
    assert_eq!(
        admit(&mut gate, WS, &fix(9_000, 4.0), 10_300),
        Err(Dropped::OtherLink)
    );
    let c = gate.counters;
    assert_eq!(
        (c.dropped_rate, c.dropped_other_link, c.published),
        (1, 3, 3)
    );
}

#[test]
fn a_late_repeated_or_reordered_fix_is_stale() {
    let mut gate = BeaconGate::default();
    // The first fix sets the link's clock offset to wall: -10 000 ms.
    assert!(admit(&mut gate, WS, &fix(20_000, 4.0), 10_000).is_ok());
    // Arrives 2.5 s later than its least-delayed sibling.
    assert_eq!(
        admit(&mut gate, WS, &fix(20_200, 4.0), 12_700),
        Err(Dropped::Stale)
    );
    // 1.9 s late is still inside the window.
    assert!(admit(&mut gate, WS, &fix(22_000, 4.0), 13_900).is_ok());
    // A repeat and a reordered fix.
    assert_eq!(
        admit(&mut gate, WS, &fix(22_000, 4.0), 14_000),
        Err(Dropped::Stale)
    );
    assert_eq!(
        admit(&mut gate, WS, &fix(21_900, 4.0), 14_100),
        Err(Dropped::Stale)
    );
    // A restarted sender (its clock far behind its last fix) starts a new baseline.
    assert!(admit(&mut gate, WS, &fix(50, 4.0), 14_200).is_ok());
    // So does one silent past the lock, whatever its offset now.
    assert!(admit(&mut gate, WS, &fix(60_000, 4.0), 50_000).is_ok());
    assert_eq!(gate.counters.dropped_stale, 3);
}

#[test]
fn extreme_fix_times_are_judged_without_overflow() {
    let mut gate = BeaconGate::default();
    assert_eq!(
        admit(&mut gate, WS, &fix(u64::MAX, 4.0), 0),
        Err(Dropped::Time)
    );
    assert_eq!(
        admit(&mut gate, WS, &fix(i64::MAX as u64 + 1, 4.0), u64::MAX),
        Err(Dropped::Time)
    );
    let max = i64::MAX as u64;
    assert!(admit(&mut gate, WS, &fix(max, 4.0), 0).is_ok());
    // Far behind the last fix: a restart, not an overflow.
    assert!(admit(&mut gate, WS, &fix(0, 4.0), 200).is_ok());
    assert!(admit(&mut gate, WS, &fix(max, 4.0), u64::MAX / 2).is_ok());
    assert_eq!(
        admit(&mut gate, WS, &fix(max, 4.0), u64::MAX / 2 + 200),
        Err(Dropped::Stale)
    );
    assert_eq!(gate.counters.dropped_time, 2);
}

#[test]
fn observe_judges_every_follow_target_frame_in_a_write() {
    let (link, mut rx) = FollowBeaconLink::new();
    let hb = MavMessage::HEARTBEAT(HEARTBEAT_DATA::DEFAULT);
    let mut data = frame(&hb);
    data.extend(frame(&MavMessage::FOLLOW_TARGET(fix(2_000, 3.0))));
    link.observe_at(WS, &data, 1_000, 42);
    let b = rx.try_recv().unwrap();
    assert_eq!((b.stamp_ns, b.horiz_acc_m), (42, 3.0));

    // A truncated frame is ignored.
    let late = frame(&MavMessage::FOLLOW_TARGET(fix(3_000, 3.0)));
    link.observe_at(WS, &late[..late.len() - 3], 3_000, 44);
    assert!(rx.try_recv().is_err());

    let status = link.status();
    assert_eq!(status["published"], 1);
    assert_eq!(status["exported"], 0);
    assert_eq!(status["export_connected"], false);
}

#[tokio::test]
async fn the_export_publishes_one_line_per_beacon_on_the_event_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("_events.sock");
    let stats = Arc::new(ExportStats::default());
    let (tx, rx) = mpsc::channel(4);
    let task = tokio::spawn(export::run(path.clone(), rx, stats.clone()));

    // No socket yet: the beacon is dropped, not queued.
    let mut gate = BeaconGate::default();
    tx.send(admit(&mut gate, WS, &fix(1_000, 4.0), 1_000).unwrap())
        .await
        .unwrap();
    for _ in 0..100 {
        if stats.dropped.load(Ordering::Relaxed) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(stats.dropped.load(Ordering::Relaxed), 1);

    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::time::sleep(export::RETRY).await;
    let beacon = admit(&mut gate, WS, &fix(1_200, 4.0), 1_200).unwrap();
    tx.send(beacon.clone()).await.unwrap();
    let (stream, _) = tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(stream).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(2), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["v"], 1);
    assert_eq!(v["op"], "publish");
    assert_eq!(v["topic"], FOLLOW_BEACON_TOPIC);
    let got: FollowBeacon = serde_json::from_value(v["payload"].clone()).unwrap();
    assert_eq!(got, beacon);
    assert_eq!(stats.exported.load(Ordering::Relaxed), 1);

    drop(tx);
    task.await.unwrap();
}
