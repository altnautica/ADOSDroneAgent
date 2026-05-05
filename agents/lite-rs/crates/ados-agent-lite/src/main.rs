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
use axum::{response::Json, routing::get, Router};
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

    /// Print agent status and exit.
    Status,

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
    "0.0.0.0:8080".into()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(cli.config).await,
        Command::Status => {
            print_status();
            Ok(())
        }
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

fn print_status() {
    println!("ados-agent-lite {}", env!("CARGO_PKG_VERSION"));
    println!(
        "status: read from /api/v1/setup/status when the agent is running"
    );
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
        .with_state(app_state);

    let bind_addr: SocketAddr = config
        .api
        .bind
        .parse()
        .with_context(|| format!("invalid api.bind address: {}", config.api.bind))?;
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
    Json(serde_json::json!({
        "device_id": state.device_id,
        "version": env!("CARGO_PKG_VERSION"),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "runtime_mode": "lite",
        "profile": "drone",
        "setup_finalized": false,
        "services": [
            { "name": "mavlink-router", "status": "running" },
            { "name": "cloud-client", "status": "running" },
            { "name": "http-api", "status": "running" }
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
        assert_eq!(parsed.api.bind, "0.0.0.0:8080");
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
