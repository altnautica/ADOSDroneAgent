//! The broker `ados-clock` runs on `sensors.sock`.
//!
//! The socket is a command-plane socket ([`OperatorListener`]): root, the
//! broker's own uid and members of the operator group may connect. A client
//! receives nothing until it sends a [`Kind::Subscribe`] frame; from then on it
//! receives every frame of those kinds published by any *other* client (a
//! later subscribe replaces the set). Any admitted client may publish sensor
//! data, but the kinds that steer other services — `sync` (moves the session
//! clock) and `rtcm` (injected into a GNSS receiver) — are accepted only from
//! service peers (root or the broker's own uid); an operator's are dropped
//! and counted. Each subscriber has a bounded queue; a slow subscriber loses
//! frames rather than stalling the publishers. Every published frame is also
//! handed to the optional `tap` (the clock's own consumer), which is never
//! blocked either.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::ipc::OperatorListener;
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
    /// Frames of a service-only kind published by an operator peer.
    pub refused: AtomicU64,
    pub clients: AtomicU64,
}

/// Whether a peer of this class may publish `kind`. `sync` and `rtcm` steer
/// other services, so only service peers may publish them.
pub fn may_publish(kind: Kind, service_peer: bool) -> bool {
    service_peer || !matches!(kind, Kind::Sync | Kind::Rtcm)
}

/// Root or this process's own uid: an agent service rather than an operator.
fn is_service_peer(stream: &UnixStream) -> bool {
    let own = nix::unistd::geteuid().as_raw();
    stream
        .peer_cred()
        .is_ok_and(|c| c.uid() == 0 || c.uid() == own)
}

/// Accept admitted clients forever. `tap` receives a copy of every published
/// frame. Bind the listener with
/// [`ados_protocol::ipc::bind_command_socket`].
pub async fn serve(
    listener: OperatorListener,
    tap: Option<mpsc::Sender<BusFrame>>,
    stats: Arc<BrokerStats>,
) {
    let subs: Arc<RwLock<HashMap<u64, Sub>>> = Arc::new(RwLock::new(HashMap::new()));
    let mut next_id = 0u64;
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!(error = %e, "sensor bus accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        next_id += 1;
        let service = is_service_peer(&stream);
        tokio::spawn(client(
            next_id,
            stream,
            service,
            subs.clone(),
            tap.clone(),
            stats.clone(),
        ));
    }
}

async fn client(
    id: u64,
    stream: UnixStream,
    service_peer: bool,
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
        if !may_publish(frame.kind, service_peer) {
            if stats.refused.fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::warn!(
                    kind = frame.kind.as_str(),
                    "sensor bus refused a service-only frame from an operator peer"
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
    use ados_protocol::ipc::bind_command_socket;

    #[tokio::test]
    async fn frames_reach_matching_subscribers_and_the_tap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sensors.sock");
        let listener = bind_command_socket(&path, 0o660).unwrap();
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
    fn only_service_peers_publish_sync_and_rtcm() {
        for kind in [Kind::Sync, Kind::Rtcm] {
            assert!(may_publish(kind, true));
            assert!(!may_publish(kind, false));
        }
        for kind in [
            Kind::Imu,
            Kind::Gnss,
            Kind::Trigger,
            Kind::Baro,
            Kind::Mag,
            Kind::Clock,
        ] {
            assert!(may_publish(kind, false), "{kind:?}");
        }
    }

    /// An operator connection's `sync` and `rtcm` frames are refused; its
    /// data frames still flow.
    #[tokio::test]
    async fn an_operator_peer_cannot_steer_the_clock_or_a_receiver() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let (tap_tx, mut tap_rx) = mpsc::channel(16);
        let stats = Arc::new(BrokerStats::default());
        let subs = Arc::new(RwLock::new(HashMap::new()));
        let task = tokio::spawn(client(1, theirs, false, subs, Some(tap_tx), stats.clone()));

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
}
