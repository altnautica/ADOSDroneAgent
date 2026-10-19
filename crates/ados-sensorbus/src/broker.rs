//! The broker `ados-clock` runs on `sensors.sock`.
//!
//! Every client may publish. A client receives nothing until it sends a
//! [`Kind::Subscribe`] frame; from then on it receives every frame of those
//! kinds published by any *other* client (a later subscribe replaces the set).
//! Each subscriber has a bounded queue; a slow subscriber loses frames rather
//! than stalling the publishers. Every published frame is also handed to the
//! optional `tap` (the clock's own consumer), which is never blocked either.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, RwLock};

use crate::client::read_frame;
use crate::frame::{encode, BusFrame, Kind};

const SUB_QUEUE: usize = 1024;

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
    pub clients: AtomicU64,
}

/// Accept clients forever. `tap` receives a copy of every published frame.
pub async fn serve(
    listener: UnixListener,
    tap: Option<mpsc::Sender<BusFrame>>,
    stats: Arc<BrokerStats>,
) {
    let subs: Arc<RwLock<HashMap<u64, Sub>>> = Arc::new(RwLock::new(HashMap::new()));
    let mut next_id = 0u64;
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        next_id += 1;
        tokio::spawn(client(
            next_id,
            stream,
            subs.clone(),
            tap.clone(),
            stats.clone(),
        ));
    }
}

async fn client(
    id: u64,
    stream: UnixStream,
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
    use crate::{v1, Origin, Publisher, Subscriber};
    use std::time::Duration;

    #[tokio::test]
    async fn frames_reach_matching_subscribers_and_the_tap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sensors.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (tap_tx, mut tap_rx) = mpsc::channel(16);
        let stats = Arc::new(BrokerStats::default());
        tokio::spawn(serve(listener, Some(tap_tx), stats.clone()));

        let mut imu_sub = Subscriber::connect(&path, &[Kind::Imu]).await.unwrap();
        let mut rtcm_sub = Subscriber::connect(&path, &[Kind::Rtcm]).await.unwrap();
        // Let the broker register both subscriptions.
        tokio::time::sleep(Duration::from_millis(50)).await;

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
    }
}
