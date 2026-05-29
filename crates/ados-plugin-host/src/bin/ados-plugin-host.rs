//! `ados-plugin-host` binary.
//!
//! The runnable plugin-host daemon. It discovers installed plugins, serves a
//! per-plugin Unix socket for every enabled subprocess plugin, and routes each
//! plugin's RPC calls to the real host services (telemetry, config, camera,
//! MAVLink send/subscribe, driver registration, process-spawn allowlist).
//! Mirrors the `ados-supervisor` / `ados-mavlink-router` binary shape: journald
//! logging on Linux with an fmt fallback, a `Type=notify` readiness ping, and a
//! single select loop over the shutdown signals.
//!
//! Token delivery: the daemon owns the per-process [`TokenIssuer`]. Each plugin
//! runner must present a token minted by this issuer at the `hello` handshake.
//! Production handoff of a minted token to the systemd-started runner process
//! (a token file or a unit env) is a separate cutover concern and is NOT wired
//! here; the issuer is created per process and held in memory only. The smoke
//! test mints a token directly via this issuer to exercise the in-process wiring.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;

use ados_plugin_host::manifest::PluginManifest;
use ados_plugin_host::mavlink_client::MavlinkClient;
use ados_plugin_host::realhost::RealHost;
use ados_plugin_host::server::DEFAULT_SOCKET_DIR;
use ados_plugin_host::state::PluginStatus;
use ados_plugin_host::{EventBus, PluginIpcServer, PluginSupervisor};
use ados_protocol::plugin::TokenIssuer;

/// The run directory holding the IPC sockets the agent's other services bind
/// (`mavlink.sock`, `state.sock`). Overridable for tests / non-default layouts,
/// matching the `ADOS_RUN_DIR` env the MAVLink router honours.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The running agent semver, used by the supervisor's compatibility gate. The
/// `ADOS_AGENT_VERSION` env mirrors the Python `ados.__version__` source; the
/// crate version is the inert fallback when the env is unset.
fn agent_version() -> String {
    std::env::var("ADOS_AGENT_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    #[cfg(target_os = "linux")]
    {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&filter))
        .try_init();
}

/// systemd readiness ping. No-op off Linux and when not run under a
/// `Type=notify` unit (`NOTIFY_SOCKET` unset).
#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(not(target_os = "linux"))]
fn sd_ready() {}

/// Read a plugin's manifest off its unpacked install dir. Returns `None` when
/// the manifest is missing or unparseable (the daemon then skips serving that
/// plugin rather than failing the whole boot).
fn read_plugin_manifest(install_dir: &Path, plugin_id: &str) -> Option<PluginManifest> {
    let manifest_path = install_dir.join(plugin_id).join("manifest.yaml");
    let text = std::fs::read_to_string(&manifest_path).ok()?;
    match PluginManifest::from_yaml_text(&text) {
        Ok(m) => Some(m),
        Err(e) => {
            tracing::warn!(plugin_id, error = %e, "plugin manifest unreadable; skipping");
            None
        }
    }
}

/// The wired daemon: the server, the per-plugin accept handles, and the issuer.
/// Holding the (id, JoinHandle) map lets shutdown abort every accept task and
/// unlink every served socket.
struct WiredDaemon<H: ados_plugin_host::HostServices> {
    server: PluginIpcServer<H>,
    served: Vec<(String, JoinHandle<()>)>,
    issuer: Arc<TokenIssuer>,
}

impl<H: ados_plugin_host::HostServices> WiredDaemon<H> {
    /// Abort every accept task and unlink every served socket. New connections
    /// stop immediately; an in-flight connection's `release_plugin` already runs
    /// on disconnect.
    fn shutdown(self) {
        for (id, handle) in self.served {
            handle.abort();
            self.server.stop_plugin(&id);
        }
    }
}

/// Build the real host from a discovered supervisor: the five facades plus the
/// MAVLink client (best-effort), the runtime lookup (install dir + spawn
/// allowlist from each plugin's manifest), and the agent-id lookup.
///
/// The MAVLink slot stays `None` when the router socket is not up yet, which is
/// the correct `not_available` posture (a `mavlink.send` then returns the
/// structured `not_available` shape rather than failing the daemon).
async fn build_host(install_dir: PathBuf, run_dir: PathBuf) -> Arc<RealHost> {
    let mut host = RealHost::new();

    // (a) MAVLink client: best-effort connect to the router's socket. A connect
    //     failure is logged and the slot stays None.
    let mavlink_sock = run_dir.join("mavlink.sock");
    match MavlinkClient::connect(&mavlink_sock).await {
        Ok(client) => {
            tracing::info!(path = %mavlink_sock.display(), "mavlink client connected");
            host = host.with_mavlink(Arc::new(client));
        }
        Err(e) => {
            tracing::warn!(
                path = %mavlink_sock.display(),
                error = %e,
                "mavlink router socket unavailable; mavlink.send will report not_available"
            );
        }
    }

    // (b) Runtime lookup: plugin_id -> (install_dir, subprocess_spawn allowlist).
    //     The install dir is the unpacked plugin dir; the allowlist is read from
    //     that plugin's manifest agent.subprocess_spawn. A plugin with no
    //     manifest / no agent block resolves to None (the handler then returns
    //     the structured not_available shape).
    let lookup_install_dir = install_dir.clone();
    host = host.with_runtime_lookup(Box::new(move |plugin_id: &str| {
        let manifest = read_plugin_manifest(&lookup_install_dir, plugin_id)?;
        let agent = manifest.agent.as_ref()?;
        let allowlist: BTreeSet<String> = agent.subprocess_spawn.iter().cloned().collect();
        Some((lookup_install_dir.join(plugin_id), allowlist))
    }));

    // (c) Agent-id lookup: the empty string (unbound). A real drone-binding
    //     source is a follow-on; the empty id degrades config writes to global
    //     scope, matching the Python default.
    host = host.with_agent_id_lookup(Box::new(|_plugin_id: &str| String::new()));

    Arc::new(host)
}

/// Wire the daemon: build the host from the (already-discovered) supervisor,
/// construct the server, and serve every enabled subprocess plugin. Factored out
/// of `main` so the smoke test exercises the full wiring without `main()`.
///
/// `supervisor` must already have `discover()`-ed. The socket dir is where the
/// per-plugin sockets bind; the install dir / run dir feed the host lookups.
async fn wire(
    supervisor: &PluginSupervisor,
    install_dir: PathBuf,
    socket_dir: PathBuf,
    run_dir: PathBuf,
) -> WiredDaemon<RealHost> {
    let issuer = Arc::new(TokenIssuer::new_random());
    let bus = Arc::new(EventBus::new());
    let host = build_host(install_dir.clone(), run_dir).await;

    let server = PluginIpcServer::new(&socket_dir, issuer.clone(), bus, host);

    let mut served: Vec<(String, JoinHandle<()>)> = Vec::new();
    for install in supervisor.installs() {
        // Serve the enabled / running subprocess plugins. A built-in / inprocess
        // / gcs-only plugin has no per-plugin runner socket. Re-serving on a
        // later enable happens on daemon restart (the lifecycle controller
        // writes state; the daemon picks it up on next boot).
        if !matches!(
            install.status,
            PluginStatus::Enabled | PluginStatus::Running
        ) {
            continue;
        }
        let Some(manifest) = read_plugin_manifest(&install_dir, &install.plugin_id) else {
            continue;
        };
        if !manifest.is_subprocess_agent() {
            continue;
        }
        match server.serve_plugin(&install.plugin_id) {
            Ok((path, handle)) => {
                tracing::info!(
                    plugin_id = %install.plugin_id,
                    socket = %path.display(),
                    "serving plugin socket"
                );
                served.push((install.plugin_id.clone(), handle));
            }
            Err(e) => {
                tracing::warn!(
                    plugin_id = %install.plugin_id,
                    error = %e,
                    "failed to bind plugin socket"
                );
            }
        }
    }

    WiredDaemon {
        server,
        served,
        issuer,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let paths = ados_plugin_host::supervisor::Paths::default();
    let install_dir = paths.install_dir.clone();
    let socket_dir = PathBuf::from(DEFAULT_SOCKET_DIR);
    let run = run_dir();
    let version = agent_version();

    tracing::info!(
        install_dir = %install_dir.display(),
        socket_dir = %socket_dir.display(),
        run_dir = %run.display(),
        agent_version = %version,
        "plugin host daemon starting"
    );

    let mut supervisor = PluginSupervisor::new(paths, false, None, version);
    if let Err(e) = supervisor.discover() {
        tracing::error!(error = %e, "plugin discovery failed");
    }

    let daemon = wire(&supervisor, install_dir, socket_dir, run).await;
    // The per-process token issuer is owned by the daemon for the session
    // lifetime; the production handoff of a minted token to each runner process
    // is wired at cutover (see the module note). It is held, not minted-from,
    // in the daemon's own run path.
    let _ = &daemon.issuer;
    tracing::info!(served = daemon.served.len(), "plugin host daemon ready");

    sd_ready();

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("received SIGTERM"),
        _ = sigint.recv() => tracing::info!("received SIGINT"),
    }

    tracing::info!("plugin host daemon stopping");
    daemon.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_plugin_host::supervisor::{Paths, RecordingSystemctl};
    use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
    use ados_protocol::plugin::{Envelope, PROTOCOL_VERSION};
    use rmpv::Value;
    use std::collections::BTreeSet;
    use std::io::Write;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixStream;
    use zip::write::SimpleFileOptions;

    const PLUGIN_ID: &str = "com.example.thermal";
    const SUBPROC_MANIFEST: &str = "id: com.example.thermal\nversion: 1.0.0\nrisk: high\ncompatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  subprocess_spawn:\n    - ffmpeg\n";

    fn paths_in(dir: &Path) -> Paths {
        Paths {
            install_dir: dir.join("plugins"),
            unit_dir: dir.join("units"),
            state_path: dir.join("state/plugin-state.json"),
            log_dir: dir.join("logs"),
        }
    }

    fn build_unsigned_archive(manifest_yaml: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            w.start_file("manifest.yaml", opts).unwrap();
            w.write_all(manifest_yaml.as_bytes()).unwrap();
            w.start_file("agent/py/x.py", opts).unwrap();
            w.write_all(b"print('hi')").unwrap();
            w.finish().unwrap();
        }
        buf
    }

    fn caps(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    async fn recv(stream: &mut UnixStream) -> Envelope {
        let mut header = [0u8; HEADER_SIZE];
        stream.read_exact(&mut header).await.expect("read header");
        let len = decode_len(header, PLUGIN_MAX_FRAME, true).expect("decode len");
        let mut body = vec![0u8; len];
        stream.read_exact(&mut body).await.expect("read body");
        Envelope::from_msgpack(&body).expect("decode envelope")
    }

    async fn send(stream: &mut UnixStream, env: &Envelope) {
        let frame = env.encode_frame().expect("encode frame");
        stream.write_all(&frame).await.expect("write frame");
        stream.flush().await.expect("flush");
    }

    fn request(method: &str, token: &str) -> Envelope {
        Envelope {
            version: PROTOCOL_VERSION,
            kind: "request".to_string(),
            method: method.to_string(),
            capability: String::new(),
            args: Value::Map(vec![]),
            request_id: format!("req-{method}"),
            token: token.to_string(),
            error: None,
        }
    }

    fn args_bool(env: &Envelope, key: &str) -> Option<bool> {
        match &env.args {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some(key))
                .and_then(|(_, v)| v.as_bool()),
            _ => None,
        }
    }

    #[tokio::test]
    async fn wiring_serves_an_enabled_plugin_and_pings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(dir.path());
        let install_dir = paths.install_dir.clone();
        let socket_dir = dir.path().join("sockets");
        let run = dir.path().join("run");

        // Install + enable one subprocess plugin via a recording systemctl, then
        // discover so the install is in memory.
        let rec = Arc::new(RecordingSystemctl::default());
        let mut supervisor =
            PluginSupervisor::new(paths, false, None, "1.0.0").with_systemctl(rec.clone());
        let archive = ados_plugin_host::archive::parse_archive_bytes(build_unsigned_archive(
            SUBPROC_MANIFEST,
        ))
        .expect("parse archive");
        supervisor
            .install_contents(archive, Path::new("/tmp/thermal.adosplug"))
            .expect("install");
        supervisor.enable(PLUGIN_ID).expect("enable");
        supervisor.discover().expect("discover");

        // Wire the daemon (no mavlink router up -> slot stays None, fine).
        let daemon = wire(&supervisor, install_dir, socket_dir.clone(), run).await;
        assert_eq!(
            daemon.served.len(),
            1,
            "the enabled plugin should be served"
        );

        // Mint a token via the daemon's issuer (the smoke-test handoff) and ping.
        let token = daemon
            .issuer
            .mint(PLUGIN_ID, &caps(&[]), 600)
            .to_token_string();
        let sock_path = socket_dir.join(format!("{PLUGIN_ID}.sock"));
        let mut client = {
            let mut s = None;
            for _ in 0..50 {
                if let Ok(c) = UnixStream::connect(&sock_path).await {
                    s = Some(c);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            s.expect("connect to plugin socket")
        };

        send(&mut client, &request("hello", &token)).await;
        let ready = recv(&mut client).await;
        assert_eq!(args_bool(&ready, "ready"), Some(true));

        send(&mut client, &request("ping", &token)).await;
        let pong = recv(&mut client).await;
        assert_eq!(pong.error, None);
        assert_eq!(args_bool(&pong, "pong"), Some(true));

        // Shutdown aborts the accept task and unlinks the socket.
        daemon.shutdown();
        assert!(!sock_path.exists(), "socket should be gone after shutdown");
    }

    #[tokio::test]
    async fn build_host_runtime_lookup_reads_spawn_allowlist() {
        // The runtime lookup resolves a plugin to its install dir + manifest
        // subprocess_spawn allowlist. Unpack a manifest and confirm process.spawn
        // authorizes an allowlisted basename and denies one outside it.
        let dir = tempfile::tempdir().expect("tempdir");
        let install_dir = dir.path().join("plugins");
        let plugin_dir = install_dir.join(PLUGIN_ID);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("manifest.yaml"), SUBPROC_MANIFEST).unwrap();

        let host = build_host(install_dir, dir.path().join("run")).await;

        use ados_plugin_host::HostServices;
        let hit = Value::Map(vec![(Value::from("basename"), Value::from("ffmpeg"))]);
        let res = host.process_spawn(PLUGIN_ID, &hit).expect("authorized");
        let authorized = match &res {
            Value::Map(m) => m
                .iter()
                .find(|(k, _)| k.as_str() == Some("authorized"))
                .and_then(|(_, v)| v.as_bool()),
            _ => None,
        };
        assert_eq!(authorized, Some(true));

        let miss = Value::Map(vec![(Value::from("basename"), Value::from("rm"))]);
        let err = host.process_spawn(PLUGIN_ID, &miss).expect_err("denied");
        assert_eq!(err.body(), "allowlist_violation: rm");
    }
}
