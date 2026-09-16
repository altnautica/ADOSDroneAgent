//! The reactive lifecycle: enable, rotate and revoke without a restart.
//!
//! Three failures shared one cause — the daemon took a snapshot at boot where a
//! continuously-maintained equality was needed:
//!
//! 1. A plugin enabled after the daemon started got no socket and no token, so
//!    it ran inert while systemd reported active and the GCS reported running.
//! 2. Capability tokens expired after 600 s and nothing re-minted them, so
//!    every gated call from every plugin failed `token_expired` ten minutes in.
//! 3. A revoke only took effect at the next daemon restart, so the CLI and the
//!    GCS reported a security control applied while the plugin kept its old
//!    grant set.
//!
//! These tests drive the real [`PluginReconciler`] against a real
//! [`PluginIpcServer`] over a real Unix socket, because the whole point is
//! behaviour across a process boundary that a unit test of either half would
//! miss.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ados_plugin_host::state::{self, PermissionGrant, PluginInstall, PluginSource, PluginStatus};
use ados_plugin_host::token_secret::TokenMint;
use ados_plugin_host::{EventBus, NoopHost, PluginIpcServer, PluginReconciler};
use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{CapabilityToken, Envelope, PROTOCOL_VERSION};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

const PLUGIN_ID: &str = "com.example.reactive";

const MANIFEST: &str = "id: com.example.reactive\nversion: 1.0.0\nrisk: medium\n\
compatibility:\n  ados_version: \">=0.1.0,<99.0.0\"\n\
agent:\n  entrypoint: plugin:Reactive\n  isolation: subprocess\n";

/// Write the plugin's unpacked manifest so the reconciler sees a subprocess
/// agent half.
fn write_manifest(install_dir: &Path) {
    let dir = install_dir.join(PLUGIN_ID);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.yaml"), MANIFEST).unwrap();
}

/// Write the plugin state file the way a lifecycle controller would.
fn write_state(state_path: &Path, status: PluginStatus, granted: &[&str]) {
    let mut permissions: BTreeMap<String, PermissionGrant> = BTreeMap::new();
    for cap in granted {
        permissions.insert(
            cap.to_string(),
            PermissionGrant {
                granted: true,
                granted_at: Some(1),
                revoked_at: None,
            },
        );
    }
    let install = PluginInstall {
        plugin_id: PLUGIN_ID.to_string(),
        version: "1.0.0".to_string(),
        source: PluginSource::LocalFile,
        source_uri: None,
        signer_id: None,
        manifest_hash: String::new(),
        status,
        installed_at: 1,
        enabled_at: Some(1),
        failure_reason: None,
        permissions,
        auto_update: true,
        pinned_version: None,
        last_update_check_at: None,
        last_update_attempt: None,
        model_status: None,
        service_status: None,
    };
    state::save_state(&[install], Some(state_path)).unwrap();
}

fn token_from_env(socket_dir: &Path) -> String {
    let path = ados_plugin_host::token_env_path(PLUGIN_ID, Some(socket_dir));
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("token env at {}: {e}", path.display()));
    body.lines()
        .find_map(|l| l.strip_prefix("ADOS_PLUGIN_TOKEN="))
        .expect("ADOS_PLUGIN_TOKEN in the env file")
        .to_string()
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

async fn send(stream: &mut UnixStream, env: &Envelope) {
    stream
        .write_all(&env.encode_frame().unwrap())
        .await
        .unwrap();
    stream.flush().await.unwrap();
}

async fn recv(stream: &mut UnixStream) -> Envelope {
    let mut header = [0u8; HEADER_SIZE];
    stream.read_exact(&mut header).await.unwrap();
    let len = decode_len(header, PLUGIN_MAX_FRAME, true).unwrap();
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await.unwrap();
    Envelope::from_msgpack(&body).unwrap()
}

fn arg_bool(env: &Envelope, key: &str) -> Option<bool> {
    match &env.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_bool()),
        _ => None,
    }
}

fn arg_str(env: &Envelope, key: &str) -> Option<String> {
    match &env.args {
        Value::Map(m) => m
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .and_then(|(_, v)| v.as_str())
            .map(str::to_string),
        _ => None,
    }
}

/// Everything a wired daemon needs, pointed at a tempdir.
struct Harness {
    _dir: tempfile::TempDir,
    socket_dir: std::path::PathBuf,
    state_path: std::path::PathBuf,
    secret_path: std::path::PathBuf,
    reconciler: Arc<PluginReconciler<NoopHost>>,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let socket_dir = dir.path().join("sockets");
    let state_path = dir.path().join("state/plugin-state.json");
    let install_dir = dir.path().join("plugins");
    let secret = dir.path().join("secrets/plugin-token-secret");
    std::fs::create_dir_all(&socket_dir).unwrap();
    write_manifest(&install_dir);

    let issuer = Arc::new(ados_plugin_host::shared_issuer(&secret).unwrap());
    let mint = Arc::new(TokenMint::new(
        issuer.clone(),
        state_path.clone(),
        socket_dir.clone(),
        String::new(),
    ));
    let server = Arc::new(
        PluginIpcServer::new(
            &socket_dir,
            issuer,
            Arc::new(EventBus::new()),
            Arc::new(NoopHost::default()),
        )
        .with_token_mint(mint.clone()),
    );
    let reconciler = Arc::new(PluginReconciler::new(
        server,
        mint,
        state_path.clone(),
        install_dir.clone(),
    ));
    Harness {
        _dir: dir,
        socket_dir,
        state_path,
        secret_path: secret,
        reconciler,
    }
}

#[tokio::test]
async fn enabling_a_plugin_serves_it_without_restarting_the_daemon() {
    let h = harness();

    // The daemon is already up and has reconciled once with nothing installed,
    // which is exactly the situation an `ados plugin enable` lands in.
    assert_eq!(h.reconciler.reconcile().serving, 0);
    let sock = h.socket_dir.join(format!("{PLUGIN_ID}.sock"));
    assert!(!sock.exists(), "nothing should be served yet");

    // The lifecycle controller writes state and pokes the daemon. No restart.
    write_state(&h.state_path, PluginStatus::Enabled, &["mavlink.write"]);
    let report = h.reconciler.reconcile();
    assert_eq!(report.started, 1);
    assert_eq!(report.serving, 1);

    // Both halves of the bridge exist now: a bound socket AND a token env file.
    // Before this, `enable` produced neither and the runner fell through to a
    // null IPC client — active to systemd, inert in fact.
    assert!(sock.exists(), "the socket must be bound");
    let token = token_from_env(&h.socket_dir);

    // And the plugin can actually call a host method with that token.
    let mut client = UnixStream::connect(&sock).await.unwrap();
    send(&mut client, &request("hello", &token)).await;
    let ready = recv(&mut client).await;
    assert_eq!(ready.error, None, "{ready:?}");
    assert_eq!(arg_bool(&ready, "ready"), Some(true));
    send(&mut client, &request("ping", &token)).await;
    let pong = recv(&mut client).await;
    assert_eq!(pong.error, None, "{pong:?}");
    assert_eq!(arg_bool(&pong, "pong"), Some(true));
}

#[tokio::test]
async fn disabling_a_plugin_tears_its_socket_and_token_down() {
    let h = harness();
    write_state(&h.state_path, PluginStatus::Running, &[]);
    assert_eq!(h.reconciler.reconcile().started, 1);
    let sock = h.socket_dir.join(format!("{PLUGIN_ID}.sock"));
    let env_path = ados_plugin_host::token_env_path(PLUGIN_ID, Some(&h.socket_dir));
    assert!(sock.exists() && env_path.exists());

    write_state(&h.state_path, PluginStatus::Disabled, &[]);
    let report = h.reconciler.reconcile();
    assert_eq!(report.stopped, 1);
    assert_eq!(report.serving, 0);
    // A disabled plugin must not leave a live token on tmpfs: it is a
    // credential for host access the operator has just withdrawn.
    assert!(!sock.exists(), "socket should be unlinked");
    assert!(!env_path.exists(), "token env should be removed");
}

#[tokio::test]
async fn an_expired_token_is_re_minted_in_place_instead_of_failing() {
    let h = harness();
    write_state(&h.state_path, PluginStatus::Running, &["mavlink.read"]);
    h.reconciler.reconcile();
    let sock = h.socket_dir.join(format!("{PLUGIN_ID}.sock"));

    // Open a session with a token that is valid now and expires in one second.
    // Driving the clock this way reproduces exactly the state a long-running
    // plugin used to reach at the ten-minute mark: the handshake happened while
    // the token was valid, and it aged out mid-flight. The host gates every
    // request against the SESSION token, so the expiry has to happen to that
    // one — putting a stale token in a later envelope proves nothing.
    //
    // `shared_issuer` hex-decodes the persisted secret; constructing from the
    // raw file bytes would sign with a different key.
    let issuer = ados_plugin_host::shared_issuer(&h.secret_path).unwrap();
    let caps: std::collections::BTreeSet<String> =
        std::iter::once("mavlink.read".to_string()).collect();
    let short_lived = issuer.mint(PLUGIN_ID, &caps, 1);
    let mut client = UnixStream::connect(&sock).await.unwrap();
    send(
        &mut client,
        &request("hello", &short_lived.to_token_string()),
    )
    .await;
    let ready = recv(&mut client).await;
    assert_eq!(ready.error, None, "{ready:?}");
    assert_eq!(arg_bool(&ready, "ready"), Some(true));

    // Let it age out, then make a gated call. Before the re-mint existed this
    // answered `token_expired` and every later call did too, for the life of
    // the process.
    tokio::time::sleep(Duration::from_millis(1400)).await;
    send(
        &mut client,
        &request("ping", &short_lived.to_token_string()),
    )
    .await;

    let first = recv(&mut client).await;
    assert_eq!(
        first.kind, "event",
        "expected a token.refresh first: {first:?}"
    );
    assert_eq!(first.method, "token.refresh");
    let refreshed = arg_str(&first, "token").expect("a fresh token in the refresh event");
    let parsed = CapabilityToken::from_token_string(&refreshed).unwrap();
    assert!(issuer.verify(&parsed, parsed.issued_at + 1).is_ok());
    assert!(
        parsed.granted_caps.contains("mavlink.read"),
        "the re-mint reads the grant set off state"
    );

    let pong = recv(&mut client).await;
    assert_eq!(pong.error, None, "the request must be served, not refused");
    assert_eq!(arg_bool(&pong, "pong"), Some(true));

    // And the session keeps working on the new token with no reconnect.
    send(&mut client, &request("ping", &refreshed)).await;
    let again = recv(&mut client).await;
    assert_eq!(again.error, None, "{again:?}");
    assert_eq!(arg_bool(&again, "pong"), Some(true));
}

#[tokio::test]
async fn a_revoke_changes_the_enforcement_decision_on_the_live_session() {
    let h = harness();
    write_state(&h.state_path, PluginStatus::Running, &["mavlink.read"]);
    h.reconciler.reconcile();
    let sock = h.socket_dir.join(format!("{PLUGIN_ID}.sock"));
    let token = token_from_env(&h.socket_dir);

    let mut client = UnixStream::connect(&sock).await.unwrap();
    send(&mut client, &request("hello", &token)).await;
    assert_eq!(arg_bool(&recv(&mut client).await, "ready"), Some(true));

    // Granted: the gate lets the method through. (NoopHost answers
    // `not_implemented`, which is a *route*, not a gate refusal — the point is
    // that the capability check passed.)
    send(&mut client, &request("mavlink.subscribe", &token)).await;
    let allowed = recv(&mut client).await;
    assert!(
        !allowed
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("capability_denied"),
        "mavlink.read is granted, so the gate must pass: {allowed:?}"
    );

    // The operator revokes it: state is rewritten and the daemon re-mints.
    write_state(&h.state_path, PluginStatus::Running, &[]);
    assert!(
        h.reconciler.rotate_token(PLUGIN_ID).unwrap(),
        "a live session must receive it"
    );

    // The session picks the new token up and the SAME call is now refused —
    // with no restart of the daemon or the plugin, and without the session
    // being dropped.
    let refresh = recv(&mut client).await;
    assert_eq!(refresh.method, "token.refresh");

    send(&mut client, &request("mavlink.subscribe", &token)).await;
    let denied = recv(&mut client).await;
    let err = denied.error.clone().unwrap_or_default();
    assert!(
        err.contains("capability_denied"),
        "the revoked capability must now be refused, got {denied:?}"
    );
}

#[tokio::test]
async fn reconciling_an_already_served_plugin_does_not_churn_it() {
    // The state poll runs every two seconds forever, so an idempotent pass is
    // what stops it from dropping live connections on a timer.
    let h = harness();
    write_state(&h.state_path, PluginStatus::Running, &[]);
    assert_eq!(h.reconciler.reconcile().started, 1);
    for _ in 0..3 {
        let report = h.reconciler.reconcile();
        assert_eq!(report.started, 0);
        assert_eq!(report.stopped, 0);
        assert_eq!(report.serving, 1);
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(h.reconciler.serving(), vec![PLUGIN_ID.to_string()]);
}
