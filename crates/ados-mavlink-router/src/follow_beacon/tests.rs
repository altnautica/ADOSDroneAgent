use super::gate::{Dropped, Writer};
use super::*;
use ados_protocol::follow_beacon::{EST_POS, EST_VEL, FOLLOW_BEACON_TOPIC, SOURCE_GCS_PHONE};
use ados_protocol::mavlink::ardupilotmega::{FOLLOW_TARGET_DATA, HEARTBEAT_DATA};
use ados_protocol::mavlink::{serialize_v2, MavHeader};
use tokio::io::AsyncBufReadExt;

const PHONE: Writer = Writer {
    origin: ClientOrigin::Trusted,
    injector: false,
    sysid: 255,
    compid: 190,
};

fn fix(ts_ms: u64, acc_m: f32) -> FOLLOW_TARGET_DATA {
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

fn frame(msg: &MavMessage) -> Vec<u8> {
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

#[test]
fn an_authorised_fix_becomes_the_documented_beacon() {
    let mut gate = BeaconGate::default();
    let b = gate
        .admit(PHONE, &fix(10_000, 4.0), 1_000, 7_000_000_000)
        .unwrap();
    assert!((b.lat - 47.397_742).abs() < 1e-9 && (b.lon - 8.545_594).abs() < 1e-9);
    assert_eq!(b.alt_m, Some(488.5));
    assert_eq!(b.vel_ned, Some([1.0, -0.5, 0.0]));
    assert_eq!((b.horiz_acc_m, b.vert_acc_m), (4.0, Some(6.0)));
    assert_eq!(
        b.stamp_ns, 7_000_000_000,
        "stamped with ADOS time at receipt"
    );
    assert_eq!(b.source, SOURCE_GCS_PHONE);
    assert_eq!((b.sysid, b.compid), (255, 190));

    // The relay is an authorised ground link too; unknowns read as unknown.
    let relayed = Writer {
        origin: ClientOrigin::Relayed,
        ..PHONE
    };
    let mut m = fix(10_200, 4.0);
    m.alt = f32::NAN;
    m.position_cov[1] = 0.0;
    m.est_capabilities = EST_POS;
    let b = gate.admit(relayed, &m, 1_200, 7_200_000_000).unwrap();
    assert_eq!((b.alt_m, b.vel_ned, b.vert_acc_m), (None, None, None));
    assert_eq!(gate.counters.published, 2);
}

#[test]
fn unauthenticated_peers_and_autonomous_writers_are_refused() {
    let mut gate = BeaconGate::default();
    let anon = Writer {
        origin: ClientOrigin::Unauthenticated,
        ..PHONE
    };
    assert_eq!(gate.admit(anon, &fix(1, 4.0), 0, 0), Err(Dropped::Origin));
    let plugin = Writer {
        injector: true,
        ..PHONE
    };
    assert_eq!(gate.admit(plugin, &fix(2, 4.0), 0, 0), Err(Dropped::Origin));
    assert_eq!(gate.counters.dropped_origin, 2);
    assert_eq!(gate.counters.published, 0);
}

#[test]
fn a_fix_without_position_or_with_poor_accuracy_is_dropped() {
    let mut gate = BeaconGate::default();
    let mut no_pos = fix(1, 4.0);
    no_pos.est_capabilities = EST_VEL;
    assert_eq!(gate.admit(PHONE, &no_pos, 0, 0), Err(Dropped::Position));
    let mut null_island = fix(1, 4.0);
    (null_island.lat, null_island.lon) = (0, 0);
    assert_eq!(
        gate.admit(PHONE, &null_island, 0, 0),
        Err(Dropped::Position)
    );
    for acc in [30.5, 0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(
            gate.admit(PHONE, &fix(1, acc), 0, 0),
            Err(Dropped::Accuracy),
            "{acc}"
        );
    }
    assert!(
        gate.admit(PHONE, &fix(1, 30.0), 0, 0).is_ok(),
        "30 m is the limit"
    );
    assert_eq!(gate.counters.dropped_accuracy, 5);
    assert_eq!(gate.counters.dropped_position, 2);
}

#[test]
fn the_rate_is_ten_hertz() {
    let mut gate = BeaconGate::default();
    assert!(gate.admit(PHONE, &fix(1_000, 4.0), 5_000, 0).is_ok());
    assert_eq!(
        gate.admit(PHONE, &fix(1_050, 4.0), 5_050, 0),
        Err(Dropped::Rate)
    );
    assert!(gate.admit(PHONE, &fix(1_100, 4.0), 5_100, 0).is_ok());
    // The cap holds across senders.
    let other = Writer {
        sysid: 254,
        ..PHONE
    };
    assert_eq!(
        gate.admit(other, &fix(9, 4.0), 5_150, 0),
        Err(Dropped::Rate)
    );
    assert_eq!(gate.counters.dropped_rate, 2);
}

#[test]
fn a_late_repeated_or_reordered_fix_is_stale() {
    let mut gate = BeaconGate::default();
    // The first fix sets the sender's clock offset to wall: -10 000 ms.
    assert!(gate.admit(PHONE, &fix(20_000, 4.0), 10_000, 0).is_ok());
    // Arrives 2.5 s later than its least-delayed sibling.
    assert_eq!(
        gate.admit(PHONE, &fix(20_200, 4.0), 12_700, 0),
        Err(Dropped::Stale)
    );
    // 1.9 s late is still inside the window.
    assert!(gate.admit(PHONE, &fix(22_000, 4.0), 13_900, 0).is_ok());
    // A repeat and a reordered fix.
    assert_eq!(
        gate.admit(PHONE, &fix(22_000, 4.0), 14_000, 0),
        Err(Dropped::Stale)
    );
    assert_eq!(
        gate.admit(PHONE, &fix(21_900, 4.0), 14_100, 0),
        Err(Dropped::Stale)
    );
    // A restarted sender (its clock far behind its last fix) starts a new session.
    assert!(gate.admit(PHONE, &fix(50, 4.0), 14_200, 0).is_ok());
    // So does one silent past the session gap, whatever its offset now.
    assert!(gate.admit(PHONE, &fix(60_000, 4.0), 50_000, 0).is_ok());
    assert_eq!(gate.counters.dropped_stale, 3);
}

#[test]
fn observe_judges_every_follow_target_frame_in_a_write() {
    let (link, mut rx) = FollowBeaconLink::new();
    let hb = MavMessage::HEARTBEAT(HEARTBEAT_DATA::DEFAULT);
    let mut data = frame(&hb);
    data.extend(frame(&MavMessage::FOLLOW_TARGET(fix(2_000, 3.0))));
    link.observe_at(&data, ClientOrigin::Trusted, false, 1_000, 42);
    let b = rx.try_recv().unwrap();
    assert_eq!((b.stamp_ns, b.horiz_acc_m), (42, 3.0));

    // An injector's frame never becomes a beacon.
    let late = frame(&MavMessage::FOLLOW_TARGET(fix(3_000, 3.0)));
    link.observe_at(&late, ClientOrigin::Trusted, true, 2_000, 43);
    assert!(rx.try_recv().is_err());
    // A truncated frame is ignored.
    link.observe_at(
        &late[..late.len() - 3],
        ClientOrigin::Trusted,
        false,
        3_000,
        44,
    );
    assert!(rx.try_recv().is_err());

    let status = link.status();
    assert_eq!(status["published"], 1);
    assert_eq!(status["dropped_origin"], 1);
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
    tx.send(gate.admit(PHONE, &fix(1_000, 4.0), 1_000, 1).unwrap())
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
    let beacon = gate.admit(PHONE, &fix(1_200, 4.0), 1_200, 2).unwrap();
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
