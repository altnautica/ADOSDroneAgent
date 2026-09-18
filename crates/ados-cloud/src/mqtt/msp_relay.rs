//! MSP byte relay over MQTT.
//!
//! The MSP sibling of the MAVLink frame relay. For an MSP flight controller
//! (Betaflight/iNav) the FC->host bytes are raw MSP responses, not MAVLink
//! frames, so they travel a dedicated byte plane the cloud relay bridges the
//! same way it bridges MAVLink frames:
//! * FC->GCS: bytes read from `/run/ados/msp.sock` are published to
//!   `ados/{id}/msp/tx` at q0.
//! * GCS->FC: payloads received on `ados/{id}/msp/rx` (q0) are written back to
//!   the IPC socket toward the flight controller.
//!
//! The hot-path design — a bounded drop-oldest queue plus an in-flight gate — is
//! shared with the MAVLink relay via [`BoundedPublishQueue`] and is unchanged
//! here. The socket is a transparent byte pipe (no MSP is parsed), so the same
//! [`MavlinkClient`] byte client reads it; the name is historical.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ados_plugin_host::mavlink_client::MavlinkClient;
use tokio::sync::{mpsc, watch};

use super::mavlink_relay::{BoundedPublishQueue, RelayMetrics, QUEUE_MAXSIZE};
use super::transport::{MqttQos, MqttTransport, RumqttcTransport};
use super::{msp_client_id, relay_username, topic_msp_rx, topic_msp_tx};
use crate::mqtt::transport::TransportConfig;

/// The MSP-over-MQTT relay. Structurally identical to the MAVLink relay (owns its
/// own rumqttc client plus the bounded-queue + in-flight gate on the hot publish
/// path); only the topics and the byte plane it bridges differ.
pub struct MspMqttRelay {
    device_id: String,
    topic_tx: String,
    topic_rx: String,
    transport_config: TransportConfig,
}

impl MspMqttRelay {
    /// Build the relay for a device id + broker dial config. The transport
    /// config's username is the `ados-{id}` relay form; callers wire the broker
    /// host/port/password (the same config the MAVLink relay uses).
    ///
    /// The handed-in config's `client_id` is REPLACED with this lane's own
    /// (`ados-{id}-msp`). Callers pass a clone of the MAVLink relay's config, and
    /// MQTT requires a broker to evict the existing session when a second client
    /// presents the same ClientID — so sharing it made the two relays disconnect
    /// each other in a sub-second loop forever on any MSP rig: no cloud
    /// telemetry, no cloud command authority, and a flapping `mqttConnected`.
    /// Rewriting it here rather than at the call sites means a third spawn site
    /// cannot reintroduce the collision.
    pub fn new(device_id: impl Into<String>, transport_config: TransportConfig) -> Self {
        let device_id = device_id.into();
        let mut transport_config = transport_config;
        transport_config.client_id = msp_client_id(&device_id);
        MspMqttRelay {
            topic_tx: topic_msp_tx(&device_id),
            topic_rx: topic_msp_rx(&device_id),
            transport_config,
            device_id,
        }
    }

    /// The MQTT ClientID this relay dials with (`ados-{device_id}-msp`), its own
    /// broker principal distinct from the MAVLink relay's.
    pub fn client_id(&self) -> &str {
        &self.transport_config.client_id
    }

    /// The relay's MQTT username (`ados-{device_id}`), exposed for the dial
    /// config the caller assembles.
    pub fn username(&self) -> String {
        relay_username(&self.device_id)
    }

    /// Run the relay until `shutdown` fires. Connects the broker transport + the
    /// IPC client, subscribes `msp/rx` (q0), forwards received payloads to the FC,
    /// and drains the bounded queue of FC bytes to `msp/tx` (q0) under the
    /// in-flight gate.
    pub async fn run(
        &self,
        ipc_sock: impl AsRef<std::path::Path>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        self.run_observed(ipc_sock, shutdown, None).await
    }

    /// Run the relay, publishing the transport's CONFIRMED broker-connection flag
    /// to `connected_out` once dialed. A supervisor reads that flag to report
    /// whether the broker session is actually up — the relay task staying alive is
    /// NOT proof of a connection (rumqttc dials lazily and retries a down broker
    /// forever), so a truthful `mqttConnected` observes this flag, not the handle.
    pub async fn run_observed(
        &self,
        ipc_sock: impl AsRef<std::path::Path>,
        shutdown: tokio::sync::watch::Receiver<bool>,
        connected_out: Option<&watch::Sender<Option<Arc<AtomicBool>>>>,
    ) -> anyhow::Result<()> {
        let transport = RumqttcTransport::connect(&self.transport_config);
        // Hand the supervisor the live connection flag (set on ConnAck, cleared
        // on Disconnect/error). Until ConnAck the flag reads false, so the
        // supervisor never reports a connection the broker has not granted.
        if let Some(sink) = connected_out {
            let _ = sink.send(Some(transport.connected_handle()));
        }
        let mut incoming = transport
            .take_incoming()
            .await
            .ok_or_else(|| anyhow::anyhow!("transport incoming channel already taken"))?;

        // Connect the IPC client (FC bytes in, commands out). Best-effort: a
        // missing socket is logged and the relay exits so systemd restarts it.
        let ipc = match MavlinkClient::connect(ipc_sock).await {
            Ok(c) => std::sync::Arc::new(c),
            Err(e) => {
                tracing::warn!(error = %e, "msp relay: ipc unavailable");
                return Ok(());
            }
        };
        // See the MAVLink relay: every byte this connection writes reached the
        // node over the broker, so it declares that before writing any.
        ipc.declare_off_box_source();

        // GCS->FC: subscribe rx and write received payloads to the IPC socket.
        // Through the transport (NOT the raw client) so the topic is recorded for
        // replay on the next accepted session; a raw-client subscribe is lost at
        // the first reconnect and the command path dies silently.
        if let Err(e) = transport
            .subscribe(&self.topic_rx, MqttQos::AtMostOnce)
            .await
        {
            tracing::warn!(error = %e, "msp relay: rx subscribe failed");
        }

        // FC->GCS: a BOUNDED channel between the IPC byte stream and the
        // publisher. An unbounded channel here would buffer FC bytes without limit
        // and OOM the process if the publisher stalls; the reader drops the NEWEST
        // buffer when the channel is full (recency is preserved by the drop-oldest
        // BoundedPublishQueue the publisher drains into).
        let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(QUEUE_MAXSIZE);
        let mut fc_frames = ipc.subscribe();
        let reader = tokio::spawn(async move {
            loop {
                match fc_frames.recv().await {
                    Ok(frame) => match frame_tx.try_send(frame) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {} // drop newest
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let mut queue = BoundedPublishQueue::new();
        let mut metrics = RelayMetrics::default();
        let mut shutdown = shutdown;
        let client = transport.client().clone();

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                // FC bytes in: enqueue (drop-oldest on full).
                frame = frame_rx.recv() => {
                    match frame {
                        Some(f) => {
                            metrics.frames_in += 1;
                            if queue.push(f) {
                                metrics.frames_dropped_queue_full += 1;
                            }
                        }
                        None => break,
                    }
                }
                // GCS->FC command in: write to the IPC socket toward the FC.
                msg = incoming.recv() => {
                    match msg {
                        Some(m) if m.topic == self.topic_rx && !m.payload.is_empty() => {
                            ipc.send_bytes(&m.payload);
                        }
                        Some(_) => {}
                        None => {}
                    }
                }
            }

            // Drain the queue under the in-flight gate. q0 publishes are
            // fire-and-forget, so a send that returns is treated as acked
            // immediately (the in-flight gate still bounds a slow client because a
            // blocked send holds the slot until it returns).
            while let Some(frame) = queue.try_take() {
                queue.on_publish_started();
                let r = client
                    .publish(
                        self.topic_tx.clone(),
                        rumqttc::QoS::AtMostOnce,
                        false,
                        frame,
                    )
                    .await;
                queue.on_publish_acked();
                match r {
                    Ok(()) => metrics.frames_published += 1,
                    Err(_) => metrics.publish_errors += 1,
                }
            }
        }

        reader.abort();
        tracing::info!(
            frames_in = metrics.frames_in,
            frames_published = metrics.frames_published,
            frames_dropped_queue_full = metrics.frames_dropped_queue_full,
            "msp relay stopped"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_relay_takes_its_own_client_id_off_the_mavlink_relays_config() {
        // Both spawn sites hand this relay a CLONE of the MAVLink relay's dial
        // config. Left as-is, the two would present the same MQTT ClientID and
        // the broker would evict whichever connected first, forever.
        let mavlink_config = TransportConfig {
            client_id: "ados-dev1".to_string(),
            host: "mqtt.example".to_string(),
            port: 443,
            ws_path: "/mqtt".to_string(),
            username: "ados-dev1".to_string(),
            password: "k".to_string(),
            inflight: 1000,
            keep_alive: Duration::from_secs(30),
        };
        let relay = MspMqttRelay::new("dev1", mavlink_config.clone());
        assert_eq!(relay.client_id(), "ados-dev1-msp");
        assert_ne!(relay.client_id(), mavlink_config.client_id);
        // The broker ACL keys on the username, which is unchanged: only the
        // session identity differs between the two lanes.
        assert_eq!(relay.username(), "ados-dev1");
        // The rest of the dial config rides through untouched.
        assert_eq!(relay.transport_config.host, "mqtt.example");
        assert_eq!(relay.transport_config.password, "k");
    }
}
