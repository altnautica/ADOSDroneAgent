//! The detection return stream (node -> drone).
//!
//! A streaming offload session ([`crate::offload_stream::run_offload_session`])
//! emits one [`OffloadDetectionBatch`] per frame; the node fans them out here,
//! and the drone subscribes over a per-session WebSocket. This mirrors the atlas
//! delta lane (`ados_atlas_transport::delta`): a broadcast channel + a WS router,
//! a slow subscriber that lags is skipped (never blocking the detector), and a
//! disconnected one is reaped even while the stream is idle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ados_protocol::offload::OffloadDetectionBatch;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::Response,
    routing::get,
    Router,
};
use tokio::sync::{broadcast, watch};

/// The WS route the drone subscribes to, one path per session.
pub const OFFLOAD_WS_ROUTE: &str = "/ws/offload/:session_id";

/// The concrete WS path for `session_id` (the client side of [`OFFLOAD_WS_ROUTE`]).
pub fn offload_ws_path(session_id: &str) -> String {
    format!("/ws/offload/{session_id}")
}

/// Fans out returned detection batches, tagged by session, to every subscriber.
pub struct DetectionBroadcaster {
    tx: broadcast::Sender<OffloadDetectionBatch>,
    /// Per-session close signal. When a session ends, its `pump_to_broadcaster`
    /// drops the session's watch sender here, waking every WS subscriber for that
    /// session so its socket closes and the drone reconnects. Keyed by session id
    /// so ending one session never disturbs another live one.
    closes: Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl DetectionBroadcaster {
    /// A broadcaster buffering up to `capacity` batches per subscriber (a slow
    /// subscriber past the buffer lags and skips, never blocking the detector).
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self {
            tx,
            closes: Mutex::new(HashMap::new()),
        }
    }

    /// A close-signal receiver for `session_id` (the WS handler holds one per
    /// connection). Get-or-creates the session's signal so a subscriber that
    /// connects before the session's pump has registered still shares the same
    /// signal; the receiver resolves once the session ends.
    fn close_watch(&self, session_id: &str) -> watch::Receiver<bool> {
        self.closes
            .lock()
            .expect("closes registry poisoned")
            .entry(session_id.to_string())
            .or_insert_with(|| watch::channel(false).0)
            .subscribe()
    }

    /// Signal that a session has ended: remove (and drop) its close sender so every
    /// WS subscriber for that session wakes and closes its socket. A session with
    /// no subscribers is a no-op; removing the entry lets a later re-open of the
    /// same session id register a fresh signal.
    pub fn close_session(&self, session_id: &str) {
        self.closes
            .lock()
            .expect("closes registry poisoned")
            .remove(session_id);
    }

    /// Publish a returned batch. Returns the number of currently-subscribed
    /// receivers at send time (NOT a delivery guarantee: a subscriber lagging
    /// past the buffer silently skips). 0 means no drone is subscribed for any
    /// session (not an error). Each receiver filters to its own session.
    pub fn publish(&self, batch: OffloadDetectionBatch) -> usize {
        self.tx.send(batch).unwrap_or(0)
    }

    /// Subscribe to the batch stream (the WS handler does this per connection,
    /// filtering to its own session).
    pub fn subscribe(&self) -> broadcast::Receiver<OffloadDetectionBatch> {
        self.tx.subscribe()
    }

    /// How many subscribers are currently connected.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// Pump a session's detection channel into the broadcaster until the channel
/// closes (the session ended). The daemon spawns this per session so the
/// streaming session stays decoupled from the WS fan-out. When the channel
/// closes, this signals every WS subscriber for `session_id` to close so the
/// drone's subscriber returns and its reconnect logic re-opens the session.
pub async fn pump_to_broadcaster(
    session_id: String,
    mut rx: tokio::sync::mpsc::Receiver<OffloadDetectionBatch>,
    broadcaster: Arc<DetectionBroadcaster>,
) {
    while let Some(batch) = rx.recv().await {
        broadcaster.publish(batch);
    }
    // The session's detection channel closed (the session ended): close this
    // session's WS subscribers so the drone stops waiting on a dead stream.
    broadcaster.close_session(&session_id);
}

/// The axum router the compute node mounts to serve the per-session detection
/// return stream.
pub fn offload_ws_router(broadcaster: Arc<DetectionBroadcaster>) -> Router {
    Router::new()
        .route(OFFLOAD_WS_ROUTE, get(offload_ws))
        .with_state(broadcaster)
}

async fn offload_ws(
    ws: WebSocketUpgrade,
    Path(session_id): Path<String>,
    State(b): State<Arc<DetectionBroadcaster>>,
) -> Response {
    // Subscribe BEFORE the upgrade completes so a batch published in the connect
    // window is not missed by a freshly-connected drone. The close watch closes
    // this socket when its session ends (so the drone reconnects).
    let rx = b.subscribe();
    let close_rx = b.close_watch(&session_id);
    ws.on_upgrade(move |socket| forward_batches(socket, session_id, rx, close_rx))
}

async fn forward_batches(
    mut socket: WebSocket,
    session_id: String,
    mut rx: broadcast::Receiver<OffloadDetectionBatch>,
    mut close_rx: watch::Receiver<bool>,
) {
    loop {
        // `biased`: drain any pending batch BEFORE observing the close, so the
        // final batches queued before a session ended are delivered rather than
        // dropped by the close firing first.
        tokio::select! {
            biased;
            published = rx.recv() => match published {
                Ok(batch) => {
                    if batch.session_id != session_id {
                        continue; // another session's batch — not this stream
                    }
                    let Ok(bytes) = batch.to_msgpack() else {
                        continue;
                    };
                    if socket.send(Message::Binary(bytes)).await.is_err() {
                        break; // the drone disconnected
                    }
                }
                // A subscriber that fell behind the buffer skips the gap and
                // keeps streaming rather than stalling the detector.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // The session ended: its close sender was dropped, so `changed()`
            // resolves (with an error). Close the socket so the drone's subscriber
            // returns and its reconnect logic re-opens the session.
            _ = close_rx.changed() => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            // Inbound: drain client frames so axum's automatic pong reply fires
            // (keepalive) and a disconnect is reaped even while the stream is idle.
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::offload::Detection;
    use futures_util::StreamExt;

    fn batch(session: &str, seq: u64) -> OffloadDetectionBatch {
        OffloadDetectionBatch::new(
            session,
            "front",
            seq,
            1000 + seq as i64,
            640,
            480,
            vec![Detection {
                bbox: [0.4, 0.4, 0.2, 0.2],
                class: "person".into(),
                confidence: 0.9,
                track_id: Some(1),
            }],
        )
    }

    #[tokio::test]
    async fn publish_reaches_a_direct_subscriber() {
        let b = DetectionBroadcaster::new(16);
        let mut rx = b.subscribe();
        assert_eq!(b.publish(batch("s1", 0)), 1);
        let got = rx.recv().await.unwrap();
        assert_eq!(got.session_id, "s1");
        assert_eq!(got.seq, 0);
    }

    #[tokio::test]
    async fn publish_with_no_subscriber_is_zero_not_an_error() {
        let b = DetectionBroadcaster::new(16);
        assert_eq!(b.publish(batch("s1", 0)), 0);
    }

    async fn spawn_server(b: Arc<DetectionBroadcaster>) -> std::net::SocketAddr {
        let app = offload_ws_router(b);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn wait_for_subscribers(b: &DetectionBroadcaster, n: usize) {
        for _ in 0..200 {
            if b.subscriber_count() == n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!(
            "subscriber_count never reached {n} (was {})",
            b.subscriber_count()
        );
    }

    #[tokio::test]
    async fn a_published_batch_reaches_a_ws_subscriber_for_its_session() {
        let broadcaster = Arc::new(DetectionBroadcaster::new(16));
        let addr = spawn_server(broadcaster.clone()).await;
        let (mut ws, _resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", offload_ws_path("s1")))
                .await
                .unwrap();
        wait_for_subscribers(&broadcaster, 1).await;

        // A batch for ANOTHER session must not reach this subscriber...
        broadcaster.publish(batch("s2", 0));
        // ...only the one for s1.
        broadcaster.publish(batch("s1", 5));

        let msg = ws.next().await.unwrap().unwrap();
        let got = OffloadDetectionBatch::from_msgpack(&msg.into_data()).unwrap();
        assert_eq!(got.session_id, "s1"); // not the s2 batch
        assert_eq!(got.seq, 5);
    }

    #[tokio::test]
    async fn a_disconnected_subscriber_is_reaped_even_while_idle() {
        let broadcaster = Arc::new(DetectionBroadcaster::new(16));
        let addr = spawn_server(broadcaster.clone()).await;
        let (ws, _resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", offload_ws_path("s1")))
                .await
                .unwrap();
        wait_for_subscribers(&broadcaster, 1).await;
        drop(ws);
        wait_for_subscribers(&broadcaster, 0).await;
    }

    #[tokio::test]
    async fn pump_forwards_a_session_channel_into_the_broadcaster() {
        let broadcaster = Arc::new(DetectionBroadcaster::new(16));
        let mut rx = broadcaster.subscribe();
        let (tx, mrx) = tokio::sync::mpsc::channel(8);
        let pump = tokio::spawn(pump_to_broadcaster(
            "s1".to_string(),
            mrx,
            broadcaster.clone(),
        ));
        tx.send(batch("s1", 0)).await.unwrap();
        tx.send(batch("s1", 1)).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().seq, 0);
        assert_eq!(rx.recv().await.unwrap().seq, 1);
        drop(tx);
        pump.await.unwrap();
    }

    #[tokio::test]
    async fn a_sessions_ws_closes_when_its_session_ends_while_another_stays_open() {
        let broadcaster = Arc::new(DetectionBroadcaster::new(16));
        let addr = spawn_server(broadcaster.clone()).await;

        // One subscriber per session.
        let (mut ws1, _r1) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", offload_ws_path("s1")))
                .await
                .unwrap();
        let (mut ws2, _r2) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", offload_ws_path("s2")))
                .await
                .unwrap();
        wait_for_subscribers(&broadcaster, 2).await;

        // s1's session ends: only its WS must close.
        broadcaster.close_session("s1");

        // ws1 sees the stream close (a Close frame, then the socket ends).
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), ws1.next())
            .await
            .expect("ws1 resolves promptly on its session end");
        match closed {
            None | Some(Err(_)) => {}
            Some(Ok(msg)) if msg.is_close() => {}
            Some(Ok(other)) => panic!("expected ws1 to close, got a data frame: {other:?}"),
        }

        // s2 stays open and still receives its own batches.
        broadcaster.publish(batch("s2", 7));
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws2.next())
            .await
            .expect("ws2 still delivers")
            .expect("ws2 open")
            .unwrap();
        let got = OffloadDetectionBatch::from_msgpack(&msg.into_data()).unwrap();
        assert_eq!(got.session_id, "s2");
        assert_eq!(got.seq, 7);
    }
}
