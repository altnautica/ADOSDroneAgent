//! End to end against a temp sensor-bus broker: a UDP receiver source and the
//! PPS reader loop.

use std::io;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ados_gnss::config::{GnssConfig, ReceiverCfg, Source};
use ados_gnss::pps::{Edge, PpsSource};
use ados_gnss::service::{Service, Shared};
use ados_gnss::sources::{bind_udp, serve_udp, Ingest};
use ados_positioning::base::{BaseMode, BaseState};
use ados_positioning::receiver::Fix;
use ados_positioning::stream::ubx_frame;
use ados_positioning::valset::{self, RxPort};
use ados_protocol::shutdown::Shutdown;
use ados_sensorbus::broker::{self, BrokerStats};
use ados_sensorbus::{v1, Kind, Origin, Publisher, Subscriber, SyncObs, SyncSource};
use tokio::net::UdpSocket;

fn shared(base: BaseMode) -> Shared {
    let cfg = GnssConfig {
        receivers: vec![ReceiverCfg {
            id: "gnss0".into(),
            source: Source::parse("udp:7800").unwrap(),
            model: None,
            port: RxPort::Uart1,
        }],
        pps_device: None,
        base,
        survey_min_dur_s: 300,
        survey_accuracy_m: 2.0,
        heading_enabled: false,
    };
    Service::new(&cfg, 0).shared()
}

const SURVEY: BaseMode = BaseMode::Survey {
    min_dur_s: 30,
    accuracy_m: 1.0,
};

/// A NAV-SVIN reporting a running survey of `dur_s` seconds.
fn nav_svin_active(dur_s: u32) -> Vec<u8> {
    let mut p = vec![0u8; 40];
    p[8..12].copy_from_slice(&dur_s.to_le_bytes());
    p[28..32].copy_from_slice(&25_000u32.to_le_bytes());
    p[37] = 1;
    ubx_frame(0x01, 0x3B, &p)
}

/// The next datagram the fake receiver gets.
async fn recv(client: &UdpSocket) -> Vec<u8> {
    let mut buf = vec![0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buf))
        .await
        .expect("a datagram")
        .unwrap();
    buf.truncate(n);
    buf
}

/// Serve a UDP receiver on an ephemeral port; returns the port.
async fn serve(shared: &Shared, bus: &std::path::Path, peer: Option<std::net::SocketAddr>) -> u16 {
    let publisher = Publisher::spawn(bus.to_path_buf(), Origin::Gnss);
    let sock = bind_udp(0).await.unwrap();
    let port = sock.local_addr().unwrap().port();
    let mut ingest = Ingest::new(shared.clone(), 0, publisher, true);
    tokio::spawn(async move { serve_udp(&mut ingest, sock, peer).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    port
}

/// A NAV-PVT with a 3D fix at a fixed position.
fn nav_pvt() -> Vec<u8> {
    let mut p = vec![0u8; 92];
    p[20] = 3;
    p[21] = 0x01;
    p[23] = 11;
    p[24..28].copy_from_slice(&775_946_000i32.to_le_bytes());
    p[28..32].copy_from_slice(&129_716_000i32.to_le_bytes());
    p[32..36].copy_from_slice(&920_400i32.to_le_bytes());
    p[36..40].copy_from_slice(&1_006_100i32.to_le_bytes());
    p[76..78].copy_from_slice(&90u16.to_le_bytes());
    ubx_frame(0x01, 0x07, &p)
}

async fn broker() -> (tempfile::TempDir, std::path::PathBuf, Arc<BrokerStats>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sensors.sock");
    let listener = ados_protocol::ipc::bind_command_socket(&path, 0o660).unwrap();
    let stats = Arc::new(BrokerStats::default());
    tokio::spawn(broker::serve(listener, None, stats.clone()));
    (dir, path, stats)
}

#[tokio::test]
async fn udp_datagram_updates_state_and_publishes_gnss() {
    let (_dir, bus, _stats) = broker().await;
    let mut sub = Subscriber::connect(&bus, &[Kind::Gnss]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let shared = shared(BaseMode::Off);
    let port = serve(&shared, &bus, None).await;

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let pvt = nav_pvt();
    client.send_to(&pvt, ("127.0.0.1", port)).await.unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.kind, Kind::Gnss);
    assert_eq!(frame.src, "gnss0");
    assert_eq!(frame.origin, Origin::Gnss);
    let raw: v1::GnssRaw = frame.decode_proto().unwrap();
    assert_eq!(raw.bytes, pvt);
    assert!(raw.t_ns > 0);

    let status = shared.lock().status(ados_sensorbus::time::mono_ns(), 0);
    let r = &status.receivers[0];
    assert_eq!(r.fix, Fix::ThreeD);
    assert_eq!(r.sats, 11);
    assert_eq!(r.source, "udp:7800");
    assert!((r.lat_deg.unwrap() - 12.9716).abs() < 1e-9);

    // The first datagram revealed the receiver: it gets its configuration.
    assert_eq!(recv(&client).await, valset::poll_mon_ver());
    assert_eq!(
        recv(&client).await,
        valset::nav_outputs(RxPort::Uart1, true, false)
    );
    assert_eq!(recv(&client).await, valset::off(RxPort::Uart1));
    // Later commands go back to the datagram sender.
    shared
        .lock()
        .set_base(SURVEY, ados_sensorbus::time::mono_ns())
        .unwrap();
    assert_eq!(
        recv(&client).await,
        valset::survey_in(RxPort::Uart1, 30, 1.0)
    );
}

#[tokio::test]
async fn udp_base_receiver_reaches_surveying_after_first_datagram() {
    let (_dir, bus, _stats) = broker().await;
    let shared = shared(SURVEY);
    let port = serve(&shared, &bus, None).await;
    // No receiver has spoken yet: nothing commanded, nothing claimed.
    assert_eq!(shared.lock().base_state(), BaseState::Off);

    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(&nav_pvt(), ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(recv(&client).await, valset::poll_mon_ver());
    assert_eq!(
        recv(&client).await,
        valset::nav_outputs(RxPort::Uart1, true, false)
    );
    assert_eq!(
        recv(&client).await,
        valset::survey_in(RxPort::Uart1, 30, 1.0)
    );
    assert_eq!(shared.lock().base_state(), BaseState::Surveying);

    // The receiver confirms the survey; the confirmation timeout no longer applies.
    client
        .send_to(&nav_svin_active(12), ("127.0.0.1", port))
        .await
        .unwrap();
    for _ in 0..100 {
        if shared.lock().base_out(0, 0).base.elapsed_s == 12 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let now = ados_sensorbus::time::mono_ns();
    shared.lock().tick(now + 60_000_000_000);
    let base = shared.lock().base_out(now, 0).base;
    assert_eq!((base.state, base.elapsed_s), (BaseState::Surveying, 12));
    assert_eq!(base.accuracy_m, Some(2.5));
}

#[tokio::test]
async fn a_new_udp_sender_port_does_not_restart_the_survey() {
    let (_dir, bus, _stats) = broker().await;
    let shared = shared(SURVEY);
    let port = serve(&shared, &bus, None).await;
    let first = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    first
        .send_to(&nav_pvt(), ("127.0.0.1", port))
        .await
        .unwrap();
    for _ in 0..3 {
        recv(&first).await;
    }
    first
        .send_to(&nav_svin_active(40), ("127.0.0.1", port))
        .await
        .unwrap();
    for _ in 0..100 {
        if shared.lock().base_out(0, 0).base.elapsed_s == 40 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The bridge comes back on another source port mid-survey.
    let second = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    second
        .send_to(&nav_svin_active(41), ("127.0.0.1", port))
        .await
        .unwrap();
    assert_eq!(recv(&second).await, valset::poll_mon_ver());
    assert_eq!(
        recv(&second).await,
        valset::nav_outputs(RxPort::Uart1, true, false)
    );
    // No CFG-TMODE rewrite follows.
    let mut buf = [0u8; 2048];
    let more = tokio::time::timeout(Duration::from_millis(300), second.recv_from(&mut buf)).await;
    assert!(more.is_err(), "the survey command was re-sent");
    let base = shared.lock().base_out(0, 0).base;
    assert_eq!((base.state, base.elapsed_s), (BaseState::Surveying, 41));
}

#[tokio::test]
async fn a_configured_udp_peer_is_configured_before_it_sends() {
    let (_dir, bus, _stats) = broker().await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let shared = shared(BaseMode::Off);
    serve(&shared, &bus, Some(client.local_addr().unwrap())).await;
    assert_eq!(recv(&client).await, valset::poll_mon_ver());
    assert_eq!(
        recv(&client).await,
        valset::nav_outputs(RxPort::Uart1, true, false)
    );
    assert_eq!(recv(&client).await, valset::off(RxPort::Uart1));
}

struct OnePulse(Option<Edge>, Shutdown);

impl PpsSource for OnePulse {
    fn fetch(&mut self, timeout: Duration) -> io::Result<Option<Edge>> {
        match self.0.take() {
            Some(e) => Ok(Some(e)),
            None => {
                self.1.trigger();
                std::thread::sleep(timeout.min(Duration::from_millis(10)));
                Ok(None)
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pps_pulse_publishes_a_sync_pair() {
    let (_dir, bus, stats) = broker().await;
    let mut sub = Subscriber::connect(&bus, &[Kind::Sync]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let publisher = Publisher::spawn(bus.clone(), Origin::Gnss);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let shared = shared(BaseMode::Off);
    let utc = 1_760_955_438_000_000_000i64;
    shared.lock().pairer.on_tim_tp(10_000_000_000, utc);
    let edge = Edge {
        realtime_ns: 1_000_010_900_000_000,
        rt_minus_mono_ns: 1_000_000_000_000_000,
    };
    let shutdown = Shutdown::new();
    let src: Box<dyn PpsSource> = Box::new(OnePulse(Some(edge), shutdown.clone()));
    let (s2, p2) = (shared.clone(), publisher.clone());
    tokio::task::spawn_blocking(move || {
        ados_gnss::daemon::pps_loop(src, &s2, &p2, "gnss0", &shutdown)
    })
    .await
    .unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((frame.kind, frame.src.as_str()), (Kind::Sync, "gnss0"));
    let obs: SyncObs = frame.decode_msgpack().unwrap();
    assert_eq!(obs.source, SyncSource::GnssPps);
    assert_eq!(obs.host_ns, 10_900_000_000);
    assert_eq!(obs.source_ns, utc);
    assert_eq!(obs.rtt_ns, 0);
    assert!(shared.lock().pairer.locked(11_000_000_000));
    assert_eq!(stats.frames_in.load(Ordering::Relaxed), 1);
}
