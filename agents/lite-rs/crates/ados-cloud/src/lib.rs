//! Cloud relay client for the lightweight agent.
//!
//! Speaks the contracts pinned at `proto/cloud/`:
//!
//! - MQTT topics under `ados/{device_id}/...` per `proto/cloud/mqtt-topics.md`
//! - HTTPS heartbeat + pairing beacon per `proto/cloud/openapi.yaml`
//!
//! At v0.1 the client publishes inbound MAVLink frames it receives on a
//! `tokio::sync::broadcast::Receiver` to the `mavlink/tx` topic and
//! emits a heartbeat every 5 seconds. Inbound MQTT subscription
//! (`mavlink/rx`, `command`, `webrtc/offer`) is wired structurally but
//! handler bodies are TODOs — the v0.1 surface only needs the outbound
//! path for Phase 1 validation. Pairing beacon emits every 30 seconds
//! when the agent has no API key.

#![forbid(unsafe_code)]

use std::fmt;
use std::time::Duration;

use rumqttc::{AsyncClient, MqttOptions, QoS, Transport};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("MQTT client error: {0}")]
    Mqtt(#[from] rumqttc::ClientError),

    #[error("HTTPS request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid configuration: {0}")]
    Config(String),
}

/// Configuration for the cloud client. Carries the broker address, the
/// device identity, and (when paired) the API key issued at pair time.
///
/// `Debug` is implemented manually so the `api_key` is never echoed into
/// log messages or panic backtraces. Use the field accessors directly
/// when the cleartext value is required.
#[derive(Clone, Serialize, Deserialize)]
pub struct CloudConfig {
    pub device_id: String,
    pub mqtt_broker: String,
    pub mqtt_port: u16,
    pub mqtt_use_tls: bool,
    pub convex_url: String,
    /// Per-device API key. Empty string means "unpaired" — the client will
    /// emit a pairing beacon instead of a heartbeat until paired.
    pub api_key: String,
}

impl fmt::Debug for CloudConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let api_key_status = if self.api_key.is_empty() {
            "<unset>"
        } else {
            "<redacted>"
        };
        f.debug_struct("CloudConfig")
            .field("device_id", &self.device_id)
            .field("mqtt_broker", &self.mqtt_broker)
            .field("mqtt_port", &self.mqtt_port)
            .field("mqtt_use_tls", &self.mqtt_use_tls)
            .field("convex_url", &self.convex_url)
            .field("api_key", &api_key_status)
            .finish()
    }
}

/// Pairing beacon payload posted to `{convex_url}/pairing/register` every
/// 30 s when the agent is unpaired.
#[derive(Debug, Serialize)]
pub struct PairingBeacon<'a> {
    pub device_id: &'a str,
    pub pairing_code: &'a str,
    pub api_key: &'a str,
    pub name: &'a str,
    pub version: &'a str,
}

/// Spawn the cloud client tasks: MQTT publish loop, HTTPS heartbeat, and
/// pairing beacon. Returns immediately. The tasks run until the inbound
/// broadcast `Sender` is dropped or the agent process exits.
pub fn spawn_cloud_client(
    config: CloudConfig,
    inbound_mavlink: broadcast::Sender<Vec<u8>>,
) -> Result<(), CloudError> {
    if config.device_id.is_empty() {
        return Err(CloudError::Config("device_id is required".into()));
    }

    // MQTT: publish inbound MAVLink frames to ados/{device_id}/mavlink/tx.
    let mqtt_config = config.clone();
    let mut mavlink_rx = inbound_mavlink.subscribe();
    tokio::spawn(async move {
        if let Err(e) = mqtt_publish_loop(mqtt_config, &mut mavlink_rx).await {
            tracing::error!(error = %e, "mqtt publish loop exited");
        }
    });

    // HTTPS: heartbeat (when paired) or pairing beacon (when unpaired).
    let http_config = config;
    tokio::spawn(async move {
        if let Err(e) = http_loop(http_config).await {
            tracing::error!(error = %e, "https loop exited");
        }
    });

    Ok(())
}

async fn mqtt_publish_loop(
    config: CloudConfig,
    mavlink_rx: &mut broadcast::Receiver<Vec<u8>>,
) -> Result<(), CloudError> {
    let client_id = format!("ados-{}", config.device_id);
    let mut opts = MqttOptions::new(&client_id, &config.mqtt_broker, config.mqtt_port);
    opts.set_keep_alive(Duration::from_secs(60));
    opts.set_clean_session(true);
    if config.mqtt_use_tls {
        // Default rustls configuration with the platform native trust store
        // bundled by rumqttc. The agent does not pin a custom CA at v0.1.
        opts.set_transport(Transport::tls_with_default_config());
    }
    if !config.api_key.is_empty() {
        opts.set_credentials(client_id.as_str(), config.api_key.as_str());
    }

    let (client, mut eventloop) = AsyncClient::new(opts, 1024);
    let topic_tx = format!("ados/{}/mavlink/tx", config.device_id);

    // Drive the eventloop in the background; handle errors with reconnect
    // backoff.
    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(_event) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "mqtt eventloop poll error; backing off");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });

    loop {
        match mavlink_rx.recv().await {
            Ok(frame) => {
                if let Err(e) = client
                    .publish(&topic_tx, QoS::AtMostOnce, false, frame)
                    .await
                {
                    tracing::warn!(error = %e, "mqtt publish failed");
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(dropped = n, "mqtt publisher lagging behind FC frame rate");
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!("mavlink broadcast closed; mqtt publish loop exiting");
                return Ok(());
            }
        }
    }
}

async fn http_loop(config: CloudConfig) -> Result<(), CloudError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    if config.api_key.is_empty() {
        loop {
            if let Err(e) = send_pairing_beacon(&client, &config).await {
                tracing::warn!(error = %e, "pairing beacon failed");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    } else {
        loop {
            if let Err(e) = send_heartbeat(&client, &config).await {
                tracing::warn!(error = %e, "heartbeat failed");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}

async fn send_pairing_beacon(
    client: &reqwest::Client,
    config: &CloudConfig,
) -> Result<(), CloudError> {
    let url = format!("{}/pairing/register", config.convex_url.trim_end_matches('/'));
    let beacon = PairingBeacon {
        device_id: &config.device_id,
        pairing_code: "", // generated by the agent's pairing manager in the next phase
        api_key: "",
        name: "ADOS Lite Agent",
        version: env!("CARGO_PKG_VERSION"),
    };
    let response = client.post(&url).json(&beacon).send().await?;
    tracing::debug!(status = %response.status(), "pairing beacon sent");
    Ok(())
}

async fn send_heartbeat(
    client: &reqwest::Client,
    config: &CloudConfig,
) -> Result<(), CloudError> {
    let url = format!("{}/agent/status", config.convex_url.trim_end_matches('/'));
    // Minimal heartbeat shape at v0.1. The full schema in
    // proto/cloud/openapi.yaml lands as the agent grows access to FC
    // state, board info, video state, etc.
    let body = serde_json::json!({
        "deviceId": config.device_id,
        "version": env!("CARGO_PKG_VERSION"),
        "agentVersion": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": 0,
        "runtimeMode": "lite",
    });
    let response = client
        .post(&url)
        .header("X-ADOS-Key", &config.api_key)
        .json(&body)
        .send()
        .await?;
    tracing::debug!(status = %response.status(), "heartbeat sent");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_config_serializes_round_trip() {
        let original = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: "broker.example".into(),
            mqtt_port: 8883,
            mqtt_use_tls: true,
            convex_url: "https://relay.example".into(),
            api_key: "secret".into(),
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let restored: CloudConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored.device_id, original.device_id);
        assert_eq!(restored.mqtt_broker, original.mqtt_broker);
        assert_eq!(restored.api_key, original.api_key);
    }

    #[test]
    fn empty_device_id_is_rejected() {
        let bad = CloudConfig {
            device_id: String::new(),
            mqtt_broker: "broker.example".into(),
            mqtt_port: 8883,
            mqtt_use_tls: true,
            convex_url: "https://relay.example".into(),
            api_key: String::new(),
        };
        let (tx, _rx) = broadcast::channel(8);
        let err = spawn_cloud_client(bad, tx).expect_err("empty device_id should fail");
        match err {
            CloudError::Config(msg) => assert!(msg.contains("device_id")),
            _ => panic!("expected Config error, got {:?}", err),
        }
    }
}
