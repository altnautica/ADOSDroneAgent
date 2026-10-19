//! End to end over TCP: an in-test fake hub speaks protocol v1, a temp-socket
//! broker carries the bus, and a subscriber checks what the service publishes;
//! an `rtcm` frame published on the bus must reach the hub as `RtcmIn`.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::broker::{serve, BrokerStats};
use ados_sensorbus::{v1, BusFrame, Kind, Origin, Publisher, Subscriber, SyncObs, SyncSource};
use ados_sensorhub::config::{HubConfig, TransportSpec};
use ados_sensorhub::link::{run_link, LinkParams};
use ados_sensorhub::rig::Extrinsics;
use ados_sensorhub::session::Stop;
use ados_sensorhub::stream::{encode, StreamDecoder};
use ados_sensorhub_proto::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{mpsc, Notify};

const RTCM_LEN: usize = 1500;

fn hello() -> Packet<'static> {
    let mut board_name = [0u8; 16];
    board_name[..3].copy_from_slice(b"sim");
    Packet::Hello(Hello {
        proto_ver: PROTO_VER,
        fw_major: 0,
        fw_minor: 1,
        fw_patch: 0,
        board_id: BOARD_ID_SIM,
        caps: caps::IMU0 | caps::TRIGGER_OUT | caps::GNSS_PASSTHROUGH | caps::RTCM_IN,
        uid: [0; 12],
        board_name,
    })
}

const SAMPLES: [ImuSample; 4] = [ImuSample {
    gyro: [0, 0, 1000],
    accel: [0, 0, 2048],
    temp: 3000,
}; 4];

fn imu(t0_ns: u64, seq0: u32) -> Packet<'static> {
    Packet::ImuBatch(ImuBatch {
        t0_ns,
        dt_ns: 1_000_000,
        imu_id: 0,
        flags: 0,
        seq0,
        samples: ImuSamples::Typed(&SAMPLES),
    })
}

/// What the fake hub saw from the service.
enum HubEvent {
    Config(Config),
    Rtcm { port: u8, bytes: Vec<u8> },
}

/// Accept one connection, wait for `Config`, stream a scripted session, then
/// report every `RtcmIn` packet.
async fn fake_hub(listener: TcpListener, events: mpsc::UnboundedSender<HubEvent>) {
    let (mut sock, _) = listener.accept().await.unwrap();
    let mut dec = StreamDecoder::new();
    let mut buf = [0u8; 2048];
    let mut got_config = false;
    let mut scripted = false;
    loop {
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        dec.push(&buf[..n], |p| match p {
            Packet::Config(c) => {
                got_config = true;
                let _ = events.send(HubEvent::Config(c));
            }
            Packet::RtcmIn(r) => {
                let _ = events.send(HubEvent::Rtcm {
                    port: r.port,
                    bytes: r.bytes.to_vec(),
                });
            }
            _ => {}
        });
        if got_config && !scripted {
            scripted = true;
            let mut wire = Vec::new();
            for p in [
                hello(),
                imu(10_000_000, 0),
                Packet::Trigger(Trigger {
                    seq: 5,
                    cam_mask: 0b11,
                    flags: 0,
                    t_start_ns: 11_000_000,
                    t_mid_ns: 13_000_000,
                    exposure_ns: 4_000_000,
                }),
                Packet::GnssRaw(GnssRaw {
                    t_ns: 12_000_000,
                    port: 2,
                    bytes: b"\xb5\x62\x01\x07",
                }),
            ] {
                wire.extend(encode(&p).unwrap());
            }
            sock.write_all(&wire).await.unwrap();
            // A packet in the next host second closes the first envelope
            // bucket, which publishes its sync pair.
            tokio::time::sleep(Duration::from_millis(1100)).await;
            sock.write_all(&encode(&imu(1_110_000_000, 4)).unwrap())
                .await
                .unwrap();
        }
    }
}

async fn next_event(rx: &mut mpsc::UnboundedReceiver<HubEvent>) -> Option<HubEvent> {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("fake hub event within 5 s")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fake_hub_session_reaches_the_bus_and_rtcm_reaches_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("sensors.sock");
    let sidecar = dir.path().join("sensorhub.json");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(serve(listener, None, Arc::new(BrokerStats::default())));

    let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = hub.local_addr().unwrap().to_string();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(fake_hub(hub, ev_tx));

    let mut sub = Subscriber::connect(&socket, &[Kind::Imu, Kind::Trigger, Kind::Gnss, Kind::Sync])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let shutdown = Shutdown::new();
    let params = LinkParams {
        cfg: HubConfig {
            transport: TransportSpec::Tcp(addr.clone()),
            ..HubConfig::default()
        },
        tty_root: dir.path().join("tty"),
        ext: Extrinsics::default(),
        socket: socket.clone(),
        sidecar: sidecar.clone(),
    };
    let link = tokio::spawn(run_link(params, shutdown.clone(), Arc::new(Notify::new())));

    let Some(HubEvent::Config(cfg)) = next_event(&mut ev_rx).await else {
        panic!("the service must send Config first");
    };
    assert_eq!(
        (cfg.imu_odr_hz, cfg.trigger_hz, cfg.exposure_us),
        (1000, 20, 4000)
    );

    let mut frames: Vec<BusFrame> = Vec::new();
    let has = |frames: &[BusFrame], kind: Kind| frames.iter().any(|f| f.kind == kind);
    while !(has(&frames, Kind::Sync) && frames.iter().filter(|f| f.kind == Kind::Imu).count() >= 2)
    {
        let f = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("bus frames within 5 s")
            .unwrap();
        frames.push(f);
    }
    assert!(frames.iter().all(|f| f.origin == Origin::Hub));

    let first_imu = frames.iter().find(|f| f.kind == Kind::Imu).unwrap();
    assert_eq!(first_imu.src, "imu0");
    let batch: v1::ImuBatch = first_imu.decode_proto().unwrap();
    assert_eq!(batch.samples.len(), 4);
    assert!((batch.samples[0].gyro[2] - 1000.0 * GYRO_LSB_RAD_S).abs() < 1e-12);
    assert!((batch.samples[0].accel[2] - 9.80665).abs() < 1e-9);
    assert_eq!(batch.samples[3].t_ns - batch.samples[0].t_ns, 3_000_000);

    let cams: Vec<&str> = frames
        .iter()
        .filter(|f| f.kind == Kind::Trigger)
        .map(|f| f.src.as_str())
        .collect();
    assert_eq!(cams, ["cam0", "cam1"]);
    let rec: v1::TriggerRecord = frames
        .iter()
        .find(|f| f.kind == Kind::Trigger)
        .unwrap()
        .decode_proto()
        .unwrap();
    assert_eq!((rec.seq, rec.exposure_ns), (5, 4_000_000));

    let gnss = frames.iter().find(|f| f.kind == Kind::Gnss).unwrap();
    assert_eq!(gnss.src, "gnss0");
    let raw: v1::GnssRaw = gnss.decode_proto().unwrap();
    assert_eq!(raw.bytes, b"\xb5\x62\x01\x07");

    let sync: SyncObs = frames
        .iter()
        .find(|f| f.kind == Kind::Sync)
        .unwrap()
        .decode_msgpack()
        .unwrap();
    assert_eq!((sync.source, sync.rtt_ns), (SyncSource::Hub, 0));
    assert!(sync.source_ns >= 10_000_000 && sync.host_ns > 0);

    // RTCM published by another bus client reaches the hub, chunked to the
    // packet limit and addressed to the GNSS port.
    let rtcm: Vec<u8> = (0..RTCM_LEN).map(|i| (i % 251) as u8).collect();
    let rest = Publisher::spawn(socket.clone(), Origin::Host);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rest.raw(Kind::Rtcm, "rest", rtcm.clone()));
    let mut got = Vec::new();
    let mut chunks = 0;
    while got.len() < RTCM_LEN {
        match next_event(&mut ev_rx).await {
            Some(HubEvent::Rtcm { port, bytes }) => {
                assert_eq!(port, 2);
                assert!(bytes.len() <= RTCM_MAX_BYTES);
                chunks += 1;
                got.extend(bytes);
            }
            Some(HubEvent::Config(_)) => {}
            None => panic!("fake hub ended"),
        }
    }
    assert_eq!(got, rtcm);
    assert_eq!(chunks, 2);

    // The sidecar reports the live link.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
    assert_eq!(body["version"], 1);
    assert_eq!(body["connected"], true);
    assert_eq!(body["transport"], format!("tcp:{addr}"));
    assert_eq!(body["fw"], "ados-sensorhub 0.1.0");
    assert_eq!(body["board"], "sim");
    assert_eq!(body["proto_ver"], 1);
    assert_eq!(body["crc_errors"], 0);
    assert_eq!(body["dropped"], 0);

    shutdown.trigger();
    let why = tokio::time::timeout(Duration::from_secs(5), link)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(why, Stop::Shutdown);
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
    assert_eq!(body["connected"], false);
}
