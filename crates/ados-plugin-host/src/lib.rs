//! Plugin RPC host: the server, dispatch gate, and handler routing for the
//! ADOS Drone Agent's plugin sandbox.
//!
//! The host binds one Unix domain socket per plugin, accepts the plugin
//! runner's connection, verifies a per-process capability token at the `hello`
//! handshake, then gates every request on its required capability before
//! routing. The wire — length-prefixed msgpack envelopes and the pipe-delimited
//! HMAC capability token — lives in the `ados-protocol` crate, which is
//! byte-parity tested against the Python supervisor. This crate composes that
//! wire; it re-implements none of it.
//!
//! Scope: the server, the dispatch table, the capability gate, the in-process
//! event bus, and the host-service facade. The full event/ping surface is
//! wired; the 17 host-coupled methods route to a [`host::HostServices`] trait
//! whose default [`host::NoopHost`] returns the `not_implemented` shape,
//! mirroring the Python `_handle_*` stub bodies until the agent's service
//! surfaces expose stable hooks (the real wiring is [`realhost::RealHost`]).
//! Plugin lifecycle (install / enable / disable / remove / archive / signing /
//! state) lives here too, in the modules below; the plugin SDK ships as the
//! separate `ados-sdk` crate.
//!
//! Modules:
//! - [`dispatch`] — the `method -> (handler, required_cap)` table and the gate
//!   producing the exact wire error strings.
//! - [`handlers`] — the in-process event bus, topic matching, the per-topic
//!   publish/subscribe checks, and host-method routing.
//! - [`host`] — the [`host::HostServices`] facade trait and [`host::NoopHost`].
//! - [`server`] — the per-plugin socket server: handshake, dispatch loop, and
//!   event push path.
//!
//! Lifecycle modules (install / enable / disable / remove):
//! - [`manifest`] — the `manifest.yaml` model the controller reads.
//! - [`archive`] — the `.adosplug` reader and the canonical payload hash.
//! - [`signing`] — Ed25519 verify, trusted-keys store, revocation list, and the
//!   hardcoded first-party allowlist.
//! - [`state`] — the on-disk install state, atomic write + advisory lock, and
//!   the permission-against-manifest filter.
//! - [`systemd`] — the per-plugin unit + slice string builders.
//! - [`supervisor`] — the lifecycle controller tying the above together.
//! - [`errors`] — the lifecycle error hierarchy.

pub mod archive;
pub mod dispatch;
pub mod errors;
pub mod handlers;
pub mod host;
pub mod manifest;
pub mod mavlink_client;
pub mod realhost;
pub mod server;
pub mod signing;
pub mod state;
pub mod supervisor;
pub mod systemd;
pub mod token_secret;
pub mod vision_client;

pub use dispatch::{gate, Gate, Method};
pub use errors::{
    ArchiveError, LifecycleError, ManifestError, SignatureError, SignatureErrorKind,
    SupervisorError,
};
pub use handlers::{Event, EventBus};
pub use host::{HostResult, HostServices, NoopHost};
pub use manifest::{AgentRuntime, PluginManifest};
pub use server::{PluginIpcServer, ServerError, DEFAULT_SOCKET_DIR};
pub use signing::{is_first_party_signer, FIRST_PARTY_SIGNERS};
pub use state::{PluginInstall, PluginSource, PluginStatus};
pub use supervisor::{semver_in_range, InstallResult, Paths, PluginSupervisor, SystemctlRunner};
pub use token_secret::{
    load_or_create_secret, shared_issuer, token_env_path, write_token_env,
    PLUGIN_TOKEN_SECRET_PATH,
};
pub use vision_client::{VisionClient, VisionRpcError};
