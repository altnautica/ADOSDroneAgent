//! The broker `ados-clock` runs on `sensors.sock`.
//!
//! The socket is a reader socket
//! ([`ados_protocol::ipc::bind_reader_socket`] with
//! [`ados_protocol::ipc::SENSORS_GROUP`]): root, the broker's own uid, members
//! of the operator group and members of the sensors reader group may connect.
//! A client receives nothing until it sends a [`Kind::Subscribe`] frame; from
//! then on it receives every frame of those kinds published by any *other*
//! client (a later subscribe replaces the set). What a client may publish
//! follows its [`PeerClass`]: a service peer anything; an operator sensor data
//! but not the kinds that steer other services — `sync` (moves the session
//! clock) and `rtcm` (injected into a GNSS receiver); a reader (a sandboxed
//! plugin holding `sensor.stream.read`) nothing. Refused frames are dropped
//! and counted. Each subscriber has a bounded queue; a slow subscriber loses
//! frames rather than stalling the publishers. Every published frame is also
//! handed to the optional `tap` (the clock's own consumer), which is never
//! blocked either.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::ipc::{OperatorListener, PeerClass};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, RwLock};

use crate::client::read_frame;
use crate::frame::{encode, BusFrame, Kind};

const SUB_QUEUE: usize = 1024;
/// Pause after a failed `accept()` (fd exhaustion, ...) before the next try.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

struct Sub {
    kinds: Vec<Kind>,
    tx: mpsc::Sender<Arc<Vec<u8>>>,
}

/// Broker counters.
#[derive(Debug, Default)]
pub struct BrokerStats {
    pub frames_in: AtomicU64,
    pub frames_out: AtomicU64,
    pub dropped: AtomicU64,
    pub bad_frames: AtomicU64,
    /// Frames a peer was not allowed to publish ([`may_publish`]).
    pub refused: AtomicU64,
    pub clients: AtomicU64,
}

/// Whether a peer of this class may publish `kind`. `sync` and `rtcm` steer
/// other services, so only service peers may publish them; a reader may only
/// subscribe.
pub fn may_publish(kind: Kind, peer: PeerClass) -> bool {
    match peer {
        PeerClass::Service => true,
        PeerClass::Operator => !matches!(kind, Kind::Sync | Kind::Rtcm),
        PeerClass::Reader => false,
    }
}

/// Accept admitted clients forever. `tap` receives a copy of every published
/// frame. Bind the listener with [`ados_protocol::ipc::bind_reader_socket`]
/// (a [`ados_protocol::ipc::bind_command_socket`] listener admits no readers).
pub async fn serve(
    listener: OperatorListener,
    tap: Option<mpsc::Sender<BusFrame>>,
    stats: Arc<BrokerStats>,
) {
    let subs: Arc<RwLock<HashMap<u64, Sub>>> = Arc::new(RwLock::new(HashMap::new()));
    let mut next_id = 0u64;
    loop {
        let (stream, class) = match listener.accept_classified().await {
            Ok(accepted) => accepted,
            Err(e) => {
                tracing::warn!(error = %e, "sensor bus accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        next_id += 1;
        tokio::spawn(client(
            next_id,
            stream,
            class,
            subs.clone(),
            tap.clone(),
            stats.clone(),
        ));
    }
}

async fn client(
    id: u64,
    stream: UnixStream,
    peer: PeerClass,
    subs: Arc<RwLock<HashMap<u64, Sub>>>,
    tap: Option<mpsc::Sender<BusFrame>>,
    stats: Arc<BrokerStats>,
) {
    stats.clients.fetch_add(1, Ordering::Relaxed);
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Arc<Vec<u8>>>(SUB_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if wr.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let mut buf = Vec::new();
    loop {
        let frame = match read_frame(&mut rd, &mut buf).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                stats.bad_frames.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            Err(_) => break,
        };
        if frame.kind == Kind::Subscribe {
            let kinds: Vec<Kind> = frame.decode_msgpack().unwrap_or_default();
            subs.write().await.insert(
                id,
                Sub {
                    kinds,
                    tx: tx.clone(),
                },
            );
            continue;
        }
        if !may_publish(frame.kind, peer) {
            if stats.refused.fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::warn!(
                    kind = frame.kind.as_str(),
                    peer = ?peer,
                    "sensor bus refused a frame this peer may not publish"
                );
            }
            continue;
        }
        stats.frames_in.fetch_add(1, Ordering::Relaxed);
        if let Ok(bytes) = encode(&frame) {
            let bytes = Arc::new(bytes);
            for (sid, sub) in subs.read().await.iter() {
                if *sid == id || !sub.kinds.contains(&frame.kind) {
                    continue;
                }
                if sub.tx.try_send(bytes.clone()).is_ok() {
                    stats.frames_out.fetch_add(1, Ordering::Relaxed);
                } else {
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if let Some(tap) = &tap {
            if tap.try_send(frame).is_err() {
                stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    subs.write().await.remove(&id);
    drop(tx);
    writer.abort();
    stats.clients.fetch_sub(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{v1, Origin, Publisher, Subscriber, SyncObs, SyncSource};
    use ados_protocol::ipc::{bind_reader_socket, classify_peer, SENSORS_GROUP};

    #[tokio::test]
    async fn frames_reach_matching_subscribers_and_the_tap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sensors.sock");
        let listener = bind_reader_socket(&path, 0o660, SENSORS_GROUP).unwrap();
        let (tap_tx, mut tap_rx) = mpsc::channel(16);
        let stats = Arc::new(BrokerStats::default());
        tokio::spawn(serve(listener, Some(tap_tx), stats.clone()));

        let mut imu_sub = Subscriber::connect(&path, &[Kind::Imu]).await.unwrap();
        let mut rtcm_sub = Subscriber::connect(&path, &[Kind::Rtcm]).await.unwrap();
        // Let the broker register both subscriptions.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The test runs as the broker's own uid: a service peer.
        let publ = Publisher::spawn(path.clone(), Origin::Hub);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let batch = v1::ImuBatch { samples: vec![] };
        assert!(publ.proto(Kind::Imu, "imu0", &batch));
        assert!(publ.raw(Kind::Rtcm, "rest", vec![0xD3]));

        let got = tokio::time::timeout(Duration::from_secs(2), imu_sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.kind, Kind::Imu);
        assert_eq!(got.src, "imu0");
        let got = tokio::time::timeout(Duration::from_secs(2), rtcm_sub.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, vec![0xD3]);
        let tapped = tap_rx.recv().await.unwrap();
        assert_eq!(tapped.kind, Kind::Imu);
        assert_eq!(stats.frames_in.load(Ordering::Relaxed), 2);
        assert_eq!(stats.refused.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn what_each_peer_class_may_publish() {
        for kind in [Kind::Sync, Kind::Rtcm] {
            assert!(may_publish(kind, PeerClass::Service));
            assert!(!may_publish(kind, PeerClass::Operator));
        }
        for kind in [
            Kind::Imu,
            Kind::Gnss,
            Kind::Trigger,
            Kind::Baro,
            Kind::Mag,
            Kind::Clock,
        ] {
            assert!(may_publish(kind, PeerClass::Operator), "{kind:?}");
        }
        for kind in [Kind::Imu, Kind::Gnss, Kind::Sync, Kind::Rtcm, Kind::Clock] {
            assert!(!may_publish(kind, PeerClass::Reader), "{kind:?}");
        }
    }

    /// Admission on the bus: an operator publishes data, a peer whose only
    /// relevant credential is the sensors group is a subscribe-only reader, and
    /// any other peer (a plugin without the grant) is refused.
    #[test]
    fn bus_admission_by_credentials() {
        const SERVER: u32 = 900;
        const OPERATOR: u32 = 988;
        const SENSORS: u32 = 977;
        const PLUGINS: u32 = 966;
        let admit = |uid: u32, gids: &[u32]| {
            classify_peer(uid, gids, SERVER, Some(OPERATOR), Some(SENSORS))
        };
        let operator = admit(1000, &[1000, OPERATOR]).unwrap();
        assert!(may_publish(Kind::Imu, operator));
        let plugin_reader = admit(61_001, &[PLUGINS, SENSORS]).unwrap();
        assert_eq!(plugin_reader, PeerClass::Reader);
        assert!(!may_publish(Kind::Imu, plugin_reader));
        assert_eq!(admit(61_002, &[PLUGINS]), None);
    }

    /// An operator connection's `sync` and `rtcm` frames are refused; its
    /// data frames still flow.
    #[tokio::test]
    async fn an_operator_peer_cannot_steer_the_clock_or_a_receiver() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (tap_tx, mut tap_rx) = mpsc::channel(16);
        let stats = Arc::new(BrokerStats::default());
        let subs = Arc::new(RwLock::new(HashMap::new()));
        let task = tokio::spawn(client(
            1,
            theirs,
            PeerClass::Operator,
            subs,
            Some(tap_tx),
            stats.clone(),
        ));

        let obs = SyncObs {
            source: SyncSource::Hub,
            host_ns: 1,
            source_ns: 2,
            rtt_ns: 0,
        };
        let frames = [
            BusFrame::msgpack(Kind::Sync, "hub", Origin::Host, &obs).unwrap(),
            BusFrame::new(Kind::Rtcm, "rest", Origin::Host, vec![0xD3]),
            BusFrame::proto(
                Kind::Imu,
                "imu0",
                Origin::Host,
                &v1::ImuBatch { samples: vec![] },
            ),
        ];
        let mut ours = ours;
        for f in &frames {
            ours.write_all(&encode(f).unwrap()).await.unwrap();
        }
        let tapped = tokio::time::timeout(Duration::from_secs(2), tap_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tapped.kind, Kind::Imu);
        assert_eq!(stats.refused.load(Ordering::Relaxed), 2);
        assert_eq!(stats.frames_in.load(Ordering::Relaxed), 1);
        drop(ours);
        task.await.unwrap();
    }

    /// A reader subscribes and receives what a service peer publishes; every
    /// frame it publishes itself is refused and reaches neither the tap nor
    /// another subscriber.
    #[tokio::test]
    async fn a_reader_receives_but_cannot_publish() {
        let stats = Arc::new(BrokerStats::default());
        let subs = Arc::new(RwLock::new(HashMap::new()));
        let (tap_tx, mut tap_rx) = mpsc::channel(16);
        let (mut reader, reader_end) = UnixStream::pair().unwrap();
        let (mut service, service_end) = UnixStream::pair().unwrap();
        let reader_task = tokio::spawn(client(
            1,
            reader_end,
            PeerClass::Reader,
            subs.clone(),
            Some(tap_tx.clone()),
            stats.clone(),
        ));
        let service_task = tokio::spawn(client(
            2,
            service_end,
            PeerClass::Service,
            subs.clone(),
            Some(tap_tx),
            stats.clone(),
        ));
        let subscribe = BusFrame::msgpack(Kind::Subscribe, "", Origin::Host, &[Kind::Imu]).unwrap();
        reader
            .write_all(&encode(&subscribe).unwrap())
            .await
            .unwrap();
        service
            .write_all(&encode(&subscribe).unwrap())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let imu = |src: &str| {
            BusFrame::proto(
                Kind::Imu,
                src,
                Origin::Host,
                &v1::ImuBatch { samples: vec![] },
            )
        };
        reader
            .write_all(&encode(&imu("forged")).unwrap())
            .await
            .unwrap();
        service
            .write_all(&encode(&imu("imu0")).unwrap())
            .await
            .unwrap();

        let mut rd = reader;
        let mut buf = Vec::new();
        let got = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut rd, &mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.src, "imu0");
        let tapped = tap_rx.recv().await.unwrap();
        assert_eq!(tapped.src, "imu0");
        assert_eq!(stats.refused.load(Ordering::Relaxed), 1);
        assert_eq!(stats.frames_in.load(Ordering::Relaxed), 1);
        // Nothing from the reader reached the service subscriber.
        let mut sbuf = Vec::new();
        assert!(tokio::time::timeout(
            Duration::from_millis(100),
            read_frame(&mut service, &mut sbuf)
        )
        .await
        .is_err());
        drop(rd);
        drop(service);
        reader_task.await.unwrap();
        service_task.await.unwrap();
    }
}
