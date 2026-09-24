//! The real [`HostServices`] implementation.
//!
//! This file holds the [`RealHost`] struct, its builders and the module
//! wiring. The handlers are in `host_services`; what they call is split by
//! responsibility: `mavlink_gate` (outbound frame classification and the
//! component gate), `facades` (component registrar), `config_store` and
//! `config_control` (plugin config and its persistence), `forwards`
//! (command-socket services), `setpoint` (flight request parsing), `offload`
//! (perception sessions), `display` (the reserved plugin page), `node_info`
//! (the node facts), `caps` (the ungrantable capability set) and `args`
//! (msgpack readers). The argument
//! validation, the inline capability gates, the error strings and the
//! success-map shapes are all part of the wire contract a plugin is written
//! against, so none of them may drift without a matching SDK change.
//!
//! Raise-vs-return mapping (read carefully — it is load-bearing):
//! * A Python `raise _RpcError(m)` becomes `Err(HostError::Rpc(m))`.
//! * A Python `raise CapabilityDenied(pid, cap)` becomes
//!   `Err(HostError::CapabilityDenied(cap))`.
//! * A Python `raise AllowlistViolation(pid, basename)` becomes
//!   `Err(HostError::AllowlistViolation(basename))`.
//! * A Python handler that *returns* a dict with an `"error"` key (the
//!   `not_available` / `send_failed` paths) becomes `Ok(<that map>)`, NOT an
//!   `Err`. Those are graceful-degrade responses, not gate failures.
//!
//! One [`Arc<RealHost>`] is shared across every per-plugin accept task, so every
//! facade is behind a [`std::sync::Mutex`]. Every lock is taken and released
//! with no `.await` in between, so a std mutex is correct: the async methods
//! (command-socket forwards, config persistence, offload) await only after
//! their guard is dropped.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rmpv::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, Notify};
use tokio::task::JoinHandle;

use ados_compute::{
    run_offload_orchestrator, ComputeClient, ComputeJobKind, NodeEndpoint, OrchestratorConfig,
};
use ados_protocol::node_credential::WorkstationCredentials;
use ados_protocol::offload_link::{read_offload_link_from, OFFLOAD_LINK_SIDECAR};

use crate::button_client::RECONNECT_INTERVAL;
use crate::frame_link::{FrameLink, SendError};
use crate::host::{not_implemented, HostError, HostResult, HostServices};
use crate::vehicle_events::FcIdentity;
use crate::vision_client::VisionClient;

mod advertise;
mod caps;
mod cloud;
mod config_control;
mod config_store;
mod convert;
mod display;
mod facades;
mod forwards;
mod host_services;
mod mavlink_gate;
mod node_info;
mod offload;
mod setpoint;
mod telemetry;

#[cfg(test)]
use self::caps::*;
use self::config_control::*;
use self::config_store::*;
use self::convert::*;
use self::display::*;
use self::facades::*;
use self::forwards::*;
use self::mavlink_gate::*;
use self::offload::*;
use self::setpoint::*;
use self::telemetry::*;
use crate::args::*;

pub use self::caps::{write_ungrantable_caps, UNGRANTABLE_CAPS_SIDECAR};
pub use self::config_store::{
    CONFIG_FILE_MAX_BYTES, CONFIG_PLUGIN_MAX_BYTES, CONFIG_PLUGIN_MAX_KEYS, CONFIG_VALUE_MAX_BYTES,
};
pub(crate) use self::mavlink_gate::mavlink_msg_id;
pub use self::mavlink_gate::{POSE_INJECT_MSG_IDS, VIO_COMPONENT_IDS};
pub use self::node_info::NodeInfoSources;

// ---------------------------------------------------------------------
// RealHost
// ---------------------------------------------------------------------

/// Resolves a plugin id to its `(install_dir, subprocess_spawn allowlist)`.
pub type RuntimeLookup = Box<dyn Fn(&str) -> Option<(PathBuf, BTreeSet<String>)> + Send + Sync>;

/// [`RuntimeLookup`] as the host holds it: shared, so a lookup (which reads the
/// plugin's manifest) can run on the blocking pool.
type SharedRuntimeLookup = Arc<dyn Fn(&str) -> Option<(PathBuf, BTreeSet<String>)> + Send + Sync>;

/// Resolves a plugin id to its bound agent identity (empty when unbound).
pub type AgentIdLookup = Box<dyn Fn(&str) -> String + Send + Sync>;

/// The process-wide button-bus reader, started on first `button.subscribe`.
/// The bus is one socket, so one connection serves every subscriber.
static BUTTON_CLIENT: std::sync::OnceLock<crate::button_client::ButtonClient> =
    std::sync::OnceLock::new();

/// The real host: the in-memory facades, the router links, the service
/// command-socket paths and the runtime lookups every handler reads.
pub struct RealHost {
    components: Mutex<ComponentRegistrar>,
    /// The live connection session per plugin (see `begin_session`).
    sessions: Mutex<HashMap<String, u64>>,
    session_seq: AtomicU64,
    config: Mutex<ConfigStore>,
    /// The generation of the config snapshot last written to disk. Its lock
    /// also serializes the writers, so an older snapshot never lands after a
    /// newer one.
    config_written: Arc<Mutex<u64>>,
    /// Each plugin's declared parameter schemas, compiled once per manifest
    /// revision.
    param_schemas: Arc<Mutex<HashMap<String, Arc<ParamSchemas>>>>,
    /// The MAVLink router link. `None` only on a host built without one (tests);
    /// the daemon always wires it, and a router that is down shows up as
    /// `sent: false` with reason `disconnected`, not as a missing slot.
    mavlink: Option<Arc<FrameLink>>,
    /// The MSP router link (Betaflight / iNav / KISS FC), wired like the
    /// MAVLink link.
    msp: Option<Arc<FrameLink>>,
    vision: Option<Arc<VisionClient>>,
    /// The paired compute node's offload client. `None` until the supervisor
    /// wires a discovered/paired node; the `compute_*` methods return
    /// `not_implemented` while unwired (the MAVLink/vision not-available posture).
    compute: Option<Arc<ComputeClient>>,
    plugin_runtime_lookup: Option<SharedRuntimeLookup>,
    agent_id_lookup: Option<AgentIdLookup>,
    /// Sidecar path the reserved display page reads its content from. The
    /// canonical `/run/ados/lcd-plugin-page.json`; a builder overrides it in
    /// tests so the write round-trips without touching `/run`.
    display_page_path: PathBuf,
    /// Command socket the GPIO-output service serves. The host forwards each
    /// `gpio.*` method to it (the radio/wifi-cmd-socket precedent); a builder
    /// overrides it in tests so the forward round-trips against a stub.
    gpio_cmd_path: PathBuf,
    /// Command socket the radio service serves for the auxiliary application
    /// stream. The host forwards each `radio.aux_stream.*` method to it (the same
    /// command-socket precedent); a builder overrides it in tests.
    radio_aux_cmd_path: PathBuf,
    /// Command socket the supervisor serves for video-source reconfiguration. The
    /// host forwards `video.source.set` to it (the same command-socket precedent);
    /// the supervisor persists the source list + restarts the video service. A
    /// builder overrides it in tests.
    video_cmd_path: PathBuf,
    /// The plugin id that currently holds the auxiliary stream open, or `None`
    /// when the stream is closed. The aux pair is a single shared resource on the
    /// one adapter, so at most one plugin owns it at a time. Used to close the
    /// stream automatically when its owner disconnects (the SAFE-by-default
    /// invariant: a stream never outlives the plugin that opened it).
    aux_stream_owner: Mutex<Option<String>>,
    /// The process-global aux-subscribe reader, started lazily on the first
    /// `radio.aux_stream.subscribe` and shared by every subscribing plugin. One
    /// connection to the radio service's aux command socket feeds this broadcast;
    /// mirrors the [`BUTTON_CLIENT`] single-connection philosophy. Initialized
    /// per-instance (rather than `static`) so a test host with an overridden
    /// `radio_aux_cmd_path` round-trips against its stub.
    aux_reader: std::sync::OnceLock<tokio::sync::broadcast::Sender<(u8, Vec<u8>)>>,
    /// The MAVLink service's vehicle-state socket `telemetry.subscribe` reads
    /// (the canonical `/run/ados/state.sock`; the daemon passes its run dir's,
    /// a builder overrides it in tests).
    vehicle_state_sock: PathBuf,
    /// The process-global vehicle-state reader, started lazily on the first
    /// `telemetry.subscribe` and shared by every subscribing plugin, like
    /// [`aux_reader`](Self::aux_reader): one connection to the state socket
    /// however many plugins read it.
    state_reader: std::sync::OnceLock<broadcast::Sender<Arc<Value>>>,
    /// Live streaming perception-offload sessions a plugin opened, keyed by
    /// session id. Each holds its cancel handle + orchestrator task + the node
    /// reach for health reads. A session is closed on an explicit
    /// `compute.stream.close` or when its opener disconnects (SAFE-by-default: a
    /// session never outlives the plugin that opened it).
    offload_streams: Mutex<HashMap<String, OffloadStreamHandle>>,
    /// Monotonic counter minting a unique session id when the plugin omits one.
    offload_session_seq: AtomicU64,
    /// The store of credentials workstations issued this drone, from which an
    /// offload session presents the one its node issued (the canonical
    /// `/etc/ados/workstation-credentials.json`; a builder overrides it in tests).
    workstation_credentials_path: PathBuf,
    /// The offload-link sidecar the perception-tier decision is read from (the
    /// canonical `/run/ados/offload-link.json`; a builder overrides it in tests).
    /// Reusing the sidecar keeps the tier decision one source of truth,
    /// not a second copy of `ados_offload::pick_tier`.
    offload_link_path: PathBuf,
    /// The plugin-state file `vision.read_model` reads a plugin's resolved
    /// `model_status` from (the canonical `/var/ados/state/plugin-state.json`,
    /// written by the Python resolver; a builder overrides it in tests). Read
    /// per call so a fresh resolution (e.g. an operator sideload flipping
    /// `needs_model`→`resolved`) is picked up without a restart.
    state_path: PathBuf,
    /// The cloud relay's local publish socket `cloud.publish` and
    /// `cloud.records.put` forward to (`<run dir>/cloud-publish.sock`; a
    /// builder overrides it in tests).
    cloud_publish_path: PathBuf,
    /// The autopilot's MAVLink identity, recorded from its heartbeats on the
    /// router link. Commands a plugin sends without a target are addressed to
    /// it.
    fc_identity: Arc<FcIdentity>,
    /// Where `node.info` reads the node facts from (the production sources
    /// resolved from the environment; a builder overrides them in tests).
    node_info_sources: NodeInfoSources,
}

impl RealHost {
    /// A host with empty facades and every external slot unwired, matching
    /// `default_host_services()`.
    pub fn new() -> Self {
        Self {
            components: Mutex::new(ComponentRegistrar::default()),
            sessions: Mutex::new(HashMap::new()),
            session_seq: AtomicU64::new(0),
            config: Mutex::new(ConfigStore::default()),
            config_written: Arc::new(Mutex::new(0)),
            param_schemas: Arc::new(Mutex::new(HashMap::new())),
            mavlink: None,
            msp: None,
            vision: None,
            compute: None,
            plugin_runtime_lookup: None,
            agent_id_lookup: None,
            display_page_path: PathBuf::from(LCD_PLUGIN_PAGE_PATH),
            gpio_cmd_path: PathBuf::from(GPIO_CMD_SOCK),
            radio_aux_cmd_path: PathBuf::from(RADIO_AUX_CMD_SOCK),
            video_cmd_path: PathBuf::from(VIDEO_CMD_SOCK),
            aux_stream_owner: Mutex::new(None),
            aux_reader: std::sync::OnceLock::new(),
            vehicle_state_sock: PathBuf::from(VEHICLE_STATE_SOCK),
            state_reader: std::sync::OnceLock::new(),
            offload_streams: Mutex::new(HashMap::new()),
            offload_session_seq: AtomicU64::new(0),
            workstation_credentials_path: WorkstationCredentials::default_path(),
            offload_link_path: PathBuf::from(OFFLOAD_LINK_SIDECAR),
            state_path: PathBuf::from(crate::state::PLUGIN_STATE_PATH),
            cloud_publish_path: ados_protocol::cloud_publish::socket_path(),
            fc_identity: Arc::new(FcIdentity::default()),
            node_info_sources: NodeInfoSources::from_env(),
        }
    }

    /// Override the plugin-state path `vision.read_model` reads (builder style,
    /// tests). Production uses the canonical `/var/ados/state/plugin-state.json`.
    pub fn with_state_path(mut self, path: PathBuf) -> Self {
        self.state_path = path;
        self
    }

    /// Override the cloud relay publish socket (builder style, tests).
    pub fn with_cloud_publish_path(mut self, path: PathBuf) -> Self {
        self.cloud_publish_path = path;
        self
    }

    /// Override the vehicle-state socket `telemetry.subscribe` reads (builder
    /// style; the daemon passes its run dir's, tests a stub's).
    pub fn with_vehicle_state_socket(mut self, path: PathBuf) -> Self {
        self.vehicle_state_sock = path;
        self
    }

    /// Override the workstation-credential store path (builder style, tests).
    /// Production uses the canonical path from [`Self::new`].
    pub fn with_workstation_credentials_path(mut self, path: PathBuf) -> Self {
        self.workstation_credentials_path = path;
        self
    }

    /// Override the offload-link sidecar path (builder style, tests). Production
    /// uses the canonical `/run/ados/offload-link.json` from [`Self::new`].
    pub fn with_offload_link_path(mut self, path: PathBuf) -> Self {
        self.offload_link_path = path;
        self
    }

    /// Override where `node.info` reads the node facts (builder style, tests).
    /// Production resolves them from the environment in [`Self::new`].
    pub fn with_node_info_sources(mut self, sources: NodeInfoSources) -> Self {
        self.node_info_sources = sources;
        self
    }

    /// Override the display-page sidecar path (builder style, tests). Production
    /// uses the canonical `/run/ados/lcd-plugin-page.json` from [`Self::new`].
    pub fn with_display_page_path(mut self, path: PathBuf) -> Self {
        self.display_page_path = path;
        self
    }

    /// Override the GPIO command socket path (builder style, tests). Production
    /// uses the canonical `/run/ados/gpio-cmd.sock` from [`Self::new`].
    pub fn with_gpio_cmd_path(mut self, path: PathBuf) -> Self {
        self.gpio_cmd_path = path;
        self
    }

    /// Override the radio auxiliary-stream command socket path (builder style,
    /// tests). Production uses the canonical `/run/ados/radio-aux.sock` from
    /// [`Self::new`].
    pub fn with_radio_aux_cmd_path(mut self, path: PathBuf) -> Self {
        self.radio_aux_cmd_path = path;
        self
    }

    /// Override the supervisor video command socket path (builder style, tests).
    /// Production uses the canonical `/run/ados/video-cmd.sock` from [`Self::new`].
    pub fn with_video_cmd_path(mut self, path: PathBuf) -> Self {
        self.video_cmd_path = path;
        self
    }

    /// Wire the MAVLink router link (builder style).
    pub fn with_mavlink(mut self, mavlink: Arc<FrameLink>) -> Self {
        self.mavlink = Some(mavlink);
        self
    }

    /// Wire the MSP router link (builder style). When wired, `msp.send`
    /// forwards raw bytes to the FC and `msp_subscribe_stream` hands out the
    /// FC->host fanout; when unwired both return the not-available posture.
    pub fn with_msp(mut self, msp: Arc<FrameLink>) -> Self {
        self.msp = Some(msp);
        self
    }

    /// Wire the vision-engine client (builder style). When wired, the three
    /// vision request methods proxy to the engine over its socket and
    /// `vision_subscribe_stream` hands out the engine's frame-descriptor
    /// fanout, mirroring the MAVLink wiring. When unwired the methods return the
    /// `not_implemented` shape and the stream is `None`, matching the
    /// MAVLink not-available posture.
    pub fn with_vision(mut self, vision: Arc<VisionClient>) -> Self {
        self.vision = Some(vision);
        self
    }

    /// Wire the paired compute node's offload client (builder style). The
    /// supervisor calls this once it has a node base url and key (from discovery
    /// and LAN pairing). When unwired the `compute_*` methods return the
    /// `not_implemented` shape, matching the MAVLink/vision not-available posture.
    pub fn with_compute(mut self, compute: Arc<ComputeClient>) -> Self {
        self.compute = Some(compute);
        self
    }

    /// Wire the plugin runtime lookup (builder style).
    pub fn with_runtime_lookup(mut self, lookup: RuntimeLookup) -> Self {
        self.plugin_runtime_lookup = Some(Arc::from(lookup));
        self
    }

    /// Wire the agent-id lookup (builder style).
    pub fn with_agent_id_lookup(mut self, lookup: AgentIdLookup) -> Self {
        self.agent_id_lookup = Some(lookup);
        self
    }

    /// Persist plugin config to a 0600 JSON file (builder style). Loads any
    /// existing records so config survives a plugin-host restart, then writes
    /// the whole store on every `config.set`. Without this the store is
    /// in-memory only (config is lost on restart).
    pub fn with_config_persistence(mut self, path: PathBuf) -> Self {
        self.config = Mutex::new(ConfigStore::load(path));
        self
    }

    /// Resolve the agent id for a plugin, swallowing lookup errors to "" exactly
    /// as `_agent_id_for`. (The Rust lookup cannot raise, so a `None` host slot
    /// is the only "" path; an empty string returned by the closure stays "".)
    fn agent_id_for(&self, plugin_id: &str) -> String {
        match &self.agent_id_lookup {
            Some(lookup) => lookup(plugin_id),
            None => String::new(),
        }
    }

    /// The shared autopilot identity cell. The daemon hands it to the router
    /// link reader, which records every autopilot heartbeat into it.
    pub fn fc_identity(&self) -> Arc<FcIdentity> {
        Arc::clone(&self.fc_identity)
    }

    /// The plugin's live connection session, or 0 before its first.
    fn current_session(&self, plugin_id: &str) -> u64 {
        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .get(plugin_id)
            .copied()
            .unwrap_or(0)
    }
}

impl Default for RealHost {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
