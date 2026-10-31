//! Per-plugin Unix-socket RPC server.
//!
//! One server instance binds one socket per plugin at
//! `<socket_dir>/<plugin_id>/host.sock`, accepts the plugin runner's
//! connection, runs the `hello` handshake, then loops on request envelopes:
//! re-check token expiry, gate the method on its required capability, route,
//! and reply.
//!
//! Each socket sits alone in its plugin's own directory because that directory
//! is what the plugin's unit bind-mounts (see [`crate::sandbox`]): a plugin
//! sees its own host socket and no other plugin's, and the bind follows a
//! socket the host re-creates, which a bind of the socket file itself would not.
//!
//! The wire is `ados-protocol` unchanged. Frames are length-prefixed msgpack
//! [`Envelope`]s; the token is the pipe-delimited [`CapabilityToken`] the
//! [`TokenIssuer`] mints and verifies (HMAC-SHA256 over the sorted-caps
//! payload, 600 s TTL). This server re-implements none of that; it composes it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ados_protocol::frame::{decode_len, HEADER_SIZE, PLUGIN_MAX_FRAME};
use ados_protocol::plugin::{
    CapabilityToken, Envelope, TokenIssuer, PROTOCOL_VERSION, TELEMETRY_STATE_MAX_HZ,
    TELEMETRY_STATE_TOPIC, TOKEN_REFRESH_METHOD,
};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio::task::{JoinError, JoinHandle, JoinSet};

use crate::dispatch::{gate, Gate, Method};
use crate::handlers::{self, Event, EventBus, PublishOutcome, SharedTopics};
use crate::host::{HostError, HostResult, HostServices};
use crate::invoke::{InvokeRegistry, InvokeRequest};
use crate::token_secret::TokenMint;

/// Default plugin socket directory. Part of the plugin wire contract: the
/// generated unit hands each plugin its socket path under it, so changing it
/// breaks every installed plugin.
pub const DEFAULT_SOCKET_DIR: &str = "/run/ados/plugins";

/// File name of the host socket inside a plugin's own socket directory.
pub const PLUGIN_SOCKET_NAME: &str = "host.sock";

/// A plugin's own socket directory, `<socket_dir>/<plugin_id>`: the one
/// directory its unit bind-mounts. It holds the host socket and nothing else.
pub fn plugin_socket_dir(socket_dir: &Path, plugin_id: &str) -> PathBuf {
    socket_dir.join(plugin_id)
}

/// The host socket a plugin connects to, `<socket_dir>/<plugin_id>/host.sock`.
pub fn plugin_socket_path(socket_dir: &Path, plugin_id: &str) -> PathBuf {
    plugin_socket_dir(socket_dir, plugin_id).join(PLUGIN_SOCKET_NAME)
}

/// Make `dir` a directory this process owns, mode 0755, reusing it when it
/// already is one. A plugin's unit bind-mounts this directory, so a sound one
/// is never replaced: a fresh directory would leave a running plugin bound to
/// the old one. Anything else at the path (a file, a symlink, a directory
/// another user owns) is removed first, so nothing but the host can write where
/// the socket is bound.
fn prepare_plugin_socket_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() && owned_by_this_process(&meta) => {}
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(dir)?,
        Ok(_) => std::fs::remove_file(dir)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))
}

/// Whether this process owns the file `meta` describes: its uid is the
/// effective uid. The same check on Linux and macOS, where the plugin host
/// runs as root and as the operator respectively.
fn owned_by_this_process(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.uid() == nix::unistd::geteuid().as_raw()
}

/// How long an accept loop waits after a failed `accept()` before trying again.
/// Fixed, with no cap on attempts, so a socket recovers on its own once the
/// transient condition (fd pressure) clears.
pub(crate) const ACCEPT_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// The shortest gap between two `telemetry.state` pushes to one plugin.
const TELEMETRY_STATE_MIN_INTERVAL: Duration =
    Duration::from_millis(1000 / TELEMETRY_STATE_MAX_HZ as u64);

/// Most detached host-method calls (see [`runs_detached`]) one connection may
/// have running at once. Past it, the next slow call waits for one to finish,
/// so a plugin flooding slow calls degrades to sequential instead of growing
/// the task set without bound.
const DETACHED_MAX_IN_FLIGHT: usize = 32;

/// How long a new connection has to complete its `hello`. A peer that
/// connects and stalls is dropped instead of holding a task and an fd.
const HELLO_DEADLINE: Duration = Duration::from_secs(5);

/// The largest frame accepted before the `hello` authenticates the peer. A
/// hello is a token and a few fields; nothing larger is read until it verifies.
const HELLO_MAX_FRAME: usize = 64 * 1024;

/// Most connections one plugin socket serves at once: the main process, its
/// declared services and a reconnect overlapping a teardown fit well inside
/// it. A connection past the cap is closed on accept.
pub const PLUGIN_MAX_CONNECTIONS: usize = 16;

/// The read size a frame body grows by, so a short or stalled frame never
/// costs its announced length up front.
const READ_CHUNK: usize = 64 * 1024;

/// Errors raised while running one plugin's socket server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Wall-clock unix seconds, clamped to 0 before the epoch (a freshly booted
/// SBC before NTP sync), matching the issuer's clamp.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Wall-clock unix milliseconds. Mirrors `events.now_ms`.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Live per-plugin token-refresh channels.
///
/// A rotation has to reach every *open connection*, not just the token
/// credential: each connection gates every request against the token it
/// verified at `hello`, so rewriting the file alone would leave a running
/// plugin on the old grant set until it restarted. One plugin may hold several
/// sessions at once (its main process, its declared services, a reconnect), so
/// each connection registers its own sender; [`push`](Self::push) hands a
/// freshly minted token to every one of them, and each connection swaps its
/// in-memory token and forwards a `token.refresh` event.
#[derive(Default)]
pub struct RefreshRegistry {
    inner: Mutex<HashMap<String, Vec<mpsc::Sender<CapabilityToken>>>>,
}

impl RefreshRegistry {
    pub fn new() -> Self {
        RefreshRegistry {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn register(&self, plugin_id: &str, tx: mpsc::Sender<CapabilityToken>) {
        if let Ok(mut map) = self.inner.lock() {
            map.entry(plugin_id.to_string()).or_default().push(tx);
        }
    }

    /// Drop this session's sender, identity-checked so another live session of
    /// the same plugin keeps its own.
    fn unregister_if(&self, plugin_id: &str, tx: &mpsc::Sender<CapabilityToken>) {
        if let Ok(mut map) = self.inner.lock() {
            if let Some(senders) = map.get_mut(plugin_id) {
                senders.retain(|held| !held.same_channel(tx));
                if senders.is_empty() {
                    map.remove(plugin_id);
                }
            }
        }
    }

    /// Hand a token to every live session of the plugin. `false` when none
    /// took it (nothing open, or every queue full); the credential the caller
    /// already wrote is what the plugin reads when it next connects.
    pub fn push(&self, plugin_id: &str, token: CapabilityToken) -> bool {
        let senders = match self.inner.lock() {
            Ok(map) => map.get(plugin_id).cloned().unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        let mut delivered = false;
        for tx in senders {
            delivered |= tx.try_send(token.clone()).is_ok();
        }
        delivered
    }
}

/// A plugin RPC server bound to one socket. Holds the token issuer and the
/// event bus shared with the rest of the host, plus the host-service facade the
/// dispatcher routes host-coupled methods through.
pub struct PluginIpcServer<H: HostServices> {
    socket_dir: PathBuf,
    token_issuer: Arc<TokenIssuer>,
    bus: Arc<EventBus>,
    host: Arc<H>,
    /// The host-to-plugin invoke seam: each connection registers its outbound
    /// sender here, and the control socket reaches a plugin's tools through it.
    invoke: Arc<InvokeRegistry>,
    /// Live token-refresh channels, one per open session.
    refresh: Arc<RefreshRegistry>,
    /// Mints a plugin's current token from state. Present in the wired daemon;
    /// `None` in a bare server (tests, the in-process smoke path), where an
    /// expired token stays expired.
    mint: Option<Arc<TokenMint>>,
    /// The shared topics of the served plugins, which the event gates read.
    shared_topics: Arc<SharedTopics>,
}

impl<H: HostServices> PluginIpcServer<H> {
    pub fn new(
        socket_dir: impl AsRef<Path>,
        token_issuer: Arc<TokenIssuer>,
        bus: Arc<EventBus>,
        host: Arc<H>,
    ) -> Self {
        Self {
            socket_dir: socket_dir.as_ref().to_path_buf(),
            token_issuer,
            bus,
            host,
            invoke: Arc::new(InvokeRegistry::new()),
            refresh: Arc::new(RefreshRegistry::new()),
            mint: None,
            shared_topics: Arc::new(SharedTopics::default()),
        }
    }

    /// The shared-topic registry the event gates read, so the reconciler can
    /// keep it equal to the served plugins' declarations.
    pub fn shared_topics(&self) -> Arc<SharedTopics> {
        self.shared_topics.clone()
    }

    /// Attach the token mint so an expired token is re-minted in place rather
    /// than failing the request.
    ///
    /// Without it the server answers `token_expired` and the plugin has no
    /// recovery path — the whole LIFE-TOKEN-TTL failure. With it, a request
    /// arriving on an aged-out token triggers a mint from the *current* grant
    /// set, the connection adopts the new token, the plugin is told, and the
    /// request is re-gated. A revoked capability is still refused, because the
    /// re-mint reads state.
    pub fn with_token_mint(mut self, mint: Arc<TokenMint>) -> Self {
        self.mint = Some(mint);
        self
    }

    /// The shared invoke registry the control socket forwards a `tool.invoke`
    /// into. Hand this to [`crate::control::serve_control`] so an operator's
    /// `POST /api/plugins/{id}/tools/{tool}/invoke` reaches a live plugin.
    pub fn invoke_registry(&self) -> Arc<InvokeRegistry> {
        self.invoke.clone()
    }

    /// The shared refresh registry, so the reconciler can push a rotated token
    /// into a live session.
    pub fn refresh_registry(&self) -> Arc<RefreshRegistry> {
        self.refresh.clone()
    }

    /// The socket path for a plugin id.
    pub fn socket_path(&self, plugin_id: &str) -> PathBuf {
        plugin_socket_path(&self.socket_dir, plugin_id)
    }

    /// Bind the per-plugin socket and spawn the accept loop. Returns the bound path and
    /// a handle to the accept task.
    pub fn serve_plugin(&self, plugin_id: &str) -> Result<(PathBuf, JoinHandle<()>), ServerError> {
        // The plugin's own directory first (the one its unit bind-mounts), then
        // the shared helper's remove-stale / bind / chmod hygiene inside it.
        // 0o660 and the plugin group let the plugin's unit (its own user, in
        // that group) connect; every request is then gated on its token.
        prepare_plugin_socket_dir(&plugin_socket_dir(&self.socket_dir, plugin_id))?;
        let path = self.socket_path(plugin_id);
        let listener = ados_protocol::ipc::bind_plugin_socket(&path, 0o660)?;

        let plugin_id = plugin_id.to_string();
        let token_issuer = self.token_issuer.clone();
        let bus = self.bus.clone();
        let host = self.host.clone();
        let socket_dir = self.socket_dir.clone();
        let invoke = self.invoke.clone();
        let refresh = self.refresh.clone();
        let mint = self.mint.clone();
        let shared_topics = self.shared_topics.clone();
        let permits = Arc::new(tokio::sync::Semaphore::new(PLUGIN_MAX_CONNECTIONS));
        let task = tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((s, _addr)) => s,
                    Err(e) => {
                        // A transient accept error (fd pressure) must not leave
                        // the plugin refused until the daemon restarts.
                        tracing::warn!(plugin_id = %plugin_id, error = %e, "plugin socket accept failed");
                        tokio::time::sleep(ACCEPT_RETRY_INTERVAL).await;
                        continue;
                    }
                };
                // A connection past the cap is closed at once: the socket's
                // group could otherwise hold unlimited idle connections.
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    tracing::warn!(
                        plugin_id = %plugin_id,
                        limit = PLUGIN_MAX_CONNECTIONS,
                        "plugin socket connection limit reached; closing the new connection"
                    );
                    drop(stream);
                    continue;
                };
                let mut conn = Connection {
                    plugin_id: plugin_id.clone(),
                    session: 0,
                    token_issuer: token_issuer.clone(),
                    bus: bus.clone(),
                    host: host.clone(),
                    socket_dir: socket_dir.clone(),
                    invoke: invoke.clone(),
                    refresh: refresh.clone(),
                    state_seen: std::sync::Mutex::new(mint.as_ref().and_then(|m| m.state_mtime())),
                    mint: mint.clone(),
                    shared_topics: shared_topics.clone(),
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    let handshake = match tokio::time::timeout(
                        HELLO_DEADLINE,
                        conn.handshake(stream),
                    )
                    .await
                    {
                        Ok(Ok(Some(done))) => done,
                        Ok(Ok(None)) => return,
                        Ok(Err(err)) => {
                            tracing::warn!(plugin_id = %conn.plugin_id, error = %err, "plugin handshake failed");
                            return;
                        }
                        Err(_) => {
                            tracing::warn!(plugin_id = %conn.plugin_id, "plugin handshake timed out");
                            return;
                        }
                    };
                    // A session begins only once the peer has authenticated,
                    // so a connection that never says hello can neither take
                    // nor release anything another session holds.
                    let session = conn.host.begin_session(&conn.plugin_id);
                    conn.session = session;
                    let (token, read_half, write_half) = handshake;
                    if let Err(err) = conn.serve(token, read_half, write_half).await {
                        tracing::warn!(
                            plugin_id = %conn.plugin_id,
                            error = %err,
                            "plugin connection ended with an error"
                        );
                    }
                    conn.host.release_session(&conn.plugin_id, session).await;
                });
            }
        });
        Ok((path, task))
    }

    /// Stop serving a plugin: remove its bound socket file. The caller owns the
    /// accept [`JoinHandle`] returned by [`serve_plugin`] and aborts it; this
    /// only unlinks the socket so a later re-serve binds cleanly and no stale
    /// socket lingers. A live connection's `release_session` already runs on
    /// disconnect (see [`serve_plugin`]); aborting the accept task stops new
    /// connections. The host drops whatever it holds for the plugin as a whole
    /// (its display page, its registered vision models).
    pub fn stop_plugin(&self, plugin_id: &str) {
        let path = self.socket_path(plugin_id);
        let _ = std::fs::remove_file(&path);
        // Drop the plugin's published-state sidecar too, so a stopped plugin
        // does not leave a stale state file the front would keep serving.
        crate::state_sidecar::remove(&self.socket_dir, plugin_id);
        let host = self.host.clone();
        let plugin_id = plugin_id.to_string();
        tokio::spawn(async move { host.release_plugin(&plugin_id).await });
    }
}

/// One accepted connection from a plugin runner.
struct Connection<H: HostServices> {
    plugin_id: String,
    /// This connection's session, from [`HostServices::begin_session`], taken
    /// once its `hello` verified. The host methods that hold per-connection
    /// state (an mDNS record, the aux stream, the display page) key it on
    /// this, since one plugin may hold several connections at once.
    session: u64,
    token_issuer: Arc<TokenIssuer>,
    bus: Arc<EventBus>,
    host: Arc<H>,
    /// The per-plugin socket directory, used to locate the published-state
    /// sidecar written on each authorized publish.
    socket_dir: PathBuf,
    /// The shared invoke registry: this connection registers its outbound
    /// sender here while its session is up so the control socket can reach it.
    invoke: Arc<InvokeRegistry>,
    /// The shared refresh registry: this connection registers a token sender
    /// here so a rotation reaches the live session, not just the credential.
    refresh: Arc<RefreshRegistry>,
    /// Mints this plugin's current token from state, for the on-expiry re-mint
    /// and the re-mint after the grant state file changes.
    mint: Option<Arc<TokenMint>>,
    /// The grant state file's mtime when this session last read it. A request
    /// that finds it changed re-mints first, so a revoke applies on the next
    /// request rather than at token expiry.
    state_seen: std::sync::Mutex<Option<std::time::SystemTime>>,
    /// The shared topics of the served plugins.
    shared_topics: Arc<SharedTopics>,
}

mod delivery;
mod dispatch;
mod handshake;
mod serve;
mod subscribe;
mod wire;

use delivery::*;
use dispatch::*;
use wire::*;

#[cfg(test)]
mod tests;
