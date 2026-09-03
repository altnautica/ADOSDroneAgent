//! The world-model descriptor stream (compute -> GCS / drone).
//!
//! The compute node fans out world-model descriptors — a splat, a point cloud, a
//! mesh, an occupancy / ESDF grid — on a broadcast channel, and a consumer
//! subscribes over a per-device WebSocket beside the video relay rather than
//! through the bounded MAVLink queue. Each descriptor is tagged with the drone it
//! belongs to, so a multi-device compute node never cross-talks one drone's world
//! into another's view. A slow subscriber that lags is skipped, never blocking
//! the trainer; a disconnected one is reaped even while the stream is idle.
//!
//! # Why this carries generation-versioned descriptors and not deltas
//!
//! This lane was specified as an "SPZ delta" stream: the trainer would emit
//! incremental splat diffs and the viewer would apply them. That cannot be built
//! as specified, and the reason is a property of the formats rather than a
//! missing implementation. SPZ and SOG are whole-scene containers: both apply
//! GLOBAL quantisation and Morton-order the gaussians, so a single added splat
//! perturbs the ordering and the quantisation ranges of the whole payload.
//! Neither the container formats nor the Khronos glTF extensions
//! (`KHR_gaussian_splatting`, `KHR_gaussian_splatting_compression_spz`) define an
//! incremental append or a delta codec. "SPZ deltas" would therefore have meant
//! inventing, shipping and maintaining a codec no other tool on earth reads, on
//! a flight-adjacent lane, for a benefit — seeing the model grow — that is
//! obtainable without it.
//!
//! So what actually rides here is what the field uses: an immutable artifact set
//! per monotonically-increasing `generation`, described by a small descriptor
//! carrying a level-of-detail chunk manifest. The viewer fetches the coarsest
//! level of the new generation first and refines, which puts pixels on screen in
//! well under a second on a scene whose full transfer takes minutes, and it
//! diffs chunk lists between generations to avoid refetching what has not
//! changed. Identical operator-visible behaviour, no bespoke codec.
//!
//! The mechanism below is deliberately agnostic to which descriptor it carries:
//! it moves framed [`AtlasEvent`]s tagged by device, so adding a topic needs no
//! transport change.

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

/// The WebSocket route a world-model consumer connects to, one path per device.
pub const WORLD_WS_ROUTE: &str = "/ws/atlas/:device_id";

/// The concrete WS path for `device_id` (the client side of [`WORLD_WS_ROUTE`]).
pub fn world_ws_path(device_id: &str) -> String {
    format!("/ws/atlas/{device_id}")
}

/// Fans out world-model descriptors, tagged by device, to every connected
/// subscriber.
pub struct WorldBroadcaster {
    tx: broadcast::Sender<(String, AtlasEvent)>,
}

impl WorldBroadcaster {
    /// A broadcaster buffering up to `capacity` events per subscriber (a slow
    /// subscriber past the buffer lags and skips, never blocking the publisher).
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish a descriptor for `device_id`. Returns the number of
    /// currently-subscribed receivers at send time — NOT a delivery guarantee,
    /// since a subscriber that is lagging past the buffer silently skips it.
    /// 0 means nothing is connected (not an error). Each receiver filters to its
    /// own device.
    pub fn publish(&self, device_id: &str, event: AtlasEvent) -> usize {
        self.tx.send((device_id.to_string(), event)).unwrap_or(0)
    }

    /// Subscribe to the (device, descriptor) stream (the WS handler does this
    /// per connection, filtering to its own device).
    pub fn subscribe(&self) -> broadcast::Receiver<(String, AtlasEvent)> {
        self.tx.subscribe()
    }

    /// How many subscribers are currently connected.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// The axum router the compute node mounts to serve the per-device world-model
/// descriptor stream.
pub fn world_ws_router(broadcaster: Arc<WorldBroadcaster>) -> Router {
    Router::new()
        .route(WORLD_WS_ROUTE, get(world_ws))
        .with_state(broadcaster)
}

async fn world_ws(
    ws: WebSocketUpgrade,
    Path(device_id): Path<String>,
    State(b): State<Arc<WorldBroadcaster>>,
) -> Response {
    // Subscribe BEFORE the upgrade completes so a descriptor published in the
    // connect window is not missed by a freshly-connected consumer.
    let rx = b.subscribe();
    ws.on_upgrade(move |socket| forward_descriptors(socket, device_id, rx))
}

async fn forward_descriptors(
    mut socket: WebSocket,
    device_id: String,
    mut rx: broadcast::Receiver<(String, AtlasEvent)>,
) {
    loop {
        tokio::select! {
            // Outbound: a published descriptor for THIS device.
            published = rx.recv() => match published {
                Ok((dev, event)) => {
                    if dev != device_id {
                        continue; // another drone's world — not this view
                    }
                    let Ok(bytes) = event.encode() else {
                        continue;
                    };
                    if socket.send(Message::Binary(bytes)).await.is_err() {
                        break; // the consumer disconnected
                    }
                }
                // A subscriber that fell behind the buffer skips the gap and
                // keeps streaming rather than stalling the trainer. Safe here in
                // a way it would NOT be for a delta lane: each descriptor is a
                // complete statement about one generation, so skipping one only
                // costs the consumer that generation, never desynchronises it.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            // Inbound: drain client frames so axum's automatic pong reply fires
            // (keepalive) and a disconnect (Close / None / Err) is reaped even
            // while the descriptor stream is idle.
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
    use ados_protocol::atlas::{
        SplatDescriptor, PLUGIN_ATLAS_MESH_TOPIC, PLUGIN_ATLAS_SPLAT_TOPIC,
    };
    use futures_util::StreamExt;

    fn descriptor(topic: &str) -> AtlasEvent {
        AtlasEvent::new(topic, None, vec![7, 7, 7])
    }

    #[tokio::test]
    async fn publish_reaches_a_direct_subscriber() {
        let b = WorldBroadcaster::new(16);
        let mut rx = b.subscribe();
        assert_eq!(b.subscriber_count(), 1);
        assert_eq!(
            b.publish("drone-1", descriptor(PLUGIN_ATLAS_SPLAT_TOPIC)),
            1
        );
        let (dev, got) = rx.recv().await.unwrap();
        assert_eq!(dev, "drone-1");
        assert_eq!(got.topic, PLUGIN_ATLAS_SPLAT_TOPIC);
    }

    #[tokio::test]
    async fn publish_with_no_subscriber_reaches_zero_not_an_error() {
        let b = WorldBroadcaster::new(16);
        assert_eq!(
            b.publish("drone-1", descriptor(PLUGIN_ATLAS_SPLAT_TOPIC)),
            0
        );
    }

    async fn spawn_world_server(b: Arc<WorldBroadcaster>) -> std::net::SocketAddr {
        let app = world_ws_router(b);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    async fn wait_for_subscribers(b: &WorldBroadcaster, n: usize) {
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
    async fn a_published_descriptor_reaches_a_ws_subscriber_for_its_device() {
        let broadcaster = Arc::new(WorldBroadcaster::new(16));
        let addr = spawn_world_server(broadcaster.clone()).await;
        let (mut ws, _resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", world_ws_path("drone-1")))
                .await
                .unwrap();
        wait_for_subscribers(&broadcaster, 1).await;

        // A descriptor for ANOTHER device must not reach this subscriber...
        broadcaster.publish("drone-2", descriptor(PLUGIN_ATLAS_MESH_TOPIC));
        // ...only the one for drone-1.
        broadcaster.publish("drone-1", descriptor(PLUGIN_ATLAS_SPLAT_TOPIC));

        let msg = ws.next().await.unwrap().unwrap();
        let event = AtlasEvent::decode(&msg.into_data()).unwrap();
        assert_eq!(event.topic, PLUGIN_ATLAS_SPLAT_TOPIC); // not the drone-2 mesh
        assert_eq!(event.payload, vec![7, 7, 7]);
    }

    #[tokio::test]
    async fn a_disconnected_subscriber_is_reaped_even_while_idle() {
        let broadcaster = Arc::new(WorldBroadcaster::new(16));
        let addr = spawn_world_server(broadcaster.clone()).await;
        let (ws, _resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", world_ws_path("drone-1")))
                .await
                .unwrap();
        wait_for_subscribers(&broadcaster, 1).await;
        // Drop the client without ever publishing (idle stream). The select! over
        // socket.recv() must detect the disconnect and reap the task + its
        // broadcast receiver.
        drop(ws);
        wait_for_subscribers(&broadcaster, 0).await;
    }

    #[tokio::test]
    async fn a_real_generation_versioned_splat_descriptor_round_trips_over_the_ws() {
        // The lane's actual payload: a complete statement about one generation,
        // carrying an LOD chunk manifest — not a delta a consumer must apply in
        // order. This is what replaced the specified SPZ-delta codec, so it is
        // worth pinning that it crosses the wire intact.
        let broadcaster = Arc::new(WorldBroadcaster::new(16));
        let addr = spawn_world_server(broadcaster.clone()).await;
        let (mut ws, _resp) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{}", world_ws_path("drone-1")))
                .await
                .unwrap();
        wait_for_subscribers(&broadcaster, 1).await;

        let splat = SplatDescriptor {
            session_id: "atlas-drone-1-1000".into(),
            generation: 7,
            gaussian_count: 1_250_000,
            step: 30_000,
            url: Some("http://node.example/artifacts/s/g7/scene.spz".into()),
            handle: None,
            manifest_url: Some("http://node.example/artifacts/s/g7/manifest.json".into()),
            lod_levels: 4,
        };
        broadcaster.publish(
            "drone-1",
            AtlasEvent::new(PLUGIN_ATLAS_SPLAT_TOPIC, None, splat.to_msgpack().unwrap()),
        );

        let msg = ws.next().await.unwrap().unwrap();
        let event = AtlasEvent::decode(&msg.into_data()).unwrap();
        let back = SplatDescriptor::from_msgpack(&event.payload).unwrap();
        assert_eq!(back, splat);
        assert_eq!(
            back.generation, 7,
            "the generation is what a viewer diffs on"
        );
        assert_eq!(back.lod_levels, 4);
    }
}
