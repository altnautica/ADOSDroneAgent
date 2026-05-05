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
use ados_setup::{setup_router, state::StateStore, SetupState};
use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
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

    /// Persist a pair code into agent.yaml and signal the running agent
    /// to reload. After this the cloud client switches from the unpaired
    /// pairing-beacon flow to the paired heartbeat flow.
    Pair {
        /// Pair code from Mission Control "Add drone".
        code: String,
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
        Command::Pair { code } => persist_pair_code(&cli.config, &code).await,
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

    // Spawn the cloud client when we have an identity. Missing MQTT broker
    // is handled inside the cloud client (it skips the MQTT publish loop
    // and runs only the HTTP loop, which serves the unpaired beacon and
    // the paired heartbeat). Missing convex_url is also tolerated — the
    // HTTP loop logs and waits for the operator to configure it via the
    // setup webapp or by editing agent.yaml directly.
    if !cloud_config.device_id.is_empty() {
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
        tracing::info!("device_id missing; running offline (no mqtt, no heartbeat)");
    }

    // axum HTTP server: full DEC-141 setup surface mounted from ados-setup.
    // Status snapshot reads live agent state (paired/unpaired, mavlink
    // port + baud, device_id). The crate-level state owns the agent.yaml
    // path + setup-state.yaml store; this binary supplies the snapshot
    // builder closure.
    let app_state_inner = Arc::new(AppState {
        device_id: config.agent.device_id.clone(),
        paired: !config.cloud.api_key.is_empty(),
        mavlink_port: config.mavlink.port.clone(),
        mavlink_baud: config.mavlink.baud,
        config_path: config_path.clone(),
    });

    // Allow override via ADOS_SETUP_STATE_PATH so tests + dev containers
    // don't need /etc write access. Production install puts this at
    // /etc/ados/setup-state.yaml owned by the agent user.
    let setup_state_path = std::env::var_os("ADOS_SETUP_STATE_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/etc/ados/setup-state.yaml"));
    let setup_state_store = StateStore::new(setup_state_path);
    let snapshot_state = app_state_inner.clone();
    let snapshot_store = setup_state_store.clone();
    let setup_state = Arc::new(SetupState {
        agent_yaml: config_path.clone(),
        store: setup_state_store,
        status_builder: Box::new(move || {
            build_setup_status(&snapshot_state, &snapshot_store)
        }),
    });

    let app = setup_router(setup_state)
        // Cap request bodies at 64 KiB. The setup surface accepts no
        // large payloads today; this is defense-in-depth for the POST
        // handlers.
        .layer(DefaultBodyLimit::max(64 * 1024));

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

/// Persist a pair code into agent.yaml at `cloud.api_key`. Writes
/// atomically (tempfile + rename) and signals the running service to
/// reload by restarting the systemd / busybox / runit unit.
async fn persist_pair_code(config_path: &std::path::Path, code: &str) -> Result<()> {
    if code.is_empty() {
        anyhow::bail!("pair code is empty");
    }

    // Load + mutate as a generic YAML document so we never reformat fields
    // the operator may have edited. serde_yaml::Value preserves the rest.
    let raw = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .with_context(|| format!("reading {}", config_path.display()))?
    } else {
        String::from("{}")
    };
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(&raw).with_context(|| "parsing yaml")?;
    if !doc.is_mapping() {
        doc = serde_yaml::Value::Mapping(Default::default());
    }
    let map = doc.as_mapping_mut().expect("doc is mapping");
    let cloud_key = serde_yaml::Value::String("cloud".into());
    let cloud = map
        .entry(cloud_key)
        .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));
    if !cloud.is_mapping() {
        *cloud = serde_yaml::Value::Mapping(Default::default());
    }
    let cloud_map = cloud.as_mapping_mut().expect("cloud is mapping");
    cloud_map.insert(
        serde_yaml::Value::String("api_key".into()),
        serde_yaml::Value::String(code.into()),
    );

    // Write atomically: write to a tempfile in the same directory, then
    // rename. This keeps the on-disk file either the old or the new copy
    // even if the agent is killed mid-write.
    let parent = config_path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent).ok();
    let tmp = parent.join(format!(
        ".agent.yaml.{}.tmp",
        std::process::id()
    ));
    let serialized = serde_yaml::to_string(&doc).with_context(|| "serializing yaml")?;
    std::fs::write(&tmp, serialized).with_context(|| format!("writing {}", tmp.display()))?;
    // 0640 — readable by root + ados group; never world-readable because
    // the file now contains the api_key.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640)).ok();
    }
    std::fs::rename(&tmp, config_path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), config_path.display()))?;

    println!("paired and config updated at {}", config_path.display());

    // Best-effort: signal the running service to pick up the new config.
    // We try systemd first, then busybox sysv-rc, then runit. If none
    // succeed, the operator restarts manually — we still wrote the file.
    let restart_attempts: &[(&str, &[&str])] = &[
        ("systemctl", &["restart", "ados-agent-lite.service"]),
        ("/etc/init.d/S99ados-agent-lite", &["restart"]),
        ("sv", &["restart", "ados-agent-lite"]),
    ];
    for (program, args) in restart_attempts {
        match std::process::Command::new(program).args(*args).status() {
            Ok(s) if s.success() => {
                println!("restarted service via {}", program);
                return Ok(());
            }
            _ => continue,
        }
    }
    println!(
        "config saved. Restart the service to pick up the new pair code: \
         sudo systemctl restart ados-agent-lite (systemd) or \
         sudo /etc/init.d/S99ados-agent-lite restart (busybox)"
    );
    Ok(())
}

#[derive(Clone)]
struct AppState {
    device_id: String,
    paired: bool,
    mavlink_port: String,
    mavlink_baud: u32,
    #[allow(dead_code)] // consumed by setup-rest handlers landing in B7.5
    config_path: PathBuf,
}

fn build_setup_status(state: &Arc<AppState>, store: &StateStore) -> Value {
    // Returns the canonical SetupStatus shape so consumers (the setup
    // webapp, Mission Control, the cloud relay) read every expected
    // field. Empty / null sub-blocks reflect the current minimum-viable
    // surface; richer values populate as the agent grows access to FC
    // state, video pipeline, and remote-access providers.
    let persisted = store.load().unwrap_or_default();
    let next_action = if persisted.finalized {
        "ready"
    } else if state.paired {
        "ready"
    } else {
        "pair"
    };
    let skipped: Vec<String> = persisted.skipped_steps.into_iter().collect();
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "device_id": state.device_id,
        "device_name": "ADOS Lite Agent",
        "profile": "drone",
        "ground_role": "",
        "runtime_mode": "lite",
        "setup_complete": persisted.finalized || state.paired,
        "setup_finalized": persisted.finalized,
        "completion_percent": if persisted.finalized { 100 } else if state.paired { 80 } else { 0 },
        "next_action": next_action,
        "steps": [],
        "skipped_steps": skipped,
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
            "port": state.mavlink_port,
            "baud": state.mavlink_baud,
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
            "paired": state.paired,
            "pair_code_required": !state.paired,
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
    })
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

    #[tokio::test]
    async fn persist_pair_code_writes_api_key_into_existing_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(
            &path,
            r#"agent:
  device_id: "test"
  name: "Test"
cloud:
  mqtt_broker: ""
  api_key: ""
"#,
        )
        .unwrap();

        persist_pair_code(&path, "ABCD1234").await.unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
        let api_key = doc
            .get("cloud")
            .and_then(|c| c.get("api_key"))
            .and_then(|k| k.as_str())
            .unwrap();
        assert_eq!(api_key, "ABCD1234");
        // Other agent fields are preserved.
        assert_eq!(
            doc.get("agent")
                .and_then(|a| a.get("device_id"))
                .and_then(|v| v.as_str()),
            Some("test")
        );
    }

    #[tokio::test]
    async fn persist_pair_code_creates_cloud_section_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(
            &path,
            r#"agent:
  device_id: "test"
"#,
        )
        .unwrap();

        persist_pair_code(&path, "WXYZ5678").await.unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&raw).unwrap();
        assert_eq!(
            doc.get("cloud")
                .and_then(|c| c.get("api_key"))
                .and_then(|k| k.as_str()),
            Some("WXYZ5678")
        );
    }

    #[tokio::test]
    async fn persist_pair_code_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.yaml");
        std::fs::write(&path, "{}").unwrap();
        let err = persist_pair_code(&path, "").await.unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
