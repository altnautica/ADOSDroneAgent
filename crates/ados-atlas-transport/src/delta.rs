//! The splat-delta lane (compute -> GCS).
//!
//! The compute node fans out SPZ splat deltas (and other world-model
//! descriptors) on a broadcast channel; the GCS Live World subscribes over a
//! per-device WebSocket beside the video relay, NOT the bounded MAVLink queue.
//! Each delta is tagged with the drone it belongs to so a multi-device compute
//! node never cross-talks one drone's world into another's GCS view. A slow
//! subscriber that lags is skipped, never blocking the trainer; a disconnected
//! one is reaped even while the stream is idle.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::Response,
    routing::get,
    Router,
};
use tokio::sync::broadcast;

use ados_protocol::atlas::AtlasEvent;

/// The WebSocket route the GCS Live World connects to, one path per device.
pub const DELTA_WS_ROUTE: &str = "/ws/atlas/:device_id";

/// The concrete WS path for `device_id` (the client side of [`DELTA_WS_ROUTE`]).
pub fn delta_ws_path(device_id: &str) -> String {
    format!("/ws/atlas/{device_id}")
}

/// Fans out world-model deltas, tagged by device, to every connected subscriber.
pub struct DeltaBroadcaster {
    tx: broadcast::Sender<(String, AtlasEvent)>,
}

impl DeltaBroadcaster {
    /// A broadcaster buffering up to `capacity` events per subscriber (a slow
    /// subscriber past the buffer lags and skips, never blocking the publisher).
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish a delta for `device_id`. Returns the number of currently-subscribed
    /// receivers at send time — NOT a delivery guarantee, since a subscriber that
    /// is lagging past the buffer silently skips it. 0 means no GCS is connected
    /// (not an error). Each receiver filters to its own device.
    pub fn publish(&self, device_id: &str, event: AtlasEvent) -> usize {
        self.tx.send((device_id.to_string(), event)).unwrap_or(0)
    }

    /// Subscribe to the (device, delta) stream (the WS handler does this per
    /// connection, filtering to its own device).
    pub fn subscribe(&self) -> broadcast::Receiver<(String, AtlasEvent)> {
        self.tx.subscribe()
    }

    /// How many subscribers are currently connected.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// The axum router the compute node mounts to serve the per-device delta stream.
pub fn delta_ws_router(broadcaster: Arc<DeltaBroadcaster>) -> Router {
    Router::new()
        .route(DELTA_WS_ROUTE, get(delta_ws))
        .with_state(broadcaster)
}

async fn delta_ws(
    ws: WebSocketUpgrade,
    Path(device_id): Path<String>,
    State(b): State<Arc<DeltaBroadcaster>>,
) -> Response {
    // Subscribe BEFORE the upgrade completes so a delta published in the connect
    // window is not missed by a freshly-connected GCS.
    let rx = b.subscribe();
    ws.on_upgrade(move |socket| forward_deltas(socket, device_id, rx))
}

async fn forward_deltas(
    mut socket: WebSocket,
    device_id: String,
    mut rx: broadcast::Receiver<(String, AtlasEvent)>,
) {
    loop {
        tokio::select! {
            // Outbound: a published delta for THIS device.
            published = rx.recv() => match published {
                Ok((dev, event)) => {
                    if dev != device_id {
                        continue; // another drone's delta — not this view
                    }
                    let Ok(bytes) = event.to_msgpack() else {
                        continue;
                    };
                    if socket.send(Message::Binary(bytes)).await.is_err() {
                        break; // the GCS disconnected
                    }
                }
                // A subscriber that fell behind the buffer skips the gap and
                // keeps streaming rather than stalling the trainer.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Inbound: drain client frames so axum's automatic pong reply fires
            // (keepalive) and a disconnect (Close / None / Err) is reaped even
            // while the delta stream is idle.
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {} // ping auto-ponged by axum; ignore other frames
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn delta(topic: &str) -> AtlasEvent {
        AtlasEvent {
            topic: topic.into(),
            device_id: None,
            payload: vec![7, 7, 7],
        }
    }

    #[tokio::test]
    async fn publish_reaches_a_direct_subscriber() {
        let b = DeltaBroadcaster::new(16);
        let mut rx = b.subscribe();
        assert_eq!(b.subscriber_count(), 1);
        assert_eq!(b.publish("drone-1", delta("plugin.atlas.splat")), 1);
        let (dev, got) = rx.recv().await.unwrap();
        assert_eq!(dev, "drone-1");
        assert_eq!(got.topic, "plugin.atlas.splat");
    }

    #[tokio::test]
    async fn publish_with_no_subscriber_reaches_zero_not_an_error() {
        let b = DeltaBroadcaster::new(16);
        assert_eq!(b.publish("drone-1", delta("plugin.atlas.splat")), 0);
    }

    async fn spawn_delta_server(b: Arc<DeltaBroadcaster>) -> std::net::SocketAddr {
        let app = delta_ws_router(b);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn wait_for_subscribers(b: &DeltaBroadcaster, n: usize) {
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
    async fn a_published_delta_reaches_a_ws_subscriber_for_its_device() {
        let broadcaster = Arc::new(DeltaBroadcaster::new(16));
        let addr = spawn_delta_server(broadcaster.clone()).await;
        let (mut ws, _resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", delta_ws_path("drone-1")))
                .await
                .unwrap();
        wait_for_subscribers(&broadcaster, 1).await;

        // A delta for ANOTHER device must not reach this subscriber...
        broadcaster.publish("drone-2", delta("plugin.atlas.mesh"));
        // ...only the one for drone-1.
        broadcaster.publish("drone-1", delta("plugin.atlas.splat"));

        let msg = ws.next().await.unwrap().unwrap();
        let event = AtlasEvent::from_msgpack(&msg.into_data()).unwrap();
        assert_eq!(event.topic, "plugin.atlas.splat"); // not the drone-2 mesh
        assert_eq!(event.payload, vec![7, 7, 7]);
    }

    #[tokio::test]
    async fn a_disconnected_subscriber_is_reaped_even_while_idle() {
        let broadcaster = Arc::new(DeltaBroadcaster::new(16));
        let addr = spawn_delta_server(broadcaster.clone()).await;
        let (ws, _resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", delta_ws_path("drone-1")))
                .await
                .unwrap();
        wait_for_subscribers(&broadcaster, 1).await;
        // Drop the client without ever publishing a delta (idle stream). The
        // select! over socket.recv() must detect the disconnect and reap the
        // task + its broadcast receiver.
        drop(ws);
        wait_for_subscribers(&broadcaster, 0).await;
    }
}
