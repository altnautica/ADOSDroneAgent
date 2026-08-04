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
pub mod button_client;
pub mod control;
pub mod dispatch;
pub mod errors;
pub mod handlers;
pub mod host;
pub mod invoke;
pub mod manifest;
pub mod mavlink_client;
pub mod msp_client;
pub mod realhost;
pub mod server;
pub mod signing;
pub mod state;
pub mod state_sidecar;
pub mod supervisor;
pub mod systemd;
pub mod token_secret;
pub mod vision_client;

pub use control::{control_socket_path, serve_control, ConfigControl, CONTROL_SOCKET_NAME};
pub use dispatch::{gate, Gate, Method};
pub use errors::{
    ArchiveError, LifecycleError, ManifestError, SignatureError, SignatureErrorKind,
    SupervisorError,
};
pub use handlers::{Event, EventBus};
pub use host::{HostResult, HostServices, NoopHost};
pub use invoke::{InvokeRegistry, InvokeRequest, DEFAULT_INVOKE_TIMEOUT};
pub use manifest::{AgentRuntime, PluginManifest};
pub use server::{PluginIpcServer, ServerError, DEFAULT_SOCKET_DIR};
pub use signing::{is_first_party_signer, FIRST_PARTY_SIGNERS};
pub use state::{PluginInstall, PluginSource, PluginStatus};
pub use supervisor::{semver_in_range, InstallResult, Paths, PluginSupervisor, SystemctlRunner};
pub use token_secret::{
    load_or_create_secret, shared_issuer, token_env_path, write_token_env, PLUGIN_TOKEN_SECRET_PATH,
};
pub use vision_client::{VisionClient, VisionRpcError};

/// Capabilities gated inside a handler rather than by the generated dispatch
/// table.
///
/// The dispatch table answers "may this plugin call this method at all", which
/// is the whole gate for most capabilities. A few are finer than a method: the
/// event bus checks the capability against the topic being published or
/// subscribed, and a MAVLink send checks the message id against the pose and
/// visual-odometry capabilities, because the method itself is broader than the
/// permission. Those checks are real gates and belong in the same picture, so
/// they are named here rather than being invisible to anything that asks what
/// is enforced.
///
/// Kept in sync by `every_gated_capability_is_declared_enforced` below, which
/// fails if this list names something no handler actually checks.
pub const HANDLER_GATED_CAPS: &[&str] = &[
    "estimator.pose.inject",
    "event.publish",
    "event.subscribe",
    "mavlink.component.vio",
    "mcp.expose",
];

#[cfg(test)]
mod capability_enforcement_guard {
    use super::HANDLER_GATED_CAPS;
    use std::collections::BTreeSet;

    /// Every capability the agent actually gates at runtime, from both places a
    /// gate can live.
    fn actually_gated() -> BTreeSet<String> {
        let mut set: BTreeSet<String> = ados_protocol::dispatch::DISPATCH_METHODS
            .iter()
            .filter_map(|m| m.required_cap)
            .map(str::to_string)
            .collect();
        set.extend(HANDLER_GATED_CAPS.iter().map(|c| c.to_string()));
        // Only agent capabilities are described by that catalog.
        set.retain(|c| ados_protocol::capabilities::get_agent_capability(c).is_some());
        set
    }

    #[test]
    fn every_gated_capability_is_declared_enforced() {
        // The catalog's `enforced` flag is read by the install-time consent
        // surface and by anything reasoning about what a plugin can do. It had
        // drifted to claim fifteen fewer gates than the code performs, which is
        // metadata that understates the protection actually in place -- the
        // safe direction, but still a surface saying something untrue about
        // itself, and one a reader would use to decide what to audit.
        let under: Vec<String> = actually_gated()
            .into_iter()
            .filter(|c| {
                ados_protocol::capabilities::get_agent_capability(c).is_some_and(|m| !m.enforced)
            })
            .collect();
        assert!(
            under.is_empty(),
            "these capabilities are gated at runtime but declared unenforced: {under:?}"
        );
    }

    #[test]
    fn nothing_claims_a_gate_it_does_not_have() {
        // The dangerous direction. A capability advertised as enforced that no
        // code checks would let an operator grant it believing something stands
        // behind it.
        let gated = actually_gated();
        let over: Vec<&str> = ados_protocol::capabilities::AGENT_CAPABILITIES
            .iter()
            .filter(|m| m.enforced && !gated.contains(m.id))
            .map(|m| m.id)
            .collect();
        assert!(
            over.is_empty(),
            "these capabilities claim a runtime gate that does not exist: {over:?}"
        );
    }

    #[test]
    fn every_named_handler_gate_is_a_real_capability() {
        for cap in HANDLER_GATED_CAPS {
            assert!(
                ados_protocol::capabilities::get_agent_capability(cap).is_some(),
                "{cap} is named as handler-gated but is not a declared capability"
            );
        }
    }
}
