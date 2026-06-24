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
//! Token delivery: the daemon builds its [`TokenIssuer`] from a *persisted*
//! HMAC secret (0600 under `/etc/ados/secrets`), not a per-process random key,
//! so a runner started by its own systemd unit can present a token this daemon
//! verifies. When the daemon serves a plugin's socket it mints a fresh token
//! from that shared issuer and writes the 0600 env file the unit references via
//! `EnvironmentFile=`; the runner reads `ADOS_PLUGIN_TOKEN` / `ADOS_PLUGIN_SOCKET`
//! from its environment and connects. The token rotates on every serve (daemon
//! restart) and on every permission change (the granted caps feed the mint).

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
use ados_plugin_host::vision_client::VisionClient;
use ados_plugin_host::{EventBus, PluginIpcServer, PluginSupervisor};
use ados_protocol::plugin::TokenIssuer;

/// The run directory holding the IPC sockets the agent's other services bind
/// (`mavlink.sock`, `state.sock`). Overridable for tests / non-default layouts,
/// matching the `ADOS_RUN_DIR` env the MAVLink router honours.
fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string()))
}

/// The per-plugin socket directory, where the per-plugin sockets, token env
/// files, and the published-state sidecars live. Honours `ADOS_PLUGIN_SOCKET_DIR`
/// (the same env the native front reads to locate a plugin's state sidecar),
/// defaulting to [`DEFAULT_SOCKET_DIR`] so both daemons agree on the path.
fn plugin_socket_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_PLUGIN_SOCKET_DIR").unwrap_or_else(|_| DEFAULT_SOCKET_DIR.to_string()),
    )
}

/// The running agent semver, used by the supervisor's compatibility gate. The
/// `ADOS_AGENT_VERSION` env mirrors the Python `ados.__version__` source; the
/// crate version is the inert fallback when the env is unset.
fn agent_version() -> String {
    std::env::var("ADOS_AGENT_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// The persisted HMAC issuer secret path. Overridable via
/// `ADOS_PLUGIN_TOKEN_SECRET` for tests / non-default layouts; otherwise the
/// 0600 file under `/etc/ados/secrets` both the daemon and the unit-generation
/// path read so a runner's token verifies in this daemon.
fn secret_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_PLUGIN_TOKEN_SECRET")
            .unwrap_or_else(|_| ados_plugin_host::PLUGIN_TOKEN_SECRET_PATH.to_string()),
    )
}

fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket
    // alongside the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-plugin-host"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-plugin-host"))
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
    /// The daemon-lifetime control socket (the on-box config-write reach for the
    /// native `ados-control` plugin-config route), and its accept task. Bound
    /// once for the whole daemon, not per plugin.
    control: Option<(PathBuf, JoinHandle<()>)>,
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
        if let Some((path, handle)) = self.control {
            handle.abort();
            let _ = std::fs::remove_file(&path);
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

    // (b) Vision client: best-effort connect to the engine's socket so the
    //     three vision request methods proxy to it and the frame-descriptor
    //     stream arms. When the engine is not up the slot stays None and the
    //     vision methods return the not_implemented shape, matching the MAVLink
    //     not_available posture.
    let vision_sock = run_dir.join("vision.sock");
    match VisionClient::connect(&vision_sock).await {
        Ok(client) => {
            tracing::info!(path = %vision_sock.display(), "vision client connected");
            host = host.with_vision(Arc::new(client));
        }
        Err(e) => {
            tracing::warn!(
                path = %vision_sock.display(),
                error = %e,
                "vision engine socket unavailable; vision methods will report not_implemented"
            );
        }
    }

    // (c) Runtime lookup: plugin_id -> (install_dir, subprocess_spawn allowlist).
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

    // (d) Agent-id lookup: the paired device id read once at startup from
    //     /etc/ados/device-id (overridable for tests / non-default layouts).
    //     This isolates a drone-scoped config write per drone instead of
    //     silently collapsing every drone write to global. The id is the same
    //     for every plugin on one drone, so it is resolved once and cloned.
    let device_id = read_device_id();
    if device_id.is_empty() {
        tracing::warn!(
            "device id not resolved; drone-scoped plugin config will fall back to global"
        );
    } else {
        tracing::info!(device_id = %device_id, "plugin config drone scope bound to device id");
    }
    host = host.with_agent_id_lookup(Box::new(move |_plugin_id: &str| device_id.clone()));

    // (e) Config persistence: a 0600 JSON store so plugin config survives a
    //     plugin-host restart instead of living only in memory.
    host = host.with_config_persistence(config_store_path());

    Arc::new(host)
}

/// The paired device id, read from `/etc/ados/device-id` (the persistent 12-char
/// hex identity the agent writes on first boot). Overridable via
/// `ADOS_DEVICE_ID_PATH` for tests / non-default layouts. Returns the empty
/// string when the file is absent or unreadable (an unpaired / pre-first-boot
/// drone), which degrades drone-scoped config to global, matching the prior
/// behaviour without a device id.
fn read_device_id() -> String {
    let path =
        std::env::var("ADOS_DEVICE_ID_PATH").unwrap_or_else(|_| "/etc/ados/device-id".to_string());
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// The plugin-config persistence file. Lives under the agent's etc dir so it
/// survives a restart; overridable via `ADOS_PLUGIN_CONFIG_PATH` for tests.
fn config_store_path() -> PathBuf {
    PathBuf::from(
        std::env::var("ADOS_PLUGIN_CONFIG_PATH")
            .unwrap_or_else(|_| "/etc/ados/plugin-config.json".to_string()),
    )
}

/// Wire the daemon: build the host from the (already-discovered) supervisor,
/// construct the server, and serve every enabled subprocess plugin. Factored out
/// of `main` so the smoke test exercises the full wiring without `main()`.
///
/// `supervisor` must already have `discover()`-ed. The socket dir is where the
/// per-plugin sockets bind; the install dir / run dir feed the host lookups.
/// `secret_path` is the persisted HMAC secret the issuer is built from; the
/// same file feeds the unit-generation path, so a runner's token verifies here.
async fn wire(
    supervisor: &PluginSupervisor,
    install_dir: PathBuf,
    socket_dir: PathBuf,
    run_dir: PathBuf,
    secret_path: &Path,
) -> WiredDaemon<RealHost> {
    // Build the issuer from the persisted secret so a runner started by its own
    // unit can present a token this daemon verifies. A failure to read/create
    // the secret falls back to a per-process random key (the in-process smoke
    // test still works; cross-process verify degrades, surfaced by the warn).
    let issuer = Arc::new(match ados_plugin_host::shared_issuer(secret_path) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                path = %secret_path.display(),
                error = %e,
                "persisted token secret unavailable; falling back to a per-process key \
                 (runner tokens will not verify cross-process)"
            );
            TokenIssuer::new_random()
        }
    });
    let bus = Arc::new(EventBus::new());
    let host = build_host(install_dir.clone(), run_dir).await;

    // The on-box control socket: the native `ados-control` plugin-config route
    // reaches this daemon's live `ConfigStore` through it (a GCS skill toggle /
    // per-drone settings change for a plugin the writer is not). Bound once for
    // the whole daemon on the shared host Arc, before the host moves into the
    // per-plugin server. A bind failure is non-fatal: plugin RPC still works,
    // only the off-box config-write reach is unavailable.
    let control = match ados_plugin_host::serve_control(host.clone(), socket_dir.clone()) {
        Ok((path, handle)) => {
            tracing::info!(socket = %path.display(), "serving plugin-host control socket");
            Some((path, handle))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "failed to bind plugin-host control socket; GCS plugin config writes \
                 will not reach the live host"
            );
            None
        }
    };

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
                // Mint the runner's token from the shared issuer and write the
                // 0600 env file the unit references. The runner then reads
                // ADOS_PLUGIN_TOKEN / ADOS_PLUGIN_SOCKET from its environment
                // and connects. The granted caps come from the install record.
                let caps = ados_plugin_host::state::granted_caps(install);
                if let Err(e) = ados_plugin_host::write_token_env(
                    &issuer,
                    &install.plugin_id,
                    &caps,
                    &path,
                    Some(&socket_dir),
                ) {
                    tracing::warn!(
                        plugin_id = %install.plugin_id,
                        error = %e,
                        "failed to write plugin token env; runner will fall back to null IPC"
                    );
                }
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
        control,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let paths = ados_plugin_host::supervisor::Paths::default();
    let install_dir = paths.install_dir.clone();
    // The per-plugin + control + state-sidecar socket dir. Honours
    // `ADOS_PLUGIN_SOCKET_DIR` (the same env `ados-control` reads) so a test /
    // SITL run points both daemons at a writable tempdir instead of
    // `/run/ados/plugins`.
    let socket_dir = plugin_socket_dir();
    let run = run_dir();
    let version = agent_version();

    tracing::info!(
        install_dir = %install_dir.display(),
        socket_dir = %socket_dir.display(),
        run_dir = %run.display(),
        agent_version = %version,
        "plugin host daemon starting"
    );

    // The lifecycle controller refuses to grant a capability the default Rust
    // host cannot back (its host method returns not_implemented regardless of
    // wiring) so an operator never hands out a capability that can only error at
    // call time. `require_signed=true` is the safe production default for the
    // live install path.
    let mut supervisor = PluginSupervisor::production(paths, None, version)
        .with_ungrantable_caps(RealHost::ungrantable_caps());
    if let Err(e) = supervisor.discover() {
        tracing::error!(error = %e, "plugin discovery failed");
    }

    let secret = secret_path();
    let daemon = wire(&supervisor, install_dir, socket_dir, run, &secret).await;
    // The shared-secret issuer is owned by the daemon for the session lifetime;
    // it both verifies the runner's `hello` token and (in `wire`) minted the
    // token env file each served plugin's unit reads.
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
        // A tempdir secret path makes the issuer persist a shared secret.
        let secret = dir.path().join("secrets/plugin-token-secret");
        let daemon = wire(&supervisor, install_dir, socket_dir.clone(), run, &secret).await;
        assert_eq!(
            daemon.served.len(),
            1,
            "the enabled plugin should be served"
        );

        // The serve path wrote the runner's token env file from the shared
        // issuer; the token it carries must verify against a SEPARATE issuer
        // reloaded from the same persisted secret (the cross-process contract:
        // a runner unit's env file is consumed by a different process). The
        // ping below then proves the daemon itself accepts that exact token.
        let env_path = ados_plugin_host::token_env_path(PLUGIN_ID, Some(&socket_dir));
        let env_body = std::fs::read_to_string(&env_path).expect("token env written");
        let token_line = env_body
            .lines()
            .find_map(|l| l.strip_prefix("ADOS_PLUGIN_TOKEN="))
            .expect("ADOS_PLUGIN_TOKEN in env file");
        let parsed = ados_protocol::plugin::CapabilityToken::from_token_string(token_line)
            .expect("env token parses");
        let reloaded_issuer = ados_plugin_host::shared_issuer(&secret).expect("reload issuer");
        let now = parsed.issued_at + 1;
        assert!(
            reloaded_issuer.verify(&parsed, now).is_ok(),
            "the runner's env token must verify in a separately-constructed issuer"
        );

        // Use that very token (as a runner would, read from its env) for the
        // hello + ping, proving the daemon accepts it.
        let token = token_line.to_string();
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

    /// The generated unit (for BOTH the Python and the Rust runtime branches)
    /// references the EnvironmentFile the daemon writes, with a socket path that
    /// matches what the env file declares, and the env token verifies against an
    /// issuer built from the same persisted secret. This is the unit-level proof
    /// that "the generated unit env + token + socket are consistent" for both
    /// runtimes — the practical stand-in for a full live two-process launch.
    #[test]
    fn unit_env_token_and_socket_are_consistent_for_both_runtimes() {
        use ados_plugin_host::systemd::render_unit;
        use ados_protocol::plugin::CapabilityToken;

        for runtime in ["python", "rust"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_dir = dir.path().join("plugins");
            let secret = dir.path().join("secrets/plugin-token-secret");
            let plugin_id = "com.example.consistency";

            // The unit the install path writes: it must reference the env file
            // and carry the static socket Environment line.
            let manifest_yaml = format!(
                "id: {plugin_id}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\nagent:\n  entrypoint: agent/x\n  runtime: {runtime}\n  permissions:\n    - mavlink.read\n"
            );
            let manifest =
                ados_plugin_host::PluginManifest::from_yaml_text(&manifest_yaml).expect("manifest");
            let unit = render_unit(&manifest, dir.path()).expect("unit");
            // Both runtimes deliver the token via the same env file + static
            // socket Environment line.
            assert!(
                unit.contains(
                    "EnvironmentFile=-/run/ados/plugins/com.example.consistency.token.env"
                ),
                "{runtime} unit missing EnvironmentFile: {unit}"
            );
            assert!(
                unit.contains(
                    "Environment=ADOS_PLUGIN_SOCKET=/run/ados/plugins/com.example.consistency.sock"
                ),
                "{runtime} unit missing socket Environment: {unit}"
            );
            // Never on the command line.
            let exec = unit
                .lines()
                .find(|l| l.starts_with("ExecStart="))
                .expect("ExecStart");
            assert!(!exec.to_lowercase().contains("token"), "{exec}");

            // The daemon-side: an issuer from the persisted secret mints the
            // runner token and writes the env file. A separately-built issuer
            // (a stand-in for the serving daemon process) verifies it.
            let minting = ados_plugin_host::shared_issuer(&secret).expect("issuer");
            let sock = socket_dir.join(format!("{plugin_id}.sock"));
            ados_plugin_host::write_token_env(
                &minting,
                plugin_id,
                &caps(&["mavlink.read"]),
                &sock,
                Some(&socket_dir),
            )
            .expect("write env");

            let env_path = ados_plugin_host::token_env_path(plugin_id, Some(&socket_dir));
            let body = std::fs::read_to_string(&env_path).expect("env file");
            let token_line = body
                .lines()
                .find_map(|l| l.strip_prefix("ADOS_PLUGIN_TOKEN="))
                .expect("token line");
            let socket_line = body
                .lines()
                .find_map(|l| l.strip_prefix("ADOS_PLUGIN_SOCKET="))
                .expect("socket line");
            assert_eq!(socket_line, sock.to_string_lossy());

            let parsed = CapabilityToken::from_token_string(token_line).expect("parse");
            assert_eq!(parsed.plugin_id, plugin_id);
            let verifying = ados_plugin_host::shared_issuer(&secret).expect("issuer 2");
            assert!(
                verifying.verify(&parsed, parsed.issued_at + 1).is_ok(),
                "{runtime}: env token must verify against the shared persisted secret"
            );
        }
    }
}
