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

    /// Persist a pair code into pairing.json and signal the running
    /// agent to reload. After this the cloud client switches from the
    /// unpaired pairing-beacon flow to the paired heartbeat flow.
    Pair {
        /// Pair code from Mission Control "Add drone".
        code: String,
    },

    /// Re-run the install script in upgrade mode. Pulls the latest
    /// signed binary from GitHub Releases, verifies SHA256, and replaces
    /// the on-disk binary in place. Setup state + pairing state are
    /// preserved. Mirrors `ados update` from the DEC-141 four-command
    /// CLI contract.
    Update,

    /// Stop the agent service, remove the binary + init unit, and
    /// preserve config + pairing state for a possible re-install.
    /// Mirrors `ados uninstall` from the DEC-141 contract.
    Uninstall,

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
    /// Pre-2026-05-05 versions of agent.yaml stored the per-device API
    /// key here. The canonical location is now /etc/ados/pairing.json
    /// (matching the Python full agent's PairingManager). The field is
    /// retained as deserializable for back-compat — if an old agent.yaml
    /// has it set and pairing.json is empty, we migrate it on first
    /// boot. Going forward, all pair operations write to pairing.json.
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
        Command::Update => run_install_script(&["--upgrade"]).await,
        Command::Uninstall => run_install_script(&["--uninstall"]).await,
        Command::Version => {
            println!("ados-agent-lite {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

/// Re-run install-lite.sh from the canonical raw URL with the supplied
/// flags. Used by `update` and `uninstall` so the agent stays a single
/// signed static binary — operator state lives in /etc/ados/, not in
/// the agent process. Falls back to a local copy at /usr/local/bin/
/// install-lite.sh when present (developer override).
async fn run_install_script(args: &[&str]) -> Result<()> {
    use std::process::Command as PCommand;
    const URL: &str =
        "https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/scripts/install-lite.sh";
    // Prefer a sibling install-lite.sh if the operator put one there
    // for testing. Otherwise curl-pipe the canonical URL.
    let local_paths = [
        "/usr/local/share/ados/install-lite.sh",
        "/usr/local/bin/install-lite.sh",
    ];
    let mut command = if let Some(path) = local_paths.iter().find(|p| std::path::Path::new(p).exists()) {
        let mut c = PCommand::new("sh");
        c.arg(path).args(args);
        c
    } else {
        // Curl-pipe: `curl ... | sh -s -- <args>` with explicit args
        // separator. Falls back to wget on Buildroot rootfs without
        // curl (mirrors the install-script's own fetch helper).
        let fetch = if std::path::Path::new("/usr/bin/curl").exists() {
            format!("curl -fsSL {URL}")
        } else {
            format!("wget -q -O - {URL}")
        };
        let mut c = PCommand::new("sh");
        c.arg("-c").arg(format!(
            "{} | sh -s -- {}",
            fetch,
            args.iter()
                .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        c
    };
    let status = command
        .status()
        .with_context(|| format!("running install-lite.sh {}", args.join(" ")))?;
    if !status.success() {
        anyhow::bail!("install-lite.sh exited with code {:?}", status.code());
    }
    Ok(())
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

    // Resolve pairing.json path. Defaults to /etc/ados/pairing.json (next
    // to agent.yaml). Tests + dev containers override via the env var.
    let pairing_path = std::env::var_os("ADOS_PAIRING_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            config_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("/etc/ados"))
                .join("pairing.json")
        });

    // Migrate legacy agent.yaml `cloud.api_key` to pairing.json on first
    // boot if the new file is empty. This preserves operator pairings
    // from pre-2026-05-05 agent.yaml configs without forcing a re-pair.
    if !config.cloud.api_key.is_empty() {
        let store = ados_setup::pairing::PairingStore::new(&pairing_path);
        if let Ok(existing) = store.load() {
            if !existing.is_paired() {
                tracing::info!(
                    "migrating legacy cloud.api_key from agent.yaml -> pairing.json"
                );
                // The old field stored what was effectively a pair code;
                // we treat it as one and let the cloud relay re-claim.
                let _ = store.set_code(&config.cloud.api_key);
            }
        }
    }

    let cloud_config = CloudConfig {
        device_id: config.agent.device_id.clone(),
        mqtt_broker: config.cloud.mqtt_broker.clone(),
        mqtt_port: config.cloud.mqtt_port,
        mqtt_use_tls: config.cloud.mqtt_use_tls,
        convex_url: config.cloud.convex_url.clone(),
        pairing_path: pairing_path.clone(),
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
        // FC writer for cloud-received MAVLink frames. Pass through only
        // when a real router is up; otherwise the cloud client logs +
        // drops inbound mavlink/rx publishes.
        let fc_writer = mavlink_handles.as_ref().map(|h| h.outbound.clone());
        if let Err(e) =
            ados_cloud::spawn_cloud_client(cloud_config, mavlink_inbound, fc_writer)
        {
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
        mavlink_port: config.mavlink.port.clone(),
        mavlink_baud: config.mavlink.baud,
        pairing_path: pairing_path.clone(),
    });

    // Allow override via ADOS_SETUP_STATE_PATH so tests + dev containers
    // don't need /var write access. Production install puts this at
    // /var/lib/ados/setup/state.json — same path the Python full agent
    // uses, so an operator can swap between agents without losing setup
    // state.
    let setup_state_path = std::env::var_os("ADOS_SETUP_STATE_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/var/lib/ados/setup/state.json"));
    let setup_state_store = StateStore::new(setup_state_path);
    let snapshot_state = app_state_inner.clone();
    let snapshot_store = setup_state_store.clone();
    let snapshot_yaml = config_path.clone();
    let setup_state = Arc::new(SetupState {
        agent_yaml: config_path.clone(),
        store: setup_state_store,
        status_builder: Box::new(move || {
            build_setup_status(&snapshot_state, &snapshot_store, &snapshot_yaml)
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

/// Persist a pair code via the canonical `pairing.json` path (mirrors
/// the Python full agent's PairingManager). The pair code goes to
/// `pairing.json:pairing_code`, NOT to `agent.yaml:cloud.api_key` —
/// those are different values per the proto:
///
/// - `pairing_code` is a short 6-char operator-readable code that gets
///   typed into Mission Control's "Add drone" dialog.
/// - `api_key` is the long `ados_<base64url-32>` per-device bearer
///   the cloud relay returns AFTER a successful claim.
///
/// Conflating them (the prior implementation) broke the cloud relay's
/// X-ADOS-Key header check on heartbeats — the agent was sending its
/// short pair code as the API key.
///
/// On success this also signals the running service to reload by
/// restarting via systemd / busybox sysv-rc / runit.
async fn persist_pair_code(config_path: &std::path::Path, code: &str) -> Result<()> {
    if code.is_empty() {
        anyhow::bail!("pair code is empty");
    }

    // The pairing.json path lives next to /etc/ados/agent.yaml. We derive
    // it from the config path's parent so test runs (and dev containers)
    // that override the config path automatically pick up a sibling
    // pairing.json without further env wiring.
    let pairing_path = config_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/etc/ados"))
        .join("pairing.json");

    let store = ados_setup::pairing::PairingStore::new(&pairing_path);
    store
        .set_code(code)
        .with_context(|| format!("writing {}", pairing_path.display()))?;

    println!("pair code saved to {}", pairing_path.display());

    // Best-effort: signal the running service to pick up the new code.
    // Absolute paths so a subverted $PATH does not redirect to a hostile
    // binary. We try systemd first, then busybox sysv-rc, then runit.
    let restart_attempts: &[(&str, &[&str])] = &[
        ("/usr/bin/systemctl", &["restart", "ados-agent-lite.service"]),
        ("/bin/systemctl", &["restart", "ados-agent-lite.service"]),
        ("/etc/init.d/S99ados-agent-lite", &["restart"]),
        ("/usr/bin/sv", &["restart", "ados-agent-lite"]),
    ];
    for (program, args) in restart_attempts {
        if !std::path::Path::new(program).exists() {
            continue;
        }
        match std::process::Command::new(program).args(*args).status() {
            Ok(s) if s.success() => {
                println!("restarted service via {}", program);
                return Ok(());
            }
            _ => continue,
        }
    }
    println!(
        "code saved. Restart the service to pick up the new pair code: \
         sudo systemctl restart ados-agent-lite (systemd) or \
         sudo /etc/init.d/S99ados-agent-lite restart (busybox)"
    );
    Ok(())
}

#[derive(Clone)]
struct AppState {
    device_id: String,
    mavlink_port: String,
    mavlink_baud: u32,
    pairing_path: PathBuf,
}

fn build_setup_status(
    state: &Arc<AppState>,
    store: &StateStore,
    agent_yaml: &std::path::Path,
) -> Value {
    // Returns the canonical SetupStatus shape so consumers (the setup
    // webapp, Mission Control, the cloud relay) read every expected
    // field. Re-reads pairing.json each call so a `ados-agent-lite pair`
    // from another process flips paired-state on the next /status query
    // without needing an agent restart.
    let persisted = store.load().unwrap_or_default();
    let pairing_state = ados_setup::pairing::PairingStore::new(&state.pairing_path)
        .load()
        .unwrap_or_default();
    let paired = pairing_state.is_paired();
    let next_action = if persisted.finalized {
        "ready"
    } else if paired {
        "ready"
    } else {
        "pair"
    };
    let skipped: Vec<String> = persisted.skipped_steps.iter().cloned().collect();
    // Read everything we can from the live agent.yaml so the wizard
    // reflects operator config + the cloud relay surface drives the
    // GCS Lite-card plumbing correctly.
    let yaml_view = read_yaml_view(agent_yaml);
    let (profile, ground_role) = (yaml_view.profile.clone(), yaml_view.ground_role.clone());
    // Hostname + local IPs for the network block (best-effort; falls
    // back to empties on systems without these probes).
    let hostname = sys_hostname();
    let local_ips = sys_local_ips();
    // api.bind parses to "host:port" — extract the port for the
    // network.api_port surface (falls back to 8080 if parse fails).
    let api_port = yaml_view
        .api_bind
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(8080);

    // Compute the 10-step lifecycle. Mirrors src/ados/setup/service.py
    // so the wizard sidebar surfaces identical step states regardless of
    // which agent half (Python full or Rust lite) is serving.
    let steps = build_steps(
        &profile,
        paired,
        persisted.finalized,
        &persisted.skipped_steps,
    );
    let total_steps = steps.len();
    let complete_count = steps
        .iter()
        .filter(|s| s.get("state").and_then(|v| v.as_str()) == Some("complete"))
        .count();
    let completion_percent = if total_steps == 0 {
        0
    } else {
        ((complete_count as f64 / total_steps as f64) * 100.0).round() as i64
    };
    let next_action = steps
        .iter()
        .find(|s| s.get("state").and_then(|v| v.as_str()) == Some("needs_action"))
        .and_then(|s| s.get("action_label").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(|| if persisted.finalized { "ready".into() } else { next_action.into() });

    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "device_id": state.device_id,
        "device_name": yaml_view.agent_name,
        "profile": profile,
        "ground_role": ground_role,
        "runtime_mode": "lite",
        "setup_complete": persisted.finalized || paired,
        "setup_finalized": persisted.finalized,
        "completion_percent": if persisted.finalized { 100 } else { completion_percent },
        "next_action": next_action,
        "steps": steps,
        "skipped_steps": skipped,
        "access_urls": [],
        "network": {
            "hostname": hostname,
            "mdns_host": format!("ados-{}.local", state.device_id),
            "api_port": api_port,
            "hotspot_enabled": false,
            "hotspot_ssid": "",
            "local_ips": local_ips
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
            "mode": yaml_view.cloud_mode,
            "paired": paired,
            "pair_code_required": !paired,
            "backend_url": yaml_view.cloud_url,
            "backend_reachable": false,
            "last_checked": null
        },
        "profile_suggestion": {
            "detected": "unconfigured",
            "ground_role_hint": "direct",
            "ground_score": 0,
            "air_score": 0,
            "mesh_capable": false,
            "signals": {},
            "confirmed": false,
            "detected_at": null
        },
        "hardware_check": null,
        "services": [
            { "name": "mavlink-router", "state": "running" },
            { "name": "cloud-client",   "state": "running" },
            { "name": "http-api",       "state": "running" }
        ],
        "telemetry": {}
    })
}

/// Build the 10-step wizard lifecycle list with per-step state.
/// Mirrors `src/ados/setup/service.py:build_setup_status` step-derivation
/// so consumers (the universal setup webapp + Mission Control) render
/// the same progress sidebar regardless of which agent serves.
///
/// At Phase 1 lite v1 we have visibility into:
/// - profile + ground_role (from agent.yaml)
/// - paired (from pairing.json)
/// - finalized + skipped_steps (from setup-state.json)
///
/// We do NOT yet have live mavlink heartbeat / video / remote-access
/// state; those steps mark themselves `needs_action` (or `optional` if
/// the operator skipped them). Once Phase E3 wires runtime state, the
/// derivations below grow.
fn build_steps(
    profile: &str,
    paired: bool,
    finalized: bool,
    skipped: &std::collections::BTreeSet<String>,
) -> Vec<serde_json::Value> {
    let is_drone = profile == "drone";
    let is_ground = profile == "ground_station";
    let mut out: Vec<serde_json::Value> = Vec::new();

    let push = |out: &mut Vec<_>, id: &str, label: &str, state: &str, detail: &str, action_label: &str| {
        let mut effective_state = state.to_string();
        if skipped.contains(id) && state == "needs_action" {
            effective_state = "optional".into();
        }
        out.push(serde_json::json!({
            "id": id,
            "label": label,
            "state": effective_state,
            "detail": detail,
            "action_label": action_label,
            "href": "",
        }));
    };

    // welcome — always complete (the operator made it past the welcome screen).
    push(&mut out, "welcome", "Welcome", "complete", "Onboarding starting.", "");

    // profile — complete when one of {drone, ground_station} is set.
    if is_drone || is_ground {
        push(
            &mut out,
            "profile",
            "Profile",
            "complete",
            &format!("{} profile selected.", if is_drone { "Drone" } else { "Ground station" }),
            "",
        );
    } else {
        push(
            &mut out,
            "profile",
            "Profile",
            "needs_action",
            "Pick the role this device serves.",
            "Choose profile",
        );
    }

    // hardware_check — at lite v1 we treat it as needs_action by default;
    // the operator runs the explicit /hardware-check route to populate it.
    // Real per-component derivation lands in Phase E3 alongside the
    // runtime hardware-check engine.
    push(
        &mut out,
        "hardware_check",
        "Hardware",
        "needs_action",
        "Verify the FC, camera, and Wi-Fi adapters.",
        "Run hardware check",
    );

    // cloud_choice — surfaced after profile. Complete when paired (the
    // pair flow implies the cloud_choice step landed first).
    if paired {
        push(&mut out, "cloud_choice", "Cloud", "complete", "Cloud relay configured.", "");
    } else {
        push(
            &mut out,
            "cloud_choice",
            "Cloud",
            "needs_action",
            "Pick how this device reaches Mission Control.",
            "Choose cloud mode",
        );
    }

    // pair — operator-typed code claim.
    if paired {
        push(&mut out, "pair", "Pair", "complete", "Device claimed.", "");
    } else {
        push(
            &mut out,
            "pair",
            "Pair",
            "needs_action",
            "Enter the pair code from Mission Control.",
            "Pair device",
        );
    }

    // mavlink — drone profile only. Live heartbeat probe lands in Phase E3.
    if is_drone {
        push(
            &mut out,
            "mavlink",
            "Flight controller",
            "needs_action",
            "Connect a flight controller over USB or UART.",
            "Connect FC",
        );
    }

    // video — always emitted; lite v1 has no video pipeline so always
    // needs_action until MSN-055 ships RKMPI / V4L2.
    push(
        &mut out,
        "video",
        "Video",
        "needs_action",
        "Video pipeline lands in the next phase.",
        "Configure video",
    );

    // ground_receiver — ground_station profile only.
    if is_ground {
        push(
            &mut out,
            "ground_receiver",
            "Ground receiver",
            "needs_action",
            "Configure WFB-ng receiver dongle.",
            "Configure receiver",
        );
    }

    // remote_access — always optional unless explicitly configured.
    push(
        &mut out,
        "remote_access",
        "Remote access",
        "optional",
        "Add a Cloudflare Tunnel for off-LAN access.",
        "Configure tunnel",
    );

    // finish — complete when finalized.
    if finalized {
        push(&mut out, "finish", "Finish", "complete", "Setup complete.", "");
    } else {
        push(
            &mut out,
            "finish",
            "Finish",
            "needs_action",
            "Confirm setup is complete.",
            "Finish",
        );
    }

    out
}

/// Live view of agent.yaml that the SetupStatus surface needs. Read on
/// every /status call so operator edits to the file flow through to
/// Mission Control on the next heartbeat without an agent restart.
#[derive(Debug, Clone)]
struct YamlView {
    agent_name: String,
    profile: String,
    ground_role: String,
    cloud_mode: String,
    cloud_url: String,
    api_bind: String,
}

impl Default for YamlView {
    fn default() -> Self {
        Self {
            agent_name: "ADOS Lite Agent".into(),
            profile: "drone".into(),
            ground_role: String::new(),
            cloud_mode: "cloud".into(),
            cloud_url: String::new(),
            api_bind: default_api_bind(),
        }
    }
}

fn read_yaml_view(path: &std::path::Path) -> YamlView {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return YamlView::default(),
    };
    let doc: serde_yaml::Value = match serde_yaml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return YamlView::default(),
    };
    let s = |path: &[&str]| -> Option<String> {
        let mut cur = &doc;
        for k in path {
            cur = cur.get(k)?;
        }
        cur.as_str().map(String::from)
    };
    YamlView {
        agent_name: s(&["agent", "name"]).unwrap_or_else(|| "ADOS Lite Agent".into()),
        profile: s(&["agent", "profile"]).unwrap_or_else(|| "drone".into()),
        ground_role: s(&["ground_station", "role"]).unwrap_or_default(),
        cloud_mode: s(&["cloud", "mode"]).unwrap_or_else(|| "cloud".into()),
        cloud_url: s(&["cloud", "convex_url"]).unwrap_or_default(),
        api_bind: s(&["api", "bind"]).unwrap_or_else(default_api_bind),
    }
}

/// Best-effort hostname read. Falls back to empty string on failure.
fn sys_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .or_else(|| {
            // macOS / non-Linux fallback via the `hostname` shell command.
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default()
}

/// Enumerate non-loopback IPv4 + IPv6 addresses by parsing /sys/class/net.
/// Best-effort; returns empty vec when the sysfs surface is unavailable.
/// We avoid the `nix` crate to keep the dep tree lean — `getifaddrs`
/// would be cleaner but for v1 reading sysfs + /proc/net/fib_trie is
/// pragmatic.
fn sys_local_ips() -> Vec<String> {
    // Easiest path that works on both Linux and macOS: shell out to
    // `hostname -I` (Linux) or `ipconfig getifaddr en0` (macOS) — but
    // those have different surfaces. Go with `ip -4 -o addr` which
    // exists on every Linux Buildroot rootfs.
    let out = std::process::Command::new("ip")
        .args(["-o", "-4", "addr", "show", "scope", "global"])
        .output();
    let mut ips = Vec::new();
    if let Ok(o) = out {
        if let Ok(text) = String::from_utf8(o.stdout) {
            for line in text.lines() {
                // Format: "2: wlan0    inet 192.168.200.225/24 brd ..."
                if let Some(idx) = line.find("inet ") {
                    let rest = &line[idx + 5..];
                    if let Some(end) = rest.find('/') {
                        ips.push(rest[..end].to_string());
                    }
                }
            }
        }
    }
    ips
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
    async fn persist_pair_code_writes_pairing_json() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        std::fs::write(
            &agent_yaml,
            r#"agent:
  device_id: "test"
  name: "Test"
cloud:
  mqtt_broker: ""
"#,
        )
        .unwrap();

        // Pair code goes to pairing.json (sibling of agent.yaml), NOT
        // to agent.yaml's cloud.api_key. The two values are different
        // per the proto: pair_code is a 6-char operator-typed code,
        // api_key is the longer ados_<base64url-32> the cloud relay
        // returns after a successful claim.
        persist_pair_code(&agent_yaml, "ABCD1234").await.unwrap();

        let pairing_path = dir.path().join("pairing.json");
        assert!(pairing_path.exists(), "pairing.json should be created");
        let store = ados_setup::pairing::PairingStore::new(&pairing_path);
        let state = store.load().unwrap();
        assert_eq!(state.pairing_code.as_deref(), Some("ABCD1234"));
        assert!(!state.is_paired(), "set_code clears the paired flag");
    }

    #[tokio::test]
    async fn persist_pair_code_uppercases_lowercase_input() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        std::fs::write(&agent_yaml, "agent:\n  device_id: \"x\"\n").unwrap();
        persist_pair_code(&agent_yaml, "abcd1234").await.unwrap();
        let pairing_path = dir.path().join("pairing.json");
        let state = ados_setup::pairing::PairingStore::new(&pairing_path)
            .load()
            .unwrap();
        // PairingStore::set_code uppercases — operator-typed lowercase
        // and uppercase resolve to the same canonical code.
        assert_eq!(state.pairing_code.as_deref(), Some("ABCD1234"));
    }

    #[tokio::test]
    async fn persist_pair_code_does_not_touch_agent_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let agent_yaml = dir.path().join("agent.yaml");
        let original = "agent:\n  device_id: \"test\"\n  name: \"Test\"\ncloud:\n  mqtt_broker: \"broker\"\n";
        std::fs::write(&agent_yaml, original).unwrap();
        persist_pair_code(&agent_yaml, "ABCD1234").await.unwrap();
        let after = std::fs::read_to_string(&agent_yaml).unwrap();
        assert_eq!(
            after, original,
            "agent.yaml must not be rewritten by pair (the code goes to pairing.json)"
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
