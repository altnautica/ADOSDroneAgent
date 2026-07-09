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
use super::transport::RumqttcTransport;
use super::{relay_username, topic_msp_rx, topic_msp_tx};
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
    pub fn new(device_id: impl Into<String>, transport_config: TransportConfig) -> Self {
        let device_id = device_id.into();
        MspMqttRelay {
            topic_tx: topic_msp_tx(&device_id),
            topic_rx: topic_msp_rx(&device_id),
            transport_config,
            device_id,
        }
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

        // GCS->FC: subscribe rx and write received payloads to the IPC socket.
        if let Err(e) = transport
            .client()
            .subscribe(self.topic_rx.clone(), rumqttc::QoS::AtMostOnce)
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
