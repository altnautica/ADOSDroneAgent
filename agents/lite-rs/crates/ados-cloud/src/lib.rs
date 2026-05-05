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
//! path the control-plane validation needs. Pairing beacon emits every 30 seconds
//! when the agent has no API key.

#![forbid(unsafe_code)]

pub mod sysmetrics;

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ados_setup::pairing::PairingStore;
use rumqttc::{AsyncClient, MqttOptions, QoS, Transport};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::broadcast;

const DEFAULT_PAIRING_PATH: &str = "/etc/ados/pairing.json";

/// Per-frame ceiling on the MQTT publish call. A stalled broker (TLS
/// handshake hang, congested upstream link, half-open socket) would
/// otherwise let the publish future park forever while the FC keeps
/// producing frames at ~30 Hz. After the timeout the frame is logged
/// and dropped; the broadcast channel naturally moves on to the next
/// one rather than queueing stale telemetry.
const MQTT_PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);

/// Drop guard that aborts a spawned task synchronously when the guard
/// is dropped. Used to tie the lifetime of the MQTT eventloop driver
/// to the outer publish loop: if the parent task is cancelled, the
/// inner eventloop task is cancelled with it instead of continuing
/// to poll the broker as a zombie.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

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
/// device identity, and the path to `pairing.json` where the live
/// pair-code + api-key live.
///
/// `Debug` is implemented manually so the pair-code path is logged but
/// no secret value ever lands in a panic backtrace.
///
/// Note: prior versions of this struct carried an `api_key` field
/// directly. That was structurally wrong — agent.yaml's `cloud.api_key`
/// was being conflated with the short operator-typed pair code. The
/// canonical state lives in `pairing.json` (matching the Python full
/// agent's PairingManager). The cloud client now reads pairing.json on
/// every iteration so a `ados-agent-lite pair CODE` from another
/// process is picked up without restart.
#[derive(Clone, Serialize, Deserialize)]
pub struct CloudConfig {
    pub device_id: String,
    pub mqtt_broker: String,
    pub mqtt_port: u16,
    pub mqtt_use_tls: bool,
    pub convex_url: String,
    /// Path to pairing.json. Default is `/etc/ados/pairing.json` to match
    /// the Python full agent. Tests override this to a tempdir.
    #[serde(default = "default_pairing_path")]
    pub pairing_path: PathBuf,
    /// Static board + agent metadata reported on each heartbeat. Filled
    /// in once at agent startup from `agent.yaml` plus board fingerprint;
    /// the cloud client never re-reads it during the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_meta: Option<AgentMeta>,
}

/// Static metadata stamped onto every heartbeat. The GCS reads these
/// fields from `cmd_droneStatus` to render the fleet card subtitle (e.g.
/// "Luckfox Pico Zero • RV1106G3 • 256 MB"), the setup-webapp deep link,
/// and the per-drone capability matrix.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMeta {
    /// Human-readable board name from `boards/<id>/board.yaml display_name`.
    pub board_name: Option<String>,
    /// SoC variant string, e.g. `rv1106g3`, `bcm2710a1`.
    pub soc: Option<String>,
    /// Architecture, e.g. `armv7`, `aarch64`. Mirrors `uname -m`.
    pub arch: Option<String>,
    /// Total physical RAM in megabytes — static, sourced from board.yaml.
    pub ram_mb: Option<u32>,
    /// Hostname (derived once at startup; rarely changes mid-run).
    pub hostname: Option<String>,
    /// First non-loopback IPv4 the agent observed at startup. Used by
    /// the GCS to construct the setup-webapp URL when the operator
    /// clicks "Open setup wizard". Re-detected on each heartbeat so a
    /// DHCP renewal flips the URL without an agent restart.
    pub last_ip: Option<String>,
    /// mDNS hostname (`<host>.local`) for operators on the same LAN.
    pub mdns_host: Option<String>,
}

fn default_pairing_path() -> PathBuf {
    PathBuf::from(DEFAULT_PAIRING_PATH)
}

impl fmt::Debug for CloudConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CloudConfig")
            .field("device_id", &self.device_id)
            .field("mqtt_broker", &self.mqtt_broker)
            .field("mqtt_port", &self.mqtt_port)
            .field("mqtt_use_tls", &self.mqtt_use_tls)
            .field("convex_url", &self.convex_url)
            .field("pairing_path", &self.pairing_path)
            .field("agent_meta", &self.agent_meta)
            .finish()
    }
}

/// Pairing beacon payload posted to `{convex_url}/pairing/register` every
/// 30 s when the agent is unpaired. Field names are camelCase per
/// `proto/cloud/openapi.yaml` so the cloud relay parses them correctly.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
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
///
/// `outbound_fc` is the FC writer channel — frames received from the
/// cloud relay on `ados/{device_id}/mavlink/rx` are forwarded to this
/// sender, which the MAVLink router then writes to the FC serial.
/// Pass `None` if no FC is connected; cloud-received frames are then
/// logged at WARN and dropped.
pub fn spawn_cloud_client(
    config: CloudConfig,
    inbound_mavlink: broadcast::Sender<Vec<u8>>,
    outbound_fc: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
) -> Result<(), CloudError> {
    if config.device_id.is_empty() {
        return Err(CloudError::Config("device_id is required".into()));
    }

    // MQTT: publish inbound MAVLink frames to ados/{device_id}/mavlink/tx,
    // and route incoming MQTT messages on subscribed topics. Skip the loop
    // entirely when the broker is unconfigured (unpaired boot, broker URL
    // not yet supplied by the pairing flow).
    if !config.mqtt_broker.is_empty() {
        let mqtt_config = config.clone();
        let mut mavlink_rx = inbound_mavlink.subscribe();
        let fc_writer = outbound_fc.clone();
        tokio::spawn(async move {
            if let Err(e) = mqtt_publish_loop(mqtt_config, &mut mavlink_rx, fc_writer).await {
                tracing::error!(error = %e, "mqtt publish loop exited");
            }
        });
    } else {
        tracing::info!("mqtt_broker empty; skipping MQTT publish loop until paired");
    }

    // HTTPS: heartbeat (when paired) or pairing beacon (when unpaired).
    // Always spawned so the unpaired path keeps registering the device with
    // the cloud relay until the operator pairs.
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
    outbound_fc: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
) -> Result<(), CloudError> {
    let client_id = format!("ados-{}", config.device_id);
    let mut opts = MqttOptions::new(&client_id, &config.mqtt_broker, config.mqtt_port);
    opts.set_keep_alive(Duration::from_secs(60));
    // clean_session=false preserves unsent frames across reconnect — the
    // broker keeps the inflight queue. Setting true would drop any
    // mid-flight publishes during a network blip.
    opts.set_clean_session(false);
    if config.mqtt_use_tls {
        // Default rustls configuration with the platform native trust store
        // bundled by rumqttc. The agent does not pin a custom CA at v0.1.
        opts.set_transport(Transport::tls_with_default_config());
    }
    // Read the api_key from pairing.json on connect. If the agent
    // re-pairs, the next reconnect will pick up the new key — we don't
    // need a hot-swap path because rumqttc reconnects on auth failures.
    let pairing_store = PairingStore::new(&config.pairing_path);
    if let Ok(state) = pairing_store.load() {
        if let Some(key) = state.api_key.as_deref() {
            if !key.is_empty() {
                opts.set_credentials(client_id.as_str(), key);
            }
        }
    }

    let (client, mut eventloop) = AsyncClient::new(opts, 1024);
    let topic_tx = format!("ados/{}/mavlink/tx", config.device_id);

    // Subscribe to inbound topics per proto/cloud/mqtt-topics.md. Per-topic
    // QoS matches the spec: mavlink/rx is QoS 0 (fire-and-forget — broker
    // queueing defeats real-time framing), command + webrtc/offer are
    // QoS 1 (acks required for delivery).
    for (sub_topic, qos) in [
        (format!("ados/{}/mavlink/rx", config.device_id), QoS::AtMostOnce),
        (format!("ados/{}/command", config.device_id), QoS::AtLeastOnce),
        (format!("ados/{}/webrtc/offer", config.device_id), QoS::AtLeastOnce),
    ] {
        if let Err(e) = client.subscribe(&sub_topic, qos).await {
            tracing::warn!(topic = %sub_topic, error = %e, "mqtt subscribe failed");
        }
    }

    // Drive the eventloop in the background. Routes inbound publishes
    // by topic suffix:
    //
    //   `mavlink/rx`    forwarded to FC writer (drops on full queue)
    //   `command`       logged at INFO; v1 has no command surface
    //   `webrtc/offer`  logged at INFO; video pipeline lands separately
    //
    // The handle is wrapped in `AbortOnDrop` so the inner task is
    // cancelled both on a clean exit (broadcast closed) AND on an
    // abrupt drop of the outer publish loop (parent task cancelled,
    // agent shutdown). Without the guard the eventloop survived the
    // outer task as a zombie until its next broker poll completed.
    let device_id_owned = config.device_id.clone();
    let topic_rx = format!("ados/{}/mavlink/rx", device_id_owned);
    let topic_command = format!("ados/{}/command", device_id_owned);
    let topic_offer = format!("ados/{}/webrtc/offer", device_id_owned);
    let eventloop_handle = tokio::spawn(async move {
        // Capped exponential backoff for poll errors. A hard broker outage
        // (DNS down, TLS negotiation failing, link gone) would otherwise
        // storm reconnects every few seconds indefinitely. Start at 1s,
        // double on each consecutive error, ceiling at 60s. Reset on the
        // first successful poll. The progression is 1, 2, 4, 8, 16, 32,
        // 60, 60, ... — seven attempts in the first ~2 minutes, then
        // one per minute steady state.
        let mut backoff = Duration::from_secs(1);
        const MAX_BACKOFF: Duration = Duration::from_secs(60);
        loop {
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p))) => {
                    backoff = Duration::from_secs(1);
                    let topic = p.topic.as_str();
                    if topic == topic_rx {
                        if let Some(ref fc) = outbound_fc {
                            // try_send so a backed-up FC writer never
                            // stalls the cloud client. Drops are logged.
                            if let Err(e) = fc.try_send(p.payload.to_vec()) {
                                tracing::warn!(
                                    error = %e,
                                    bytes = p.payload.len(),
                                    "fc writer queue full; dropping cloud-relayed mavlink frame"
                                );
                            }
                        } else {
                            tracing::debug!(
                                bytes = p.payload.len(),
                                "received mavlink/rx frame but no FC writer wired"
                            );
                        }
                    } else if topic == topic_command {
                        tracing::info!(
                            bytes = p.payload.len(),
                            "received cloud command (no handler at v1 — dropped)"
                        );
                    } else if topic == topic_offer {
                        tracing::info!(
                            bytes = p.payload.len(),
                            "received webrtc/offer (no video pipeline at this scope)"
                        );
                    } else {
                        tracing::debug!(topic = %topic, "received message on unexpected topic");
                    }
                }
                Ok(_event) => {
                    backoff = Duration::from_secs(1);
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        backoff_secs = backoff.as_secs(),
                        "mqtt eventloop poll error; backing off"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    });

    let _eventloop_guard = AbortOnDrop(eventloop_handle);

    loop {
        match mavlink_rx.recv().await {
            Ok(frame) => {
                let frame_len = frame.len();
                let publish_fut = client.publish(&topic_tx, QoS::AtMostOnce, false, frame);
                match tokio::time::timeout(MQTT_PUBLISH_TIMEOUT, publish_fut).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        tracing::warn!(error = %e, "mqtt publish failed");
                    }
                    Err(_) => {
                        // Broker stall longer than the per-frame ceiling.
                        // Drop this frame; the broadcast channel is still
                        // delivering, so the next iteration picks up a
                        // fresher one rather than a backlogged stale one.
                        tracing::warn!(
                            broker = %config.mqtt_broker,
                            topic = %topic_tx,
                            bytes = frame_len,
                            timeout_secs = MQTT_PUBLISH_TIMEOUT.as_secs(),
                            "mqtt publish exceeded per-frame ceiling; dropping frame"
                        );
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    dropped = n,
                    broker = %config.mqtt_broker,
                    topic = %topic_tx,
                    "mqtt publisher lagging behind FC frame rate"
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::info!("mavlink broadcast closed; mqtt publish loop exiting");
                // _eventloop_guard aborts the eventloop task on drop.
                return Ok(());
            }
        }
    }
}

async fn http_loop(config: CloudConfig) -> Result<(), CloudError> {
    // Without a relay URL we have no destination. Wait quietly and let the
    // operator point us at the cloud relay (or future config-reload signal)
    // rather than burning CPU on errors.
    if config.convex_url.is_empty() {
        tracing::info!(
            "convex_url empty; HTTPS loop idle. Configure cloud.convex_url \
             in agent.yaml or pair via the setup webapp"
        );
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;

    let pairing_store = PairingStore::new(&config.pairing_path);
    let max_interval = Duration::from_secs(300);
    let mut consecutive_failures: u32 = 0;
    // Stamp the heartbeat with a wall-clock-relative uptime so the GCS can
    // tell when an agent rebooted without needing kernel boot time.
    let started_at = Instant::now();

    loop {
        // Re-read pairing state every iteration so a `ados-agent-lite pair`
        // from another process flips us from beacon to heartbeat without
        // requiring an agent restart.
        let pairing_state = pairing_store.load().ok().unwrap_or_default();
        let is_paired = pairing_state.is_paired();
        let base_interval = if is_paired {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(30)
        };

        let result = if is_paired {
            send_heartbeat(
                &client,
                &config,
                pairing_state.api_key.as_deref().unwrap_or(""),
                started_at,
            )
            .await
        } else {
            // Mint a code on the first beacon if one isn't set yet so the
            // operator has something to type into Mission Control.
            let code = match pairing_state.pairing_code {
                Some(ref c) if !c.is_empty() => c.clone(),
                _ => match pairing_store.get_or_create_code() {
                    Ok(c) => {
                        // Pair code is a pre-auth bearer; logging the live
                        // value at INFO would persist it into journalctl /
                        // syslog. Log only the length so the operator can
                        // confirm a code was minted without leaking it.
                        tracing::info!(code_length = c.len(), "pairing code minted");
                        c
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not mint pairing code; sending empty beacon");
                        String::new()
                    }
                },
            };
            send_pairing_beacon(&client, &config, &code).await
        };

        match result {
            Ok(()) => consecutive_failures = 0,
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    error = %e,
                    consecutive_failures,
                    "cloud heartbeat / beacon failed"
                );
            }
        }
        let delay = if consecutive_failures == 0 {
            base_interval
        } else {
            let exp = consecutive_failures.min(8);
            let scaled = base_interval.saturating_mul(1u32 << exp.min(8));
            scaled.min(max_interval)
        };
        tokio::time::sleep(delay).await;
    }
}

async fn send_pairing_beacon(
    client: &reqwest::Client,
    config: &CloudConfig,
    pairing_code: &str,
) -> Result<(), CloudError> {
    let url = format!("{}/pairing/register", config.convex_url.trim_end_matches('/'));
    // Beacon name prefers the operator-set board name (e.g. "Luckfox
    // Pico Zero") so the Mission Control "Add drone" dialog shows
    // something the operator recognises. Falls back to a generic label
    // when no board metadata is populated yet.
    let display_name = config
        .agent_meta
        .as_ref()
        .and_then(|m| m.board_name.as_deref())
        .unwrap_or("ADOS Lite Agent");
    let beacon = PairingBeacon {
        device_id: &config.device_id,
        pairing_code,
        api_key: "",
        name: display_name,
        version: env!("CARGO_PKG_VERSION"),
    };
    let response = client.post(&url).json(&beacon).send().await?;
    tracing::debug!(status = %response.status(), "pairing beacon sent");
    Ok(())
}

async fn send_heartbeat(
    client: &reqwest::Client,
    config: &CloudConfig,
    api_key: &str,
    started_at: Instant,
) -> Result<(), CloudError> {
    let url = format!("{}/agent/status", config.convex_url.trim_end_matches('/'));
    let body = build_heartbeat_body(config, started_at);
    let response = client
        .post(&url)
        .header("X-ADOS-Key", api_key)
        .json(&body)
        .send()
        .await?;
    tracing::debug!(status = %response.status(), "heartbeat sent");
    Ok(())
}

/// Builds the heartbeat JSON body emitted to `/agent/status`.
///
/// Static fields (board / soc / arch / ramMb / hostname) come from
/// `agent_meta` populated at agent startup. Dynamic fields (cpuPct /
/// memUsedMb / memTotalMb / socTempC) come from a fresh sysmetrics
/// tick. Network identity (lastIp, mdnsHost) is re-detected each tick
/// so a DHCP renewal flips the GCS deep-link without an agent restart.
///
/// The lite agent currently emits 16 of the 26 fields documented in
/// `proto/cloud/openapi.yaml::AgentHeartbeat`. The remaining 10 fields
/// (video state, disk usage, cpu/memory history, peripherals, remote
/// access, fcPort/fcBaud) require subsystems the lite binary does not
/// host yet and are deferred to the video-pipeline mission.
///
/// The `services` array is a static three-element snapshot because the
/// lite agent is a single tokio process: it does not supervise
/// separate systemd units the way the Python agent does, so the
/// optional cpuPercent/memoryMb/uptimeSeconds/pid per-entry fields are
/// not meaningful here.
///
/// `fcConnected` is hardcoded to `false` until a live MAVLink router
/// probe is wired in alongside the FC heartbeat work.
fn build_heartbeat_body(config: &CloudConfig, started_at: Instant) -> serde_json::Value {
    let metrics = sysmetrics::collect();
    let uptime_secs = started_at.elapsed().as_secs();
    let meta = config.agent_meta.clone().unwrap_or_default();

    serde_json::json!({
        "deviceId": config.device_id,
        "version": env!("CARGO_PKG_VERSION"),
        "agentVersion": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": uptime_secs,
        "runtimeMode": "lite",
        // Static board metadata. Field names match proto/cloud/openapi.yaml
        // and the Python full agent so the GCS fleet card renders the
        // same shape regardless of which agent is talking.
        "boardName": meta.board_name,
        "boardSoc": meta.soc,
        "boardArch": meta.arch,
        "boardRamMb": meta.ram_mb,
        // Network identity.
        "hostname": meta.hostname,
        "lastIp": meta.last_ip,
        "mdnsHost": meta.mdns_host,
        // Live metrics — same keys the Python agent emits.
        "cpuPercent": metrics.cpu_pct,
        "memoryUsedMb": metrics.mem_used_mb,
        "memoryTotalMb": metrics.mem_total_mb,
        "temperature": metrics.soc_temp_c,
        // Static composition snapshot. The lite agent is a single
        // process; these names map to internal tokio tasks rather
        // than systemd units.
        "services": [
            {"name": "mavlink-router", "status": "running", "category": "system"},
            {"name": "cloud-client", "status": "running", "category": "system"},
            {"name": "http-api", "status": "running", "category": "system"}
        ],
        // No live FC heartbeat probe at v0.1; flips dynamic when the
        // MAVLink router exposes connection state.
        "fcConnected": false,
    })
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
            pairing_path: PathBuf::from("/etc/ados/pairing.json"),
            agent_meta: Some(AgentMeta {
                board_name: Some("Luckfox Pico Zero".into()),
                soc: Some("rv1106g3".into()),
                arch: Some("armv7".into()),
                ram_mb: Some(256),
                hostname: Some("luckfox".into()),
                last_ip: Some("192.168.200.225".into()),
                mdns_host: Some("luckfox.local".into()),
            }),
        };
        let serialized = serde_json::to_string(&original).unwrap();
        let restored: CloudConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(restored.device_id, original.device_id);
        assert_eq!(restored.mqtt_broker, original.mqtt_broker);
        assert_eq!(restored.pairing_path, original.pairing_path);
        let meta = restored.agent_meta.expect("agent_meta survives round-trip");
        assert_eq!(meta.board_name.as_deref(), Some("Luckfox Pico Zero"));
        assert_eq!(meta.ram_mb, Some(256));
    }

    #[test]
    fn cloud_config_omits_agent_meta_when_unset() {
        // Older agent.yaml files won't have the metadata block. The
        // config must still serialize and deserialize cleanly.
        let original = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: false,
            convex_url: String::new(),
            pairing_path: PathBuf::from("/tmp/pair.json"),
            agent_meta: None,
        };
        let serialized = serde_json::to_string(&original).unwrap();
        // The serialized form should not contain the field at all
        // (skip_serializing_if).
        assert!(!serialized.contains("agentMeta"));
        let restored: CloudConfig = serde_json::from_str(&serialized).unwrap();
        assert!(restored.agent_meta.is_none());
    }

    #[test]
    fn heartbeat_body_includes_services_and_fc_connected() {
        // Defense-in-depth: the GCS fleet card relies on these two
        // fields to render the services panel and the FC-connected
        // badge. A future refactor that drops them silently would
        // regress the GCS without surfacing in unit-test output
        // unless we pin the shape here.
        let config = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: "broker.example".into(),
            mqtt_port: 8883,
            mqtt_use_tls: true,
            convex_url: "https://relay.example".into(),
            pairing_path: PathBuf::from("/etc/ados/pairing.json"),
            agent_meta: None,
        };
        let body = build_heartbeat_body(&config, Instant::now());

        // fcConnected is the static `false` placeholder until a live
        // MAVLink router probe lands.
        assert_eq!(body.get("fcConnected"), Some(&serde_json::Value::Bool(false)));

        // services is a 3-element array; each entry has name + status
        // + category, matching the OpenAPI schema.
        let services = body
            .get("services")
            .and_then(|v| v.as_array())
            .expect("services field is an array");
        assert_eq!(services.len(), 3);
        let names: Vec<&str> = services
            .iter()
            .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
            .collect();
        assert_eq!(names, vec!["mavlink-router", "cloud-client", "http-api"]);
        for entry in services {
            assert_eq!(entry.get("status").and_then(|v| v.as_str()), Some("running"));
            assert_eq!(entry.get("category").and_then(|v| v.as_str()), Some("system"));
        }

        // runtimeMode is "lite" so the GCS knows which agent flavor
        // is reporting and renders the right pill.
        assert_eq!(
            body.get("runtimeMode").and_then(|v| v.as_str()),
            Some("lite")
        );
    }

    #[test]
    fn empty_device_id_is_rejected() {
        let bad = CloudConfig {
            device_id: String::new(),
            mqtt_broker: "broker.example".into(),
            mqtt_port: 8883,
            mqtt_use_tls: true,
            convex_url: "https://relay.example".into(),
            pairing_path: PathBuf::from("/etc/ados/pairing.json"),
            agent_meta: None,
        };
        let (tx, _rx) = broadcast::channel(8);
        let err = spawn_cloud_client(bad, tx, None).expect_err("empty device_id should fail");
        match err {
            CloudError::Config(msg) => assert!(msg.contains("device_id")),
            _ => panic!("expected Config error, got {:?}", err),
        }
    }
}
