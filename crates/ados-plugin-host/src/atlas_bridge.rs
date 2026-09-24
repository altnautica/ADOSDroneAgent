//! Bridge from the Atlas bus onto the plugin event bus.
//!
//! `ados-atlas` publishes its shared-data topics (`plugin.atlas.pose` and the
//! world-model artifact descriptors) on the local atlas bus. Every
//! `plugin.atlas.*` event is republished on the plugin [`EventBus`] as a host
//! event. A plugin subscribes to a shared topic only through a plugin's
//! `shared_topics` declaration (see [`crate::handlers::is_subscribe_allowed`]),
//! and no plugin may declare or publish under `plugin.atlas.`, so these host
//! events reach no plugin subscriber. Capture-internal topics (keyframes,
//! capture state, offloaded pose) stay on the atlas bus.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::atlas::{AtlasEvent, PLUGIN_ATLAS_TOPICS};
use ados_protocol::frame::PLUGIN_MAX_FRAME;
use ados_protocol::ipc::read_length_prefixed;
use rmpv::Value;
use tokio::task::JoinHandle;

use crate::handlers::EventBus;
use crate::vehicle_events::publish_host_event;

/// Fixed wait between reconnect attempts, with no cap: Atlas starts and stops
/// with its capture gate, and the bridge must pick it up whenever it appears.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);

/// The plugin-bus `(topic, payload)` one atlas bus frame republishes as, or
/// `None` for a frame that is not a shared-data topic or does not decode.
pub fn plugin_event_from_frame(body: &[u8]) -> Option<(String, Value)> {
    let event = AtlasEvent::decode(body).ok()?;
    if !PLUGIN_ATLAS_TOPICS.contains(&event.topic.as_str()) {
        return None;
    }
    let payload = rmpv::decode::read_value(&mut event.payload.as_slice()).ok()?;
    Some((event.topic, payload))
}

/// Subscribe to the atlas bus at `path` forever and republish its shared-data
/// events on `bus`.
pub fn spawn_atlas_bridge(bus: Arc<EventBus>, path: PathBuf) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match tokio::net::UnixStream::connect(&path).await {
                Ok(mut stream) => {
                    tracing::info!(path = %path.display(), "atlas bridge subscribed");
                    loop {
                        match read_length_prefixed(&mut stream, PLUGIN_MAX_FRAME, false).await {
                            Ok(Some(body)) => {
                                if let Some((topic, payload)) = plugin_event_from_frame(&body) {
                                    publish_host_event(&bus, &topic, payload);
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                tracing::debug!(error = %e, "atlas bridge read failed; reconnecting");
                                break;
                            }
                        }
                    }
                }
                Err(e) => tracing::debug!(error = %e, "atlas bus not reachable; retrying"),
            }
            tokio::time::sleep(RECONNECT_INTERVAL).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::atlas::{ATLAS_KEYFRAME_TOPIC, PLUGIN_ATLAS_POSE_TOPIC};
    use tokio::io::AsyncWriteExt;

    fn frame(topic: &str, payload: &Value) -> Vec<u8> {
        let mut inner = Vec::new();
        rmpv::encode::write_value(&mut inner, payload).unwrap();
        AtlasEvent::new(topic, None, inner).encode().unwrap()
    }

    #[test]
    fn only_shared_data_topics_cross_onto_the_plugin_bus() {
        let payload = Value::Map(vec![(Value::from("ts_ms"), Value::from(7))]);
        assert_eq!(
            plugin_event_from_frame(&frame(PLUGIN_ATLAS_POSE_TOPIC, &payload)),
            Some((PLUGIN_ATLAS_POSE_TOPIC.to_string(), payload.clone()))
        );
        assert_eq!(
            plugin_event_from_frame(&frame(ATLAS_KEYFRAME_TOPIC, &payload)),
            None
        );
        assert_eq!(plugin_event_from_frame(b"not msgpack"), None);
    }

    /// End to end: a pose published on the atlas bus reaches a plugin-bus
    /// subscriber as a host event.
    #[tokio::test]
    async fn a_pose_on_the_atlas_bus_is_republished_to_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("atlas.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let bridge = spawn_atlas_bridge(Arc::clone(&bus), sock);
        let (mut conn, _) = listener.accept().await.unwrap();
        let body = frame(PLUGIN_ATLAS_POSE_TOPIC, &Value::from("pose"));
        let mut wire = (body.len() as u32).to_be_bytes().to_vec();
        wire.extend_from_slice(&body);
        conn.write_all(&wire).await.unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("the pose must reach the plugin bus")
            .unwrap();
        assert_eq!(event.topic, PLUGIN_ATLAS_POSE_TOPIC);
        assert_eq!(
            event.publisher_plugin_id,
            crate::vehicle_events::HOST_PUBLISHER
        );
        assert_eq!(event.payload, Value::from("pose"));
        bridge.abort();
    }
}
