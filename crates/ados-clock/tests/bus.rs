//! The broker on a temp socket, fed through `Publisher`: sync pairs lock the
//! hub clock and an IMU stream lands in the stream table.

use std::time::Duration;

use ados_clock::clock::Timebase;
use ados_clock::{spawn_bus, Engine};
use ados_sensorbus::time::mono_ns;
use ados_sensorbus::{v1, Kind, Origin, Publisher, SyncObs, SyncSource};

#[tokio::test]
async fn sync_and_imu_frames_reach_the_clock_and_the_stream_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run/sensors.sock");
    // A stale file where the socket goes is replaced.
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"stale").unwrap();
    let (mut tap, _stats) = spawn_bus(&path).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o660);

    let hub = Publisher::spawn(path.clone(), Origin::Hub);
    // Let the publisher connect before the frames are queued.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let now = mono_ns();
    for i in 0..8u64 {
        let host_ns = now - (7 - i) * 100_000_000;
        let obs = SyncObs {
            source: SyncSource::Hub,
            host_ns,
            source_ns: host_ns as i64 + 12_000,
            rtt_ns: 0,
            sim_time: false,
        };
        assert!(hub.msgpack(Kind::Sync, "hub", &obs));
    }
    let batch = v1::ImuBatch {
        samples: (0..10u64)
            .map(|k| v1::ImuSample {
                t_ns: now - 9_000_000 + k * 1_000_000,
                seq: k,
                gyro: vec![0.0; 3],
                accel: vec![0.0, 0.0, 9.81],
                temp_c: None,
            })
            .collect(),
    };
    assert!(hub.proto(Kind::Imu, "imu0", &batch));

    let mut engine = Engine::new(None, ados_clock::fit::LatencyFloor::Unknown);
    for _ in 0..9 {
        let frame = tokio::time::timeout(Duration::from_secs(5), tap.recv())
            .await
            .expect("frame within the deadline")
            .expect("tap open");
        engine.ingest(&frame, mono_ns());
    }

    let at = mono_ns();
    let clock = engine.clock.report(at);
    assert_eq!(clock.timebase, Timebase::HubFreerun);
    assert!(clock.locked);
    assert!(
        (0..1_000).contains(&clock.sigma_ns),
        "sigma {}",
        clock.sigma_ns
    );
    let hub_row = &clock.sources[1];
    assert_eq!(hub_row.id, "hub");
    assert!(hub_row.locked);
    assert!(
        (hub_row.offset_ns - 12_000).abs() < 100,
        "{}",
        hub_row.offset_ns
    );
    assert_eq!(hub_row.last_update_ns, now);
    let sample = engine.clock.sample(at);
    assert!(sample.locked);
    assert!((sample.t_ns as i64 - (at as i64 + 12_000)).abs() < 100);

    let sensors = engine.streams.report(at);
    assert_eq!(sensors.streams.len(), 1);
    let imu = &sensors.streams[0];
    assert_eq!(
        (imu.kind, imu.src.as_str(), imu.origin),
        ("imu", "imu0", "hub")
    );
    assert_eq!(imu.count, 10);
    assert_eq!(imu.last_t_ns, now);
    assert!(imu.last_rx_ns >= now);
}
