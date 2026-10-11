use super::*;
use crate::backend::RecordingBackend;
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
const SUBPROC_MANIFEST: &str = "id: com.example.thermal\nversion: 1.0.0\nrisk: high\ncompatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n    - process.spawn\n  subprocess_spawn:\n    - ffmpeg\n";

fn paths_in(dir: &Path) -> Paths {
    Paths {
        install_dir: dir.join("plugins"),
        unit_dir: dir.join("units"),
        state_path: dir.join("state/plugin-state.json"),
        log_dir: dir.join("logs"),
        control_dir: dir.join("plugin-host"),
        loopback_guard_state: dir.join("plugin-loopback-guard.json"),
        socket_dir: dir.join("sockets"),
        token_secret: dir.join("secrets/plugin-token-secret"),
        runner: dir.join("bin/ados-plugin-runner"),
        run_dir: dir.join("run"),
        data_root: dir.join("plugin-data"),
        device_id_file: dir.join("device-id"),
    }
}

fn build_unsigned_archive(manifest_yaml: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
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
    let socket_dir = paths.socket_dir.clone();
    let secret = paths.token_secret.clone();

    // Install + enable one subprocess plugin via a recording backend, then
    // discover so the install is in memory.
    let rec = Arc::new(RecordingBackend::default());
    let mut supervisor =
        PluginSupervisor::new(paths.clone(), false, None, "1.0.0").with_backend(rec.clone());
    let archive = crate::archive::parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST))
        .expect("parse archive");
    supervisor
        .install_contents(archive, Path::new("/tmp/thermal.adosplug"))
        .expect("install");
    supervisor.enable(PLUGIN_ID).expect("enable");
    supervisor.discover().expect("discover");

    // Wire the daemon (no mavlink router up -> slot stays None, fine).
    // A tempdir secret path makes the issuer persist a shared secret.
    let daemon = wire(&paths, Arc::new(VehicleArmedProbe::new(false))).await;
    assert_eq!(
        daemon.reconciler.serving().len(),
        1,
        "the enabled plugin should be served"
    );

    // The serve path wrote the runner's token env file from the shared
    // issuer; the token it carries must verify against a SEPARATE issuer
    // reloaded from the same persisted secret (the cross-process contract:
    // a runner unit's env file is consumed by a different process). The
    // ping below then proves the daemon itself accepts that exact token.
    let env_path = crate::token_credential_path(PLUGIN_ID, Some(&socket_dir));
    let env_body = std::fs::read_to_string(&env_path).expect("token credential written");
    let token_line = env_body
        .lines()
        .find_map(|l| l.strip_prefix("ADOS_PLUGIN_TOKEN="))
        .expect("ADOS_PLUGIN_TOKEN in env file");
    let parsed = ados_protocol::plugin::CapabilityToken::from_token_string(token_line)
        .expect("env token parses");
    let reloaded_issuer = crate::shared_issuer(&secret).expect("reload issuer");
    let now = parsed.issued_at + 1;
    assert!(
        reloaded_issuer.verify(&parsed, now).is_ok(),
        "the runner's env token must verify in a separately-constructed issuer"
    );

    // Use that very token (as a runner would, read from its env) for the
    // hello + ping, proving the daemon accepts it.
    let token = token_line.to_string();
    let sock_path = crate::plugin_socket_path(&socket_dir, PLUGIN_ID);
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
    assert_eq!(ready.error, None, "hello was refused: {ready:?}");

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

    let host = build_host(
        install_dir,
        dir.path().join("run"),
        dir.path().join("state/plugin-state.json"),
        String::new(),
        &Arc::new(EventBus::new()),
        Arc::new(VehicleArmedProbe::new(false)),
    )
    .await;

    use crate::HostServices;
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
/// loads the token credential the daemon writes, with a socket path that
/// matches what the credential declares, and the token verifies against an
/// issuer built from the same persisted secret. This is the unit-level proof
/// that "the generated unit credential + token + socket are consistent" for
/// both runtimes — the practical stand-in for a full live two-process launch.
#[test]
fn unit_env_token_and_socket_are_consistent_for_both_runtimes() {
    use crate::backend::render_systemd;
    use crate::systemd::build_unit_spec;
    use ados_protocol::plugin::CapabilityToken;

    for runtime in ["python", "rust"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(dir.path());
        let socket_dir = paths.socket_dir.clone();
        let secret = paths.token_secret.clone();
        let plugin_id = "com.example.consistency";

        // The unit the install path writes: it must reference the env file
        // the mint writes and carry the static socket Environment line.
        let manifest_yaml = format!(
                "id: {plugin_id}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\nagent:\n  entrypoint: agent/x\n  runtime: {runtime}\n  permissions:\n    - mavlink.read\n"
            );
        let manifest = crate::PluginManifest::from_yaml_text(&manifest_yaml).expect("manifest");
        let unit = render_systemd(
            &build_unit_spec(&manifest, &paths, "drone", &BTreeSet::new(), false)
                .expect("build")
                .expect("unit"),
        );
        let env_file = crate::token_credential_path(plugin_id, Some(&socket_dir));
        let sock = crate::plugin_socket_path(&socket_dir, plugin_id);
        // Both runtimes receive the token as the same credential, plus the
        // static socket Environment line.
        assert!(
            unit.contains(&format!(
                "LoadCredential=ados-plugin-token:{}",
                env_file.display()
            )),
            "{runtime} unit missing LoadCredential: {unit}"
        );
        assert!(
            unit.contains(&format!(
                "Environment=ADOS_PLUGIN_SOCKET={}",
                sock.display()
            )),
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
        let minting = crate::shared_issuer(&secret).expect("issuer");
        crate::write_token_credential(
            &minting,
            plugin_id,
            &caps(&["mavlink.read"]),
            &sock,
            &paths.data_root,
            "test-drone",
            Some(&socket_dir),
        )
        .expect("write env");

        let body = std::fs::read_to_string(&env_file).expect("env file");
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
        let verifying = crate::shared_issuer(&secret).expect("issuer 2");
        assert!(
            verifying.verify(&parsed, parsed.issued_at + 1).is_ok(),
            "{runtime}: env token must verify against the shared persisted secret"
        );
    }
}

/// Injector arbitration is OFF unless the flag is explicitly set true. The
/// default-off path is the load-bearing invariant: with no flag, no config,
/// or a config that omits it, the host declares no injector and the gate
/// stays inert. Proven-to-bite: only `injector_arbitration: true` flips it.
#[test]
fn injector_arbitration_defaults_off_and_arms_only_when_set() {
    // Explicit true → armed.
    assert!(parse_injector_arbitration(
        "mavlink:\n  injector_arbitration: true\n"
    ));
    // Explicit false, absent key, absent section, empty doc → off.
    assert!(!parse_injector_arbitration(
        "mavlink:\n  injector_arbitration: false\n"
    ));
    assert!(!parse_injector_arbitration(
        "mavlink:\n  legacy_stream_request: true\n"
    ));
    assert!(!parse_injector_arbitration("radio:\n  region: US\n"));
    assert!(!parse_injector_arbitration(""));
    // Garbage YAML → off (never accidentally arms).
    assert!(!parse_injector_arbitration("::: not yaml :::"));
}
