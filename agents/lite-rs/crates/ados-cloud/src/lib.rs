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

pub mod handlers;
pub mod sysmetrics;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ados_setup::diag::{now_unix_seconds, DiagState};
use ados_setup::pairing::{is_valid_api_key, PairingStore};
use rumqttc::{AsyncClient, MqttOptions, QoS, Transport};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{broadcast, RwLock};

pub use handlers::{
    CommandHandler, CommandOutcome, RebootProvider, SystemRebootProvider, WebRtcOffer,
};

const DEFAULT_PAIRING_PATH: &str = "/etc/ados/pairing.json";

/// Default MQTT keepalive in seconds. rumqttc sends a PINGREQ at half this
/// interval; the broker disconnects clients that go silent for more than
/// 1.5x the value (per the MQTT 3.1.1 spec). 60 s balances cellular
/// radio-on time against detection latency.
const DEFAULT_MQTT_KEEPALIVE_SECS: u64 = 60;
/// Lower bound for `mqtt_keepalive_secs`. Below 10 s the rumqttc PINGREQ
/// rate climbs into the same order as the FC frame rate; the keepalive
/// stops being a sanity check and starts adding bandwidth pressure.
#[allow(dead_code)]
const MIN_MQTT_KEEPALIVE_SECS: u64 = 10;
/// Upper bound for `mqtt_keepalive_secs`. The MQTT 3.1.1 spec caps
/// keepalive at 18 hours; most brokers reject anything past 1800 s with
/// CONNACK return-code 3.
#[allow(dead_code)]
const MAX_MQTT_KEEPALIVE_SECS: u64 = 1800;

/// Default TCP connect-phase ceiling for the cloud HTTP client. Distinct
/// from the total-request timeout: a stalled DNS lookup or a half-open
/// TCP handshake should fail fast and let exponential backoff pick a
/// fresh attempt rather than burning the full request timeout.
const DEFAULT_HTTP_CONNECT_TIMEOUT_SECS: u64 = 3;
/// Default total-request timeout for the cloud HTTP client. Covers
/// DNS + connect + TLS + body read together.
const DEFAULT_HTTP_REQUEST_TIMEOUT_SECS: u64 = 10;

/// Per-frame ceiling on the MQTT publish call. A stalled broker (TLS
/// handshake hang, congested upstream link, half-open socket) would
/// otherwise let the publish future park forever while the FC keeps
/// producing frames at ~30 Hz. After the timeout the frame is logged
/// and dropped; the broadcast channel naturally moves on to the next
/// one rather than queueing stale telemetry.
const MQTT_PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);

/// Documented capacity of the inbound MAVLink broadcast channel that the
/// router constructs and this loop subscribes to. Mirrors the constant
/// in the router crate. Used purely as a denominator on lag warnings so
/// an operator reading journalctl can size the drop against the buffer.
/// If the router and this constant ever drift, the warning's denominator
/// is misleading but no behavior changes.
const MAVLINK_BROADCAST_CHANNEL_CAPACITY: usize = 1024;

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
    /// MQTT keepalive in seconds. Used as the `set_keep_alive` argument
    /// when the publish loop opens its connection. Defaults to 60 s.
    /// Validated to fall within `[10, 1800]` at config-load time.
    /// Cellular operators may raise this to reduce radio-on time;
    /// local-broker operators may lower it for faster failure detection.
    #[serde(default = "default_mqtt_keepalive_secs")]
    pub mqtt_keepalive_secs: u64,
    /// TCP connect-phase ceiling on the cloud HTTP client. Distinct from
    /// the total-request timeout: a half-open TCP socket or stalled DNS
    /// resolution should fail fast and let exponential backoff pick a
    /// fresh attempt. Default 3 s.
    #[serde(default = "default_http_connect_timeout_secs")]
    pub connect_timeout_secs: u64,
    /// Total-request timeout on the cloud HTTP client. Covers DNS,
    /// connect, TLS, and body read together. Default 10 s.
    #[serde(default = "default_http_request_timeout_secs")]
    pub request_timeout_secs: u64,
    /// Runtime gate for the inbound `reboot` cloud command. Defaults
    /// to `false` so a stock install never reboots the host on a
    /// stray cloud message; an operator opts in by editing
    /// `agent.yaml`. Field name mirrors `cloud.allow_reboot` in the
    /// Python full agent's config schema.
    #[serde(default)]
    pub allow_reboot: bool,
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

fn default_mqtt_keepalive_secs() -> u64 {
    DEFAULT_MQTT_KEEPALIVE_SECS
}

fn default_http_connect_timeout_secs() -> u64 {
    DEFAULT_HTTP_CONNECT_TIMEOUT_SECS
}

fn default_http_request_timeout_secs() -> u64 {
    DEFAULT_HTTP_REQUEST_TIMEOUT_SECS
}

/// Apply default substitution + bound-clamp to the MQTT keepalive read
/// off `CloudConfig`. Returns the value that should be passed to
/// `set_keep_alive`. A zero value (operator omitted the field; serde's
/// `#[serde(default)]` filled it in but a hand-edited yaml could still
/// send `mqtt_keepalive_secs: 0` through) is treated as "use default".
///
/// Reserved for the SIGHUP hot-reload validator path; the publish loop
/// reads the value verbatim today.
#[allow(dead_code)]
fn resolve_mqtt_keepalive_secs(secs: u64) -> u64 {
    let raw = if secs == 0 {
        DEFAULT_MQTT_KEEPALIVE_SECS
    } else {
        secs
    };
    raw.clamp(MIN_MQTT_KEEPALIVE_SECS, MAX_MQTT_KEEPALIVE_SECS)
}

/// Apply default substitution to the connect-phase timeout. A zero
/// value is treated as "use default" so a hand-edited agent.yaml that
/// drops `0` cannot disable the connect timeout entirely.
fn resolve_connect_timeout_secs(secs: u64) -> u64 {
    if secs == 0 {
        DEFAULT_HTTP_CONNECT_TIMEOUT_SECS
    } else {
        secs
    }
}

/// Apply default substitution to the total-request timeout. A zero
/// value is treated as "use default" — same rationale as the connect
/// timeout.
fn resolve_request_timeout_secs(secs: u64) -> u64 {
    if secs == 0 {
        DEFAULT_HTTP_REQUEST_TIMEOUT_SECS
    } else {
        secs
    }
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
            .field("mqtt_keepalive_secs", &self.mqtt_keepalive_secs)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("allow_reboot", &self.allow_reboot)
            .finish()
    }
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: true,
            convex_url: String::new(),
            pairing_path: PathBuf::from(DEFAULT_PAIRING_PATH),
            agent_meta: None,
            mqtt_keepalive_secs: default_mqtt_keepalive_secs(),
            connect_timeout_secs: default_http_connect_timeout_secs(),
            request_timeout_secs: default_http_request_timeout_secs(),
            allow_reboot: false,
        }
    }
}

/// Shared cloud-config handle. The agent binary holds the only writer
/// (the SIGHUP hot-reload path); the cloud-client tasks hold readers
/// and re-read on each iteration so an operator-edited agent.yaml takes
/// effect on the next tick without a process restart.
///
/// Type alias rather than a newtype so existing `Arc::clone` patterns at
/// the call site keep working; the shape is intentionally narrow.
pub type SharedCloudConfig = Arc<RwLock<CloudConfig>>;

/// Convenience constructor for the agent binary: wraps `CloudConfig` in
/// the shared `Arc<RwLock<_>>` so a single call site does not have to
/// re-import `tokio::sync::RwLock`.
pub fn shared_cloud_config(config: CloudConfig) -> SharedCloudConfig {
    Arc::new(RwLock::new(config))
}

/// Pairing beacon payload posted to `{convex_url}/pairing/register` every
/// 30 s when the agent is unpaired. Field names are camelCase per
/// `proto/cloud/openapi.yaml` so the cloud relay parses them correctly.
///
/// The optional fields (`board`, `tier`, `mdns_host`, `local_ip`) mirror
/// what the Python full agent emits so Mission Control's "Add drone"
/// dialog can render the same unpaired card regardless of which agent is
/// reporting. `local_ip` is what the GCS uses to construct the deep-link
/// "Open setup wizard" button on the unpaired card; `mdns_host` is the
/// fallback when the operator is on the same LAN with mDNS available.
/// Skip-when-None keeps the beacon body byte-compatible with older
/// relays that didn't expect these keys.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingBeacon<'a> {
    pub device_id: &'a str,
    pub pairing_code: &'a str,
    pub api_key: &'a str,
    pub name: &'a str,
    pub version: &'a str,
    /// Structured board identifier (e.g. `"Luckfox Pico Zero"`). The
    /// `name` field stays human-readable; this is the value the GCS
    /// fleet card uses for the subtitle pill once the device pairs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<&'a str>,
    /// Capability tier (`0`/`1`/`2`/`3`). The lite agent does not yet
    /// compute a tier value — the field is reserved for the upcoming
    /// video pipeline mission to populate. Stays `None` at v0.1 so the
    /// beacon body simply omits the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<i32>,
    /// mDNS hostname (`<host>.local`) for operators on the same LAN.
    /// Lifted from `agent_meta.mdns_host`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mdns_host: Option<&'a str>,
    /// First non-loopback IPv4 the agent observed at startup. Used by
    /// the GCS deep-link to construct the setup-webapp URL on the
    /// unpaired drone card.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_ip: Option<&'a str>,
}

/// Optional channels the cloud client routes inbound MQTT messages onto.
/// Bundled into a struct so the public `spawn_cloud_client` signature
/// stays narrow as new inbound topic handlers are added (today: command
/// + webrtc/offer; future: peer-to-peer LAN signaling).
#[derive(Default)]
pub struct InboundChannels {
    /// FC writer mpsc. Frames received on `ados/{device_id}/mavlink/rx`
    /// are forwarded here; the MAVLink router then writes them to the
    /// FC serial. `None` makes the dispatcher log + drop inbound
    /// MAVLink traffic.
    pub fc_writer: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Heartbeat trigger mpsc. The `command` handler fires this on a
    /// `status_request` envelope so an immediate heartbeat goes out
    /// instead of waiting for the next 5 s tick. `None` makes
    /// `status_request` a no-op (logged at INFO).
    pub heartbeat_trigger: Option<tokio::sync::mpsc::Sender<()>>,
    /// WebRTC offer route. Reserved for the future video mission;
    /// today the lite agent does not host a peer so this is always
    /// `None`. When `None` the `webrtc/offer` handler synthesizes a
    /// `rejected` answer with `webrtc-not-supported-on-lite`.
    pub webrtc_route: Option<tokio::sync::mpsc::Sender<WebRtcOffer>>,
}

/// Spawn the cloud client tasks: MQTT publish loop, HTTPS heartbeat, and
/// pairing beacon. Returns immediately. The tasks run until the inbound
/// broadcast `Sender` is dropped or the agent process exits.
///
/// `inbound_mavlink` is the broadcast channel the FC reader publishes
/// frames on. The MQTT publish loop subscribes and forwards each
/// frame to the cloud relay on `ados/{device_id}/mavlink/tx`.
///
/// `inbound_channels` carries the optional handler routes for inbound
/// MQTT topics. See `InboundChannels` for per-field semantics.
///
/// `diag` is the shared diagnostic state the agent binary constructs
/// for the `/api/v1/diag` handler. The MQTT publish loop and HTTPS
/// heartbeat update its atomic counters so `mqtt.connected_recently`,
/// `cloud_relay.last_heartbeat_at`, and `cloud_relay.consecutive_failures`
/// reflect the live cloud-relay path instead of staying at the seeded
/// default values forever.
pub fn spawn_cloud_client(
    config: CloudConfig,
    inbound_mavlink: broadcast::Sender<Vec<u8>>,
    inbound_channels: InboundChannels,
    diag: Arc<DiagState>,
) -> Result<(), CloudError> {
    if config.device_id.is_empty() {
        return Err(CloudError::Config("device_id is required".into()));
    }

    // URL-scheme validation. The convex_url and mqtt_broker fields flow
    // from operator-supplied agent.yaml. Reject schemes that would shoot
    // requests off into the local filesystem; warn loudly when the
    // operator has wired plaintext transport against a non-loopback
    // endpoint (credentials would otherwise ship in cleartext).
    if !config.convex_url.is_empty() {
        let url = config.convex_url.as_str();
        if url.starts_with("https://") {
            // Encrypted; fine.
        } else if url.starts_with("http://") {
            tracing::warn!(
                convex_url = %url,
                "convex_url uses unencrypted scheme; credentials will be \
                 transmitted in cleartext"
            );
        } else {
            return Err(CloudError::Config(
                "convex_url must be http(s)".into(),
            ));
        }
    }

    // mqtt_broker is a host string (no scheme). When TLS is off and the
    // broker is not loopback, log a one-shot warning so the operator
    // sees the cleartext exposure in journalctl.
    if !config.mqtt_broker.is_empty() && !config.mqtt_use_tls {
        let host = config.mqtt_broker.as_str();
        let is_loopback = host == "127.0.0.1"
            || host == "localhost"
            || host == "::1";
        if !is_loopback {
            tracing::warn!(
                broker = %host,
                "mqtt_use_tls=false on a non-loopback broker; credentials \
                 will be transmitted in cleartext"
            );
        }
    }

    // MQTT: publish inbound MAVLink frames to ados/{device_id}/mavlink/tx,
    // and route incoming MQTT messages on subscribed topics. Skip the loop
    // entirely when the broker is unconfigured (unpaired boot, broker URL
    // not yet supplied by the pairing flow).
    if !config.mqtt_broker.is_empty() {
        let mqtt_config = config.clone();
        let mut mavlink_rx = inbound_mavlink.subscribe();
        let fc_writer = inbound_channels.fc_writer.clone();
        let heartbeat_trigger = inbound_channels.heartbeat_trigger.clone();
        let webrtc_route = inbound_channels.webrtc_route.clone();
        let mqtt_diag = diag.clone();
        tokio::spawn(async move {
            if let Err(e) = mqtt_publish_loop(
                mqtt_config,
                &mut mavlink_rx,
                fc_writer,
                heartbeat_trigger,
                webrtc_route,
                mqtt_diag,
            )
            .await
            {
                tracing::error!(error = %e, "mqtt publish loop exited");
            }
        });
    } else {
        tracing::info!("mqtt_broker empty; skipping MQTT publish loop until paired");
    }

    // HTTPS: heartbeat (when paired) or pairing beacon (when unpaired).
    // Always spawned so the unpaired path keeps registering the device with
    // the cloud relay until the operator pairs. Wrap the config in a
    // SharedCloudConfig so a future SIGHUP hot-reload path can swap the
    // broker / convex_url at runtime without restarting the agent.
    let http_config = shared_cloud_config(config);
    let http_diag = diag;
    tokio::spawn(async move {
        if let Err(e) = http_loop(http_config, http_diag).await {
            tracing::error!(error = %e, "https loop exited");
        }
    });

    Ok(())
}

async fn mqtt_publish_loop(
    config: CloudConfig,
    mavlink_rx: &mut broadcast::Receiver<Vec<u8>>,
    outbound_fc: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    heartbeat_trigger: Option<tokio::sync::mpsc::Sender<()>>,
    webrtc_route: Option<tokio::sync::mpsc::Sender<handlers::WebRtcOffer>>,
    diag: Arc<DiagState>,
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
    //
    // Defense-in-depth: gate the credential on the byte-shape validator
    // before passing it to rumqttc. A malformed key (control bytes,
    // missing prefix, wrong length — say a partial write or a hand-edited
    // pairing.json) would otherwise either flow into MQTT's CONNECT as
    // broken creds or be silently mangled at the wire. We log a prefix
    // (never the full bearer) and run the loop unauthenticated; the
    // broker will close the socket and the failure will be visible.
    let pairing_store = PairingStore::new(&config.pairing_path);
    if let Ok(state) = pairing_store.load() {
        if let Some(key) = state.api_key.as_deref() {
            if key.is_empty() {
                // No key set — beacon path drives pairing, not this loop.
            } else if is_valid_api_key(key) {
                opts.set_credentials(client_id.as_str(), key);
            } else {
                let key_prefix = if key.len() >= 8 { &key[..8] } else { key };
                tracing::warn!(
                    key_prefix = %format!("{}...", key_prefix),
                    key_len = key.len(),
                    "api_key shape invalid; running unauthenticated MQTT loop \
                     (re-pair via the setup webapp or `ados-agent-lite pair`)"
                );
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
    //   `command`       JSON envelope decoded; reboot + status_request
    //                   handled, dedup'd by request_id
    //   `webrtc/offer`  rejected on lite (no peer); answer published
    //                   on `webrtc/answer` with the right reason code
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
    let topic_answer = format!("ados/{}/webrtc/answer", device_id_owned);

    // Stateful command handler. Built once per publish-loop instance
    // so the dedup cache survives across reconnects (a re-pair would
    // build a new client + new handler; that is desired, the cache
    // need not span re-pair operations).
    let reboot_provider: Arc<dyn handlers::RebootProvider> =
        Arc::new(handlers::SystemRebootProvider);
    let command_handler = Arc::new(handlers::CommandHandler::new(
        heartbeat_trigger,
        reboot_provider,
        config.allow_reboot,
    ));

    // Publish handle for the eventloop's webrtc/answer reply path.
    // `AsyncClient` is `Clone` (rumqttc 0.25); the second handle
    // shares the same outbound queue.
    let answer_client = client.clone();
    let outbound_fc_eventloop = outbound_fc.clone();

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
                        handlers::handle_mavlink_rx(
                            p.payload.to_vec(),
                            outbound_fc_eventloop.as_ref(),
                        );
                    } else if topic == topic_command {
                        let outcome = command_handler.dispatch(&p.payload).await;
                        tracing::debug!(
                            ?outcome,
                            bytes = p.payload.len(),
                            "command dispatch complete"
                        );
                    } else if topic == topic_offer {
                        let answer =
                            handlers::handle_webrtc_offer(&p.payload, webrtc_route.as_ref());
                        if !answer.is_null() {
                            // Best-effort publish of the answer. The
                            // reject path is fire-and-forget; the
                            // operator-facing failure is "GCS shows a
                            // pending offer that never resolved" which
                            // already has a GCS-side timeout.
                            let body = match serde_json::to_vec(&answer) {
                                Ok(b) => b,
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "failed to serialize webrtc/answer; skipping publish"
                                    );
                                    continue;
                                }
                            };
                            if let Err(e) = answer_client
                                .publish(&topic_answer, QoS::AtLeastOnce, false, body)
                                .await
                            {
                                tracing::warn!(
                                    error = %e,
                                    topic = %topic_answer,
                                    "failed to publish webrtc/answer reject"
                                );
                            }
                        }
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
                    Ok(Ok(())) => {
                        // Stamp the diag state so `/api/v1/diag` reports
                        // `mqtt.connected_recently=true`. Cheap atomic
                        // store on the publish hot path.
                        diag.record_mqtt_publish(now_unix_seconds());
                    }
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
                // `mavlink_rx.len()` is the number of messages still in
                // the channel that this receiver hasn't consumed yet —
                // a live snapshot of how far behind we are at the moment
                // the lag was reported. `channel_capacity` is the static
                // slot count the router allocated. Together they tell an
                // operator whether the lag is brushing the wall (channel
                // saturated, persistent producer pressure) or whether
                // we briefly fell off and recovered.
                tracing::warn!(
                    dropped = n,
                    pending = mavlink_rx.len(),
                    channel_capacity = MAVLINK_BROADCAST_CHANNEL_CAPACITY,
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

/// Outcome of a single `/agent/status` POST. The dispatcher in
/// `http_loop` uses this to distinguish a revoked-key 401/403 (which
/// should clear the local pairing state and revert to the beacon flow)
/// from a transient network failure (which should increment the
/// failure counter and back off).
#[derive(Debug, PartialEq, Eq)]
enum HeartbeatOutcome {
    /// 2xx — heartbeat accepted by the relay.
    Ok,
    /// 401 or 403 — the api_key is no longer valid. The caller should
    /// clear pairing state via `PairingStore::unpair` so the next loop
    /// iteration falls back to the pairing beacon path.
    Unauthorized,
}

async fn http_loop(
    config: SharedCloudConfig,
    diag: Arc<DiagState>,
) -> Result<(), CloudError> {
    // The pairing path is hot-reload-unsafe (changing it mid-run would
    // strand the cloud loop reading from an old file while the rest of
    // the agent talks to a new one). Snapshot once at startup and keep
    // pointing at the original path until the operator restarts.
    let initial = config.read().await.clone();

    let pairing_store = PairingStore::new(&initial.pairing_path);
    let max_interval = Duration::from_secs(300);
    let mut consecutive_failures: u32 = 0;
    // Stamp the heartbeat with a wall-clock-relative uptime so the GCS can
    // tell when an agent rebooted without needing kernel boot time.
    let started_at = Instant::now();

    // Build the reqwest client lazily so we can rebuild it cleanly when
    // a SIGHUP changes the timeout fields. `None` means "convex_url was
    // empty when we last looked". We re-check the shared config every
    // 60 s so a SIGHUP that populates `convex_url` (for an offline-
    // bootstrapped agent the operator paired via the webapp) is picked
    // up without an agent restart.
    let mut http_client: Option<HttpClientWithTimeouts> = None;

    loop {
        // Per-tick snapshot of the live config. Cheap clone (a handful
        // of strings + ints); the read lock is held only long enough
        // to copy out.
        let live = config.read().await.clone();

        if live.convex_url.is_empty() {
            // No destination yet. Drop the client (if any) so a future
            // rebuild after the operator pairs picks up the latest
            // timeouts. Sleep before re-checking; the loop must not
            // hot-spin on the empty-URL path.
            if http_client.is_some() {
                tracing::info!(
                    "convex_url cleared; HTTPS loop idling until SIGHUP \
                     supplies a relay URL"
                );
            } else {
                tracing::info!(
                    "convex_url empty; HTTPS loop idle. Configure cloud.convex_url \
                     in agent.yaml or pair via the setup webapp"
                );
            }
            http_client = None;
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }

        // Rebuild the reqwest client when timeouts change OR when no
        // client exists yet. Both timeouts go through resolve_*_secs so
        // a 0 in agent.yaml resolves to the default rather than
        // disabling the timeout entirely.
        let connect_secs = resolve_connect_timeout_secs(live.connect_timeout_secs);
        let request_secs = resolve_request_timeout_secs(live.request_timeout_secs);
        let needs_rebuild = http_client
            .as_ref()
            .map(|c| c.connect_secs != connect_secs || c.request_secs != request_secs)
            .unwrap_or(true);
        if needs_rebuild {
            match build_http_client(connect_secs, request_secs) {
                Ok(client) => {
                    if let Some(prev) = http_client.as_ref() {
                        tracing::info!(
                            old_connect_secs = prev.connect_secs,
                            new_connect_secs = connect_secs,
                            old_request_secs = prev.request_secs,
                            new_request_secs = request_secs,
                            "rebuilding cloud HTTP client with updated timeouts"
                        );
                    } else {
                        tracing::info!(
                            connect_secs = connect_secs,
                            request_secs = request_secs,
                            "cloud HTTP client built"
                        );
                    }
                    http_client = Some(HttpClientWithTimeouts {
                        client,
                        connect_secs,
                        request_secs,
                    });
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        connect_secs = connect_secs,
                        request_secs = request_secs,
                        "could not build cloud HTTP client; backing off"
                    );
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
            }
        }

        // Safe by construction: the rebuild branch above either
        // populated `http_client` or `continue`d before fall-through.
        let Some(active) = http_client.as_ref() else {
            continue;
        };

        run_http_tick(
            &active.client,
            &live,
            &pairing_store,
            started_at,
            &mut consecutive_failures,
            max_interval,
            &diag,
        )
        .await;
    }
}

/// reqwest client paired with the timeout values it was built with. The
/// http_loop checks `(connect_secs, request_secs)` on every iteration and
/// rebuilds the client when either field changes via SIGHUP hot-reload.
struct HttpClientWithTimeouts {
    client: reqwest::Client,
    connect_secs: u64,
    request_secs: u64,
}

/// Construct a reqwest client with split connect-phase + total-request
/// timeouts. Extracted so the http_loop can rebuild on hot-reload and
/// unit tests can pin the config-load path.
fn build_http_client(
    connect_secs: u64,
    request_secs: u64,
) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(connect_secs))
        .timeout(Duration::from_secs(request_secs))
        .build()
}

/// One iteration of the HTTPS loop: read pairing state, send heartbeat
/// or beacon, update the failure counter, and sleep the appropriate
/// backoff. Extracted so unit tests can drive a single tick without
/// running the unbounded `loop {}`.
async fn run_http_tick(
    client: &reqwest::Client,
    config: &CloudConfig,
    pairing_store: &PairingStore,
    started_at: Instant,
    consecutive_failures: &mut u32,
    max_interval: Duration,
    diag: &Arc<DiagState>,
) {
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

    if is_paired {
        let api_key = pairing_state.api_key.as_deref().unwrap_or("");
        // Defense-in-depth: shape-validate the api_key before it lands
        // in an HTTP header. reqwest's `HeaderValue::from_str` rejects
        // control bytes, `\r\n`, and `\0` with `InvalidHeaderValue`,
        // which would surface as a `reqwest::Error` retried forever
        // without ever reaching the wire. Skip the heartbeat, log a
        // prefix-only warning, and increment consecutive_failures so
        // the exponential backoff kicks in immediately.
        if !is_valid_api_key(api_key) {
            let key_prefix = if api_key.len() >= 8 {
                &api_key[..8]
            } else {
                api_key
            };
            *consecutive_failures = consecutive_failures.saturating_add(1);
            tracing::warn!(
                key_prefix = %format!("{}...", key_prefix),
                key_len = api_key.len(),
                consecutive_failures = *consecutive_failures,
                "api_key shape invalid; skipping heartbeat (re-pair via the \
                 setup webapp or `ados-agent-lite pair`)"
            );
            let delay = {
                let exp = (*consecutive_failures).min(8);
                let scaled = base_interval.saturating_mul(1u32 << exp.min(8));
                scaled.min(max_interval)
            };
            tokio::time::sleep(delay).await;
            return;
        }
        match send_heartbeat(client, config, api_key, started_at).await {
            Ok(HeartbeatOutcome::Ok) => {
                *consecutive_failures = 0;
                // Stamp the diag state so `/api/v1/diag` reports the
                // live cloud-relay heartbeat instead of the seeded
                // never-published default. record_cloud_heartbeat also
                // resets the consecutive-failure atomic counter.
                diag.record_cloud_heartbeat(now_unix_seconds());
            }
            Ok(HeartbeatOutcome::Unauthorized) => {
                // The cloud relay rejected our api_key. Most likely the
                // operator clicked "Remove drone" in Mission Control or
                // rotated the device. Clearing local pairing state lets
                // the next iteration mint a fresh pair code and emit a
                // beacon so the operator can re-claim the device. Log
                // only a key prefix so journalctl never carries the
                // full bearer.
                let key_prefix = if api_key.len() >= 13 {
                    // `ados_` + 8 chars
                    &api_key[..13]
                } else {
                    api_key
                };
                let url = format!(
                    "{}/agent/status",
                    config.convex_url.trim_end_matches('/')
                );
                tracing::warn!(
                    url = %url,
                    key_prefix = %format!("{}...", key_prefix),
                    "cloud relay rejected api_key (401/403); clearing local \
                     pairing state and falling back to pairing beacon"
                );
                if let Err(e) = pairing_store.unpair() {
                    tracing::warn!(
                        error = %e,
                        "failed to clear pairing state after 401/403; \
                         next iteration will retry"
                    );
                }
                // Treat this as a recoverable handoff, not a network
                // failure: zero the counter so the next beacon goes
                // out on the unpaired-base interval rather than an
                // exponentially-stretched delay. Record the failure on
                // the diag side so an operator hitting `/api/v1/diag`
                // during the unpair handoff sees a non-zero counter
                // (the diag state survives the local-counter reset).
                *consecutive_failures = 0;
                diag.record_cloud_failure();
            }
            Err(e) => {
                *consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    error = %e,
                    consecutive_failures = *consecutive_failures,
                    "cloud heartbeat failed"
                );
                diag.record_cloud_failure();
            }
        }
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
        match send_pairing_beacon(client, config, &code).await {
            Ok(()) => *consecutive_failures = 0,
            Err(e) => {
                *consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    error = %e,
                    consecutive_failures = *consecutive_failures,
                    "cloud pairing beacon failed"
                );
            }
        }
    }

    let delay = if *consecutive_failures == 0 {
        base_interval
    } else {
        let exp = (*consecutive_failures).min(8);
        let scaled = base_interval.saturating_mul(1u32 << exp.min(8));
        scaled.min(max_interval)
    };
    tokio::time::sleep(delay).await;
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
    // Structured metadata mirrored to Mission Control's "Add drone"
    // dialog: `board` for the subtitle pill, `mdns_host` + `local_ip`
    // for the deep-link "Open setup wizard" button. Tier is reserved
    // for a future capability-detection pass and stays `None` at v0.1.
    let meta = config.agent_meta.as_ref();
    let board = meta.and_then(|m| m.board_name.as_deref());
    let mdns_host = meta.and_then(|m| m.mdns_host.as_deref());
    let local_ip = meta.and_then(|m| m.last_ip.as_deref());
    let beacon = PairingBeacon {
        device_id: &config.device_id,
        pairing_code,
        api_key: "",
        name: display_name,
        version: env!("CARGO_PKG_VERSION"),
        board,
        tier: None,
        mdns_host,
        local_ip,
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
) -> Result<HeartbeatOutcome, CloudError> {
    let url = format!("{}/agent/status", config.convex_url.trim_end_matches('/'));
    let body = build_heartbeat_body(config, started_at);
    let response = client
        .post(&url)
        .header("X-ADOS-Key", api_key)
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    tracing::debug!(status = %status, "heartbeat sent");
    if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        return Ok(HeartbeatOutcome::Unauthorized);
    }
    // Any other non-2xx becomes a CloudError::Http so the loop counts
    // it as a transient failure and applies exponential backoff.
    response.error_for_status()?;
    Ok(HeartbeatOutcome::Ok)
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
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
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
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
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
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
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
    fn pairing_beacon_serializes_optional_fields_camelcase() {
        // The "Add drone" dialog in Mission Control reads four optional
        // fields off the unpaired beacon to render the deep-link "Open
        // setup wizard" button. Pin the JSON shape so a future refactor
        // that drops a field surfaces here instead of silently breaking
        // the dialog. All four optional fields populated.
        let beacon = PairingBeacon {
            device_id: "test-device",
            pairing_code: "ABC123",
            api_key: "",
            name: "Luckfox Pico Zero",
            version: "0.1.0",
            board: Some("Luckfox Pico Zero"),
            tier: Some(2),
            mdns_host: Some("luckfox.local"),
            local_ip: Some("192.168.200.225"),
        };
        let json = serde_json::to_value(&beacon).unwrap();
        let obj = json.as_object().expect("beacon serializes to an object");
        // Existing five fields are unchanged.
        assert_eq!(obj.get("deviceId").and_then(|v| v.as_str()), Some("test-device"));
        assert_eq!(obj.get("pairingCode").and_then(|v| v.as_str()), Some("ABC123"));
        assert_eq!(obj.get("apiKey").and_then(|v| v.as_str()), Some(""));
        assert_eq!(obj.get("name").and_then(|v| v.as_str()), Some("Luckfox Pico Zero"));
        assert_eq!(obj.get("version").and_then(|v| v.as_str()), Some("0.1.0"));
        // New optional fields surface as camelCase keys.
        assert_eq!(obj.get("board").and_then(|v| v.as_str()), Some("Luckfox Pico Zero"));
        assert_eq!(obj.get("tier").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(obj.get("mdnsHost").and_then(|v| v.as_str()), Some("luckfox.local"));
        assert_eq!(obj.get("localIp").and_then(|v| v.as_str()), Some("192.168.200.225"));
        // The serialized object should expose exactly nine keys when
        // every optional is populated. Pinning the count guards against
        // a stray extra key landing in the wire format.
        assert_eq!(obj.len(), 9, "fully-populated beacon has 9 keys");
    }

    #[test]
    fn pairing_beacon_omits_optional_fields_when_unset() {
        // Older relays don't expect the new keys; skip-when-None keeps
        // the beacon body byte-compatible.
        let beacon = PairingBeacon {
            device_id: "test-device",
            pairing_code: "ABC123",
            api_key: "",
            name: "ADOS Lite Agent",
            version: "0.1.0",
            board: None,
            tier: None,
            mdns_host: None,
            local_ip: None,
        };
        let serialized = serde_json::to_string(&beacon).unwrap();
        assert!(!serialized.contains("\"board\""), "board omitted when None");
        assert!(!serialized.contains("\"tier\""), "tier omitted when None");
        assert!(!serialized.contains("\"mdnsHost\""), "mdnsHost omitted when None");
        assert!(!serialized.contains("\"localIp\""), "localIp omitted when None");
        // The five required fields are still present.
        assert!(serialized.contains("\"deviceId\":\"test-device\""));
        assert!(serialized.contains("\"name\":\"ADOS Lite Agent\""));
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
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
        };
        let (tx, _rx) = broadcast::channel(8);
        let diag = DiagState::shared();
        let err = spawn_cloud_client(bad, tx, InboundChannels::default(), diag)
            .expect_err("empty device_id should fail");
        match err {
            CloudError::Config(msg) => assert!(msg.contains("device_id")),
            _ => panic!("expected Config error, got {:?}", err),
        }
    }

    #[test]
    fn convex_url_with_bad_scheme_is_rejected() {
        // file:// (or any non-http(s) scheme) must be hard-rejected at
        // spawn time. An operator-supplied agent.yaml that points at
        // the local filesystem is a config bug we catch loudly rather
        // than letting reqwest fail noisily on every iteration.
        let bad = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: false,
            convex_url: "file:///tmp/foo".into(),
            pairing_path: PathBuf::from("/etc/ados/pairing.json"),
            agent_meta: None,
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
        };
        let (tx, _rx) = broadcast::channel(8);
        let diag = DiagState::shared();
        let err = spawn_cloud_client(bad, tx, InboundChannels::default(), diag)
            .expect_err("file:// scheme should be rejected");
        match err {
            CloudError::Config(msg) => assert!(msg.contains("convex_url")),
            _ => panic!("expected Config error, got {:?}", err),
        }
    }

    #[test]
    fn convex_url_http_is_accepted_with_warning() {
        // http:// (without TLS) must still spawn — the operator may be
        // running a local relay for dev work — but a WARN should be
        // logged. We don't assert the log line here (tracing capture
        // adds dependency weight); we only assert the spawn succeeds
        // and the unencrypted scheme is not treated as a hard error.
        let cfg = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: false,
            convex_url: "http://localhost:3210".into(),
            pairing_path: PathBuf::from("/etc/ados/pairing.json"),
            agent_meta: None,
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
        };
        let (tx, _rx) = broadcast::channel(8);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let diag = DiagState::shared();
        spawn_cloud_client(cfg, tx, InboundChannels::default(), diag)
            .expect("http:// scheme should be accepted (with warning)");
    }

    #[tokio::test]
    async fn heartbeat_401_unpairs_local_state() {
        // Reproduces the operator-facing failure the audit gate flagged:
        // the cloud relay returns 401 (api_key revoked / device removed
        // from Mission Control) and the agent had been silently
        // hammering /agent/status forever. The fix clears local
        // pairing state so the next loop iteration falls back to the
        // beacon flow.
        use ados_setup::pairing::{PairingState, PairingStore};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agent/status"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&mock)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let pairing_path = tmp.path().join("pairing.json");
        let store = PairingStore::new(&pairing_path);
        // Simulate a previously-paired device.
        // Fixture api_key must pass `is_valid_api_key()`'s shape check
        // so the heartbeat reaches the relay and we can observe the
        // 401-response path. Real generator output: `"ados_"` (5 chars)
        // + 43 url-safe-base64 no-pad chars = 48 total.
        let initial = PairingState {
            paired: true,
            api_key: Some(
                "ados_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            ),
            owner_id: Some("user_test".into()),
            ..Default::default()
        };
        store.save(&initial).unwrap();
        assert!(store.load().unwrap().is_paired(), "fixture is paired");

        let config = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: false,
            convex_url: mock.uri(),
            pairing_path: pairing_path.clone(),
            agent_meta: None,
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mut consecutive_failures = 0u32;
        let diag = DiagState::shared();
        // Drive a single tick. The 5s base sleep at the tail is the
        // post-success interval; we stop the test before it elapses
        // by using a short timeout — the heartbeat itself completes
        // synchronously against the mock and the unpair() call lands
        // before the sleep starts.
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            run_http_tick(
                &client,
                &config,
                &store,
                Instant::now(),
                &mut consecutive_failures,
                Duration::from_secs(300),
                &diag,
            ),
        )
        .await;

        // The 401 path clears local pairing state so the next loop
        // iteration falls back to the unpaired beacon.
        let after = store.load().unwrap();
        assert!(
            !after.is_paired(),
            "401 response should have unpaired the local state"
        );
        assert_eq!(after.api_key, None, "api_key cleared on 401");
        // The unauthorized branch is a recoverable handoff, not a
        // network failure: the failure counter stays at zero so the
        // next beacon goes out promptly.
        assert_eq!(
            consecutive_failures, 0,
            "401 should NOT increment consecutive_failures"
        );
        // The diag-side counter DOES record the 401 so an operator
        // hitting `/api/v1/diag` during the unpair handoff sees a
        // non-zero value. The two counters serve different audiences:
        // the local one drives the next-tick backoff; the diag one
        // surfaces "we tried and the relay rejected us" to operators.
        assert_eq!(
            diag.cloud_snapshot().consecutive_failures,
            1,
            "diag should record the 401 as a cloud-relay failure"
        );
        assert_eq!(
            diag.cloud_snapshot().last_heartbeat_at,
            None,
            "no heartbeat ever succeeded in this test"
        );
    }

    #[tokio::test]
    async fn heartbeat_500_increments_failure_counter() {
        // Counterpoint to the 401 test: a generic 5xx must bubble as a
        // network failure so the exponential backoff kicks in. Without
        // this assertion, a single switch from `error_for_status()` to
        // a more permissive shape could silently kill the backoff
        // path.
        use ados_setup::pairing::{PairingState, PairingStore};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agent/status"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let pairing_path = tmp.path().join("pairing.json");
        let store = PairingStore::new(&pairing_path);
        // Fixture api_key must pass `is_valid_api_key()`'s shape check
        // so the heartbeat reaches the (mocked) relay and the 5xx path
        // is exercised end-to-end (real shape: `"ados_"` + 43 base64url
        // no-pad chars = 48 total).
        let initial = PairingState {
            paired: true,
            api_key: Some(
                "ados_BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".into(),
            ),
            owner_id: Some("user_test".into()),
            ..Default::default()
        };
        store.save(&initial).unwrap();

        let config = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: false,
            convex_url: mock.uri(),
            pairing_path: pairing_path.clone(),
            agent_meta: None,
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mut consecutive_failures = 0u32;
        let diag = DiagState::shared();
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            run_http_tick(
                &client,
                &config,
                &store,
                Instant::now(),
                &mut consecutive_failures,
                Duration::from_secs(300),
                &diag,
            ),
        )
        .await;

        // Pairing state must NOT be cleared on a 5xx — the api_key
        // is still valid; the relay is just down.
        let after = store.load().unwrap();
        assert!(
            after.is_paired(),
            "500 response must not clear local pairing state"
        );
        assert_eq!(
            consecutive_failures, 1,
            "500 should increment consecutive_failures"
        );
    }

    #[tokio::test]
    async fn invalid_api_key_shape_skips_heartbeat_and_increments_failures() {
        // Defense-in-depth: a malformed api_key (control bytes, wrong
        // prefix, wrong length) must NOT reach the wire — reqwest's
        // `HeaderValue::from_str` would otherwise either reject it
        // with `InvalidHeaderValue` (looped forever) or pass corrupted
        // creds. The skip path must still increment the failure
        // counter so backoff kicks in immediately.
        use ados_setup::pairing::{PairingState, PairingStore};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        let received = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let received_for_mock = received.clone();
        Mock::given(method("POST"))
            .and(path("/agent/status"))
            .respond_with(move |_: &wiremock::Request| {
                received_for_mock.store(true, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200)
            })
            .mount(&mock)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let pairing_path = tmp.path().join("pairing.json");
        let store = PairingStore::new(&pairing_path);
        // Wrong prefix + short length — fails the shape gate.
        let initial = PairingState {
            paired: true,
            api_key: Some("not_an_api_key".into()),
            owner_id: Some("user_test".into()),
            ..Default::default()
        };
        store.save(&initial).unwrap();

        let config = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: false,
            convex_url: mock.uri(),
            pairing_path: pairing_path.clone(),
            agent_meta: None,
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mut consecutive_failures = 0u32;
        let diag = DiagState::shared();
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            run_http_tick(
                &client,
                &config,
                &store,
                Instant::now(),
                &mut consecutive_failures,
                Duration::from_secs(300),
                &diag,
            ),
        )
        .await;

        assert!(
            !received.load(std::sync::atomic::Ordering::SeqCst),
            "invalid-shape api_key must NOT result in a heartbeat hitting the relay"
        );
        assert!(
            store.load().unwrap().is_paired(),
            "invalid-shape skip must NOT clear local pairing state \
             (only a 401 from the relay does that)"
        );
        assert_eq!(
            consecutive_failures, 1,
            "invalid-shape skip should increment consecutive_failures so backoff kicks in"
        );
    }

    #[tokio::test]
    async fn heartbeat_success_stamps_diag_last_heartbeat_at() {
        // The Phase L1 fix wires `record_cloud_heartbeat` into the
        // 2xx branch of run_http_tick so `/api/v1/diag` reports a
        // live `cloud_relay.last_heartbeat_at` instead of the seeded
        // `null` placeholder. Without this assertion a future
        // refactor that drops the call would silently regress the
        // diag surface to its v0.1 behavior — placeholders forever.
        use ados_setup::pairing::{PairingState, PairingStore};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agent/status"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let pairing_path = tmp.path().join("pairing.json");
        let store = PairingStore::new(&pairing_path);
        // Real api_key shape so the heartbeat actually fires.
        let initial = PairingState {
            paired: true,
            api_key: Some(
                "ados_CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC".into(),
            ),
            owner_id: Some("user_test".into()),
            ..Default::default()
        };
        store.save(&initial).unwrap();

        let config = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: false,
            convex_url: mock.uri(),
            pairing_path: pairing_path.clone(),
            agent_meta: None,
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mut consecutive_failures = 0u32;
        let diag = DiagState::shared();
        // Seed a non-zero failure on the diag side so the success
        // branch's reset-to-zero is observable.
        diag.record_cloud_failure();
        diag.record_cloud_failure();
        assert_eq!(diag.cloud_snapshot().consecutive_failures, 2);

        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            run_http_tick(
                &client,
                &config,
                &store,
                Instant::now(),
                &mut consecutive_failures,
                Duration::from_secs(300),
                &diag,
            ),
        )
        .await;

        let snap = diag.cloud_snapshot();
        assert!(
            snap.last_heartbeat_at.is_some(),
            "successful heartbeat must stamp last_heartbeat_at"
        );
        assert_eq!(
            snap.consecutive_failures, 0,
            "successful heartbeat must reset the diag failure counter"
        );
        assert_eq!(consecutive_failures, 0);
    }

    #[tokio::test]
    async fn heartbeat_5xx_records_cloud_failure_in_diag() {
        // Counterpoint to the success test: a 5xx must increment the
        // diag-side counter alongside the local one. Pins the wiring
        // for the third K2.2-flagged call site.
        use ados_setup::pairing::{PairingState, PairingStore};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agent/status"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&mock)
            .await;

        let tmp = tempfile::tempdir().unwrap();
        let pairing_path = tmp.path().join("pairing.json");
        let store = PairingStore::new(&pairing_path);
        let initial = PairingState {
            paired: true,
            api_key: Some(
                "ados_DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD".into(),
            ),
            owner_id: Some("user_test".into()),
            ..Default::default()
        };
        store.save(&initial).unwrap();

        let config = CloudConfig {
            device_id: "test-device".into(),
            mqtt_broker: String::new(),
            mqtt_port: 1883,
            mqtt_use_tls: false,
            convex_url: mock.uri(),
            pairing_path: pairing_path.clone(),
            agent_meta: None,
            mqtt_keepalive_secs: 60,
            connect_timeout_secs: 3,
            request_timeout_secs: 10,
            allow_reboot: false,
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mut consecutive_failures = 0u32;
        let diag = DiagState::shared();

        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            run_http_tick(
                &client,
                &config,
                &store,
                Instant::now(),
                &mut consecutive_failures,
                Duration::from_secs(300),
                &diag,
            ),
        )
        .await;

        assert_eq!(
            diag.cloud_snapshot().consecutive_failures,
            1,
            "5xx should increment the diag failure counter"
        );
        assert_eq!(
            diag.cloud_snapshot().last_heartbeat_at,
            None,
            "no successful heartbeat in this test"
        );
    }
}
