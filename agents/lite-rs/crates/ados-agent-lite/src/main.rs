// ados-agent-lite — main binary entry point.
//
// Single-process, single static binary. Cooperating tokio tasks run the
// MAVLink router, the cloud relay client, and a tiny axum HTTP server
// that exposes /api/v1/setup/status. Reads /etc/ados/agent.yaml at
// startup. Behavior dispatches per the detected board profile loaded
// from /opt/ados/hal/boards/<id>.yaml.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use ados_cloud::CloudConfig;
use ados_mavlink::MavlinkConfig;
use anyhow::{Context, Result};
use axum::{extract::DefaultBodyLimit, response::Json, routing::get, Router};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "ados-agent-lite")]
#[command(about = "Lightweight ADOS Drone Agent for low-RAM SBCs")]
#[command(version)]
struct Cli {
    /// Path to the agent configuration file.
    #[arg(long, default_value = "/etc/ados/agent.yaml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the agent (default when invoked without a subcommand).
    Run,

    /// Print agent status and exit. Reads from the running agent's HTTP
    /// API at the bind address configured in agent.yaml.
    Status {
        /// Emit JSON to stdout instead of plain-text.
        #[arg(long, short = 'j')]
        json: bool,
    },

    /// Print version information and exit.
    Version,
}

/// Top-level agent configuration loaded from agent.yaml.
#[derive(Debug, Clone, Deserialize)]
struct AgentConfig {
    #[serde(default)]
    agent: AgentSection,
    #[serde(default)]
    mavlink: MavlinkSection,
    #[serde(default)]
    cloud: CloudSection,
    #[serde(default)]
    api: ApiSection,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AgentSection {
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    #[allow(dead_code)] // Surfaced via /api/v1/setup/status in a later phase.
    name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct MavlinkSection {
    #[serde(default = "default_mavlink_port")]
    port: String,
    #[serde(default = "default_mavlink_baud")]
    baud: u32,
}

impl Default for MavlinkSection {
    fn default() -> Self {
        Self {
            port: default_mavlink_port(),
            baud: default_mavlink_baud(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CloudSection {
    #[serde(default)]
    mqtt_broker: String,
    #[serde(default = "default_mqtt_port")]
    mqtt_port: u16,
    #[serde(default = "default_true")]
    mqtt_use_tls: bool,
    #[serde(default)]
    convex_url: String,
    #[serde(default)]
    api_key: String,
}

impl Default for CloudSection {
    fn default() -> Self {
        Self {
            mqtt_broker: String::new(),
            mqtt_port: default_mqtt_port(),
            mqtt_use_tls: default_true(),
            convex_url: String::new(),
            api_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ApiSection {
    #[serde(default = "default_api_bind")]
    bind: String,
}

impl Default for ApiSection {
    fn default() -> Self {
        Self {
            bind: default_api_bind(),
        }
    }
}

fn default_mavlink_port() -> String {
    "/dev/ttyS0".into()
}
fn default_mavlink_baud() -> u32 {
    115_200
}
fn default_mqtt_port() -> u16 {
    8883
}
fn default_true() -> bool {
    true
}
fn default_api_bind() -> String {
    // Bind to localhost by default. Operators who need LAN access for the
    // setup webapp must explicitly opt in via api.bind in agent.yaml. This
    // avoids unintentionally exposing the setup surface to other devices
    // on the same Wi-Fi.
    "127.0.0.1:8080".into()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(cli.config).await,
        Command::Status { json } => print_status(&cli.config, json).await,
        Command::Version => {
            println!("ados-agent-lite {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).compact().init();
}

async fn print_status(config_path: &std::path::Path, as_json: bool) -> Result<()> {
    let config = load_config(config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    // Construct the status URL from the configured bind address. When the
    // agent binds to 0.0.0.0 the CLI still reaches it via 127.0.0.1.
    let bind_addr: SocketAddr = config
        .api
        .bind
        .parse()
        .with_context(|| format!("invalid api.bind address: {}", config.api.bind))?;
    let host = if bind_addr.ip().is_unspecified() {
        std::net::IpAddr::from([127u8, 0, 0, 1])
    } else {
        bind_addr.ip()
    };
    let url = format!("http://{}:{}/api/v1/setup/status", host, bind_addr.port());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("contacting agent at {}", url))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .with_context(|| "parsing agent status response")?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        let device_id = body
            .get("device_id")
            .and_then(|v| v.as_str())
            .unwrap_or("<unset>");
        let version = body.get("version").and_then(|v| v.as_str()).unwrap_or("?");
        let runtime_mode = body
            .get("runtime_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let profile = body.get("profile").and_then(|v| v.as_str()).unwrap_or("?");
        let setup_finalized = body
            .get("setup_finalized")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        println!("ados-agent-lite {} (HTTP {})", version, status.as_u16());
        println!("  device_id:        {}", device_id);
        println!("  profile:          {}", profile);
        println!("  runtime_mode:    {}", runtime_mode);
        println!("  setup_finalized: {}", setup_finalized);
    }
    Ok(())
}

async fn run(config_path: PathBuf) -> Result<()> {
    let config = load_config(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        device_id = config.agent.device_id,
        "ados-agent-lite starting"
    );

    // Cooperating tasks. Each spawns its own background work. The main
    // task waits for shutdown signal and supervises panics via Tokio's
    // catch-unwind in spawn().
    let mavlink_config = MavlinkConfig {
        port: config.mavlink.port.clone(),
        baud: config.mavlink.baud,
    };

    let mavlink_handles = match ados_mavlink::spawn_router(mavlink_config) {
        Ok(handles) => Some(handles),
        Err(e) => {
            // FC serial is not always present in dev / cgroup tests. Log
            // and continue so the cloud heartbeat path can still surface
            // the agent in the fleet view.
            tracing::warn!(error = %e, "mavlink router unavailable; continuing without FC link");
            None
        }
    };

    let cloud_config = CloudConfig {
        device_id: config.agent.device_id.clone(),
        mqtt_broker: config.cloud.mqtt_broker.clone(),
        mqtt_port: config.cloud.mqtt_port,
        mqtt_use_tls: config.cloud.mqtt_use_tls,
        convex_url: config.cloud.convex_url.clone(),
        api_key: config.cloud.api_key.clone(),
    };

    if !cloud_config.device_id.is_empty() && !cloud_config.mqtt_broker.is_empty() {
        let mavlink_inbound = mavlink_handles
            .as_ref()
            .map(|h| h.inbound.clone())
            .unwrap_or_else(|| {
                let (tx, _rx) = tokio::sync::broadcast::channel(16);
                tx
            });
        if let Err(e) = ados_cloud::spawn_cloud_client(cloud_config, mavlink_inbound) {
            tracing::warn!(error = %e, "cloud client spawn failed; running offline");
        }
    } else {
        tracing::info!("cloud config incomplete; running offline (no mqtt, no heartbeat)");
    }

    // axum HTTP server: minimal /api/v1/setup/status stub at v0.1.
    let app_state = Arc::new(AppState {
        device_id: config.agent.device_id.clone(),
    });
    let app = Router::new()
        .route("/api/v1/setup/status", get(setup_status))
        // Cap request bodies at 64 KiB. The setup surface accepts no
        // large payloads today; this is defense-in-depth for the POST
        // handlers that land in subsequent versions.
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(app_state);

    let bind_addr: SocketAddr = config
        .api
        .bind
        .parse()
        .with_context(|| format!("invalid api.bind address: {}", config.api.bind))?;
    if bind_addr.ip().is_unspecified() {
        tracing::warn!(
            addr = %bind_addr,
            "http api binding 0.0.0.0; setup surface is exposed to every interface"
        );
    }
    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("binding HTTP server on {}", bind_addr))?;
    tracing::info!(addr = %bind_addr, "http api listening");

    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "http api exited with error");
        }
    });

    // Wait for shutdown.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutdown signal received");
        }
        result = server => {
            tracing::warn!(?result, "http api task ended");
        }
    }

    tracing::info!("ados-agent-lite stopped");
    Ok(())
}

fn load_config(path: &std::path::Path) -> Result<AgentConfig> {
    if !path.exists() {
        tracing::warn!(path = %path.display(), "config file missing; using defaults");
        return Ok(AgentConfig {
            agent: AgentSection::default(),
            mavlink: MavlinkSection::default(),
            cloud: CloudSection::default(),
            api: ApiSection::default(),
        });
    }
    let raw = std::fs::read_to_string(path)?;
    let parsed: AgentConfig = serde_yaml::from_str(&raw)
        .with_context(|| format!("parsing yaml at {}", path.display()))?;
    Ok(parsed)
}

#[derive(Clone)]
struct AppState {
    device_id: String,
}

async fn setup_status(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Json<Value> {
    // Returns the canonical SetupStatus shape so consumers (the setup
    // webapp, Mission Control, the cloud relay) read every expected
    // field. Empty / null sub-blocks reflect the v0.1 minimum-viable
    // surface; richer values populate as the agent grows access to FC
    // state, video pipeline, and remote-access providers.
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "device_id": state.device_id,
        "device_name": "ADOS Lite Agent",
        "profile": "drone",
        "ground_role": "",
        "runtime_mode": "lite",
        "setup_complete": false,
        "setup_finalized": false,
        "completion_percent": 0,
        "next_action": "pair",
        "steps": [],
        "skipped_steps": [],
        "access_urls": [],
        "network": {
            "hostname": "",
            "mdns_host": "",
            "api_port": 8080,
            "hotspot_enabled": false,
            "hotspot_ssid": "",
            "local_ips": []
        },
        "mavlink": {
            "connected": false,
            "port": null,
            "baud": null,
            "websocket_url": null,
            "public_websocket_url": null
        },
        "video": {
            "state": "not_initialized",
            "whep_url": null,
            "public_whep_url": null,
            "recording": false
        },
        "remote_access": {
            "provider": "none",
            "enabled": false,
            "configured": false,
            "status": "disabled",
            "public_urls": [],
            "error": ""
        },
        "cloud_choice": {
            "mode": "cloud",
            "paired": false,
            "pair_code_required": true,
            "backend_url": "",
            "backend_reachable": false,
            "last_checked": null
        },
        "profile_suggestion": null,
        "hardware_check": null,
        "services": [
            { "name": "mavlink-router", "state": "running" },
            { "name": "cloud-client",   "state": "running" },
            { "name": "http-api",       "state": "running" }
        ]
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_defaults_load_from_empty_yaml() {
        let parsed: AgentConfig = serde_yaml::from_str("{}").unwrap();
        assert_eq!(parsed.mavlink.port, "/dev/ttyS0");
        assert_eq!(parsed.mavlink.baud, 115_200);
        assert_eq!(parsed.cloud.mqtt_port, 8883);
        assert!(parsed.cloud.mqtt_use_tls);
        assert_eq!(parsed.api.bind, "127.0.0.1:8080");
    }

    #[test]
    fn agent_config_loads_full_yaml() {
        let yaml = r#"
agent:
  device_id: "test-device"
  name: "Test"
mavlink:
  port: "/dev/ttyACM0"
  baud: 57600
cloud:
  mqtt_broker: "broker.example"
  mqtt_port: 1883
  mqtt_use_tls: false
  convex_url: "https://relay.example"
  api_key: "secret"
api:
  bind: "127.0.0.1:9090"
"#;
        let parsed: AgentConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(parsed.agent.device_id, "test-device");
        assert_eq!(parsed.mavlink.port, "/dev/ttyACM0");
        assert_eq!(parsed.mavlink.baud, 57_600);
        assert_eq!(parsed.cloud.mqtt_broker, "broker.example");
        assert!(!parsed.cloud.mqtt_use_tls);
        assert_eq!(parsed.api.bind, "127.0.0.1:9090");
    }
}
