//! The slice of `/etc/ados/config.yaml` the pairing-info route projects.
//!
//! The FastAPI pairing-info endpoint reads its identity fields off the loaded
//! agent config: the device id, the human name, the profile, and the radio
//! peer's device id. This module reads the same on-disk YAML directly, typing
//! only the fields the route needs and tolerating every other section, the same
//! pattern the cloud relay's config reader uses. A config the Python agent wrote
//! reads identically here because the field names + defaults are mirrored.
//!
//! The reader is total: a missing or unparseable file yields the all-defaults
//! snapshot rather than an error, so the guaranteed-200 pairing-info contract
//! holds on a fresh, partially-configured agent.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;

/// Canonical config location, overridable via the `ADOS_CONFIG` env var the
/// systemd unit sets — the same convention the sibling crates use.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";

/// The identity file for the node whose config is at `config`:
/// `$ADOS_DEVICE_ID_PATH` when set, else `device-id` beside the config, which
/// for the canonical config is `/etc/ados/device-id`. Deriving it from the
/// config path keeps a reader pointed at a test config off the host's file.
pub fn device_id_file_for(config: &Path) -> PathBuf {
    std::env::var(ados_protocol::identity::ENV_DEVICE_ID_PATH)
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| config.with_file_name("device-id"))
}

/// The `agent:` section: the device identity the pairing-info route reports.
/// Defaults mirror the Python `AgentConfig` field defaults so an absent section
/// reads the same value the loaded Python config would (`name: "my-drone"`,
/// `profile: "auto"`).
#[derive(Debug, Clone, Deserialize)]
pub struct AgentSection {
    #[serde(default)]
    pub device_id: String,
    #[serde(default = "default_agent_name")]
    pub name: String,
    #[serde(default = "default_agent_profile")]
    pub profile: String,
}

fn default_agent_name() -> String {
    "my-drone".to_string()
}

fn default_agent_profile() -> String {
    "auto".to_string()
}

impl Default for AgentSection {
    fn default() -> Self {
        AgentSection {
            device_id: String::new(),
            name: default_agent_name(),
            profile: default_agent_profile(),
        }
    }
}

/// The `video.wfb:` slice the pairing-info route reads. Only the radio peer's
/// device id is typed; every other wfb field is tolerated. Mirrors the Python
/// `video.wfb.paired_with_device_id`, which persists on both profiles once a
/// bind tunnel (or a PresenceBeacon) back-fills it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WfbSection {
    #[serde(default)]
    pub paired_with_device_id: Option<String>,
}

/// The `video:` section. Only the nested `wfb` slice is read here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VideoSection {
    #[serde(default)]
    pub wfb: WfbSection,
}

/// The slice of the agent config the pairing-info route projects. Every field
/// defaults so a missing section never fails the load.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PairingConfig {
    #[serde(default)]
    pub agent: AgentSection,
    #[serde(default)]
    pub video: VideoSection,
}

impl PairingConfig {
    /// The pairing slice of the shared config snapshot (see [`config_snapshot`]):
    /// one parse per change of the file, re-checked at most every
    /// [`CONFIG_CACHE_TTL`]. The hot-path reader; all-defaults on absence or a
    /// parse error, logged once per change rather than once per request.
    pub fn cached(path: &Path) -> Arc<Self> {
        config_snapshot(path, CONFIG_CACHE_TTL).pairing
    }

    /// Load from the canonical path (or the `ADOS_CONFIG` override). A missing or
    /// unparseable file yields the all-defaults config rather than failing — the
    /// pairing-info route must still answer 200.
    pub fn load() -> Self {
        let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
        Self::load_from(Path::new(&path))
    }

    /// Load from an explicit path, read fresh on every call (testable).
    /// All-defaults on absence / parse error, logging the parse error each call:
    /// per-request readers use [`Self::cached`] instead. `agent.device_id` is
    /// always the node's resolved identity
    /// ([`ados_protocol::identity::device_id`]): the provisioned file first, so
    /// a configured value only stands in on a host with no identity file.
    pub fn load_from(path: &Path) -> Self {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => ados_config::yaml_or_default(&text, "control"),
            Err(_) => PairingConfig::default(),
        };
        cfg.agent.device_id = ados_protocol::identity::device_id_at(
            &device_id_file_for(path),
            Some(&cfg.agent.device_id),
        );
        cfg
    }

    /// The radio peer's device id, or `None` when no peer is recorded or it is
    /// the empty string. Mirrors the Python guard
    /// (`isinstance(peer_raw, str) and peer_raw`): a present-but-empty value is
    /// treated as no peer.
    pub fn radio_peer_device_id(&self) -> Option<String> {
        self.video
            .wfb
            .paired_with_device_id
            .as_ref()
            .filter(|s| !s.is_empty())
            .cloned()
    }
}

/// The `security.api:` slice the proxied-route auth gate reads. Only the
/// manually-configured key is typed; every other field is tolerated. Mirrors
/// the Python `ApiSecurityConfig.api_key` (default `""`).
#[derive(Debug, Clone, Deserialize)]
pub struct ApiSecuritySection {
    #[serde(default)]
    pub api_key: String,
    /// Browser origins allowed to make cross-origin calls to this edge.
    ///
    /// Mirrors the Python `ApiSecurityConfig.cors_origins` field name and
    /// default so a config either half wrote reads identically here. The
    /// defaults ALWAYS apply on top (see
    /// [`ApiSecuritySection::effective_cors_origins`]), for the same reason
    /// the Python side does it: a deployment yaml that sets this key was
    /// otherwise dropping the local-dev Mission Control origin and silently
    /// breaking it.
    #[serde(default = "default_cors_origins")]
    pub cors_origins: Vec<String>,
    /// Extra origins added on top of the defaults. This is the key an ops
    /// file should populate.
    #[serde(default)]
    pub cors_origins_extra: Vec<String>,
}

impl Default for ApiSecuritySection {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            cors_origins: default_cors_origins(),
            cors_origins_extra: Vec::new(),
        }
    }
}

/// The Mission Control dev origins, matching `DEFAULT_CORS_ORIGINS` in
/// `src/ados/core/config/security.py`. Two lists, one contract.
pub fn default_cors_origins() -> Vec<String> {
    [
        "http://localhost:4000",
        "http://127.0.0.1:4000",
        "http://localhost:4001",
        "http://127.0.0.1:4001",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

impl ApiSecuritySection {
    /// The deduped union of the defaults, the configured list and the extras,
    /// or the full replacement named by `ADOS_CORS_ORIGINS_OVERRIDE`.
    ///
    /// Same merge and same env override as the Python half, so the two
    /// surfaces cannot disagree about who may call the agent from a browser.
    pub fn effective_cors_origins(&self) -> Vec<String> {
        if let Ok(raw) = std::env::var("ADOS_CORS_ORIGINS_OVERRIDE") {
            let listed: Vec<String> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            if !listed.is_empty() {
                return listed;
            }
        }
        let mut seen = std::collections::HashSet::new();
        default_cors_origins()
            .into_iter()
            .chain(self.cors_origins.iter().cloned())
            .chain(self.cors_origins_extra.iter().cloned())
            .filter(|o| !o.is_empty() && seen.insert(o.clone()))
            .collect()
    }
}

/// The `security:` section of `/etc/ados/config.yaml` the proxied-route auth
/// gate reads. Mirrors the Python `SecurityConfig` field names + defaults so a
/// config the Python agent wrote reads identically here:
///
/// - `api.api_key` ⇒ the manually-configured key (default `""`).
/// - `hmac_enabled` / `hmac_secret` ⇒ the HMAC signing gate (default off / `""`).
/// - `setup_token_required` ⇒ the setup-mutation token escalation (default off).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SecuritySection {
    #[serde(default)]
    pub api: ApiSecuritySection,
    #[serde(default)]
    pub hmac_enabled: bool,
    #[serde(default)]
    pub hmac_secret: String,
    #[serde(default)]
    pub setup_token_required: bool,
}

/// The top-level config slice carrying only the `security:` section. A separate
/// total reader (not folded into `PairingConfig`) so each surface types only
/// the section it needs. Like `PairingConfig`, the reader is total: a missing
/// or unparseable file yields all-defaults, so a fresh or partially-configured
/// agent never fails to load.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ControlSecurityConfig {
    #[serde(default)]
    pub security: SecuritySection,
    #[serde(default)]
    pub mcp: McpSection,
}

/// The `mcp:` section: the AI-control-surface posture the LAN auth edge reads.
/// A control-front-only section (no Python mirror). Default all-off, so the MCP
/// token path is inert until an operator opts in.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpSection {
    /// When true, the LAN auth edge accepts a scoped MCP token
    /// (`X-ADOS-MCP-Token`) as an alternative to `X-ADOS-Key` on a would-be-401,
    /// enforcing the token's scope against the route class. Default off.
    #[serde(default)]
    pub token_accept_enabled: bool,
}

impl ControlSecurityConfig {
    /// The security slice of the shared config snapshot, all-defaults when the
    /// file cannot be read or parsed. The edge gates that must fail closed on an
    /// unreadable config read [`crate::security_state::SecurityState`] instead.
    pub fn cached(path: &Path) -> Arc<Self> {
        config_snapshot(path, CONFIG_CACHE_TTL)
            .security
            .unwrap_or_default()
    }

    /// Read the slice fresh from `path`: all-defaults on absence or a parse
    /// error. For a one-off read; per-request readers use [`Self::cached`] or
    /// [`crate::security_state::SecurityState`].
    pub fn load_from(path: &Path) -> Self {
        ados_config::load_yaml_or_default(path, "control")
    }

    /// The `security:` section.
    pub fn security(&self) -> &SecuritySection {
        &self.security
    }
}

/// How long a cached config snapshot is trusted before the file is stat-ed
/// again: the same short TTL the pairing reader uses, so a config write is
/// honoured within a couple of requests without a stat per request.
pub const CONFIG_CACHE_TTL: Duration = Duration::from_secs(2);

/// The config document as the control plane types it: the pairing slice and the
/// security slice, parsed together in one pass.
#[derive(Debug, Default, Deserialize)]
struct ConfigDoc {
    #[serde(default)]
    agent: AgentSection,
    #[serde(default)]
    video: VideoSection,
    #[serde(default)]
    security: SecuritySection,
    #[serde(default)]
    mcp: McpSection,
}

/// One parse of the config, shared by every reader until the file changes.
#[derive(Clone)]
pub struct ConfigSnapshot {
    /// The pairing slice; all-defaults when the file is absent or malformed.
    pub pairing: Arc<PairingConfig>,
    /// The security slice, or why it could not be read. An absent file is the
    /// all-defaults config of a fresh node, not an error.
    pub security: Result<Arc<ControlSecurityConfig>, Arc<str>>,
}

/// Size and modification time of a file, `None` when it cannot be stat-ed.
type FileStamp = Option<(SystemTime, u64)>;

struct CacheEntry {
    /// The config file's stamp and its identity file's stamp when parsed.
    stamp: (FileStamp, FileStamp),
    checked: Instant,
    snapshot: ConfigSnapshot,
}

static CONFIG_CACHE: LazyLock<Mutex<HashMap<PathBuf, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn stamp_of(path: &Path) -> FileStamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The parsed config at `path`. Within `ttl` of the last check the cached
/// snapshot is returned untouched; after it the config file and its identity
/// file are stat-ed and the document is parsed again only when either changed.
/// A parse error is therefore logged once per change of the file.
pub fn config_snapshot(path: &Path, ttl: Duration) -> ConfigSnapshot {
    let mut cache = CONFIG_CACHE.lock();
    let now = Instant::now();
    if let Some(entry) = cache.get(path) {
        if now.duration_since(entry.checked) < ttl {
            return entry.snapshot.clone();
        }
    }
    let stamp = (stamp_of(path), stamp_of(&device_id_file_for(path)));
    if let Some(entry) = cache.get_mut(path) {
        if entry.stamp == stamp {
            entry.checked = now;
            return entry.snapshot.clone();
        }
    }
    let snapshot = parse_snapshot(path);
    cache.insert(
        path.to_path_buf(),
        CacheEntry {
            stamp,
            checked: now,
            snapshot: snapshot.clone(),
        },
    );
    snapshot
}

fn parse_snapshot(path: &Path) -> ConfigSnapshot {
    let (doc, error): (ConfigDoc, Option<String>) = match std::fs::read_to_string(path) {
        // The dedupe key names the file, so a parse error is logged once per
        // config file rather than once per process-wide "control" slice.
        Ok(text) => ados_config::yaml_reporting(&text, &format!("control:{}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (ConfigDoc::default(), None),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "config file unreadable");
            (ConfigDoc::default(), Some(e.to_string()))
        }
    };
    let ConfigDoc {
        agent,
        video,
        security,
        mcp,
    } = doc;
    let mut pairing = PairingConfig { agent, video };
    pairing.agent.device_id = ados_protocol::identity::device_id_at(
        &device_id_file_for(path),
        Some(&pairing.agent.device_id),
    );
    let security = match error {
        None => Ok(Arc::new(ControlSecurityConfig { security, mcp })),
        Some(e) => Err(Arc::from(e)),
    };
    ConfigSnapshot {
        pairing: Arc::new(pairing),
        security,
    }
}

/// Read the agent config as an untyped object for the routes that project a
/// handful of raw fields rather than deserializing a typed slice.
///
/// Always an object: a missing file, an unreadable one, a parse failure, and a
/// YAML document that is not a mapping all yield `{}`, so callers can index
/// straight into it. What changes versus reading it inline is that a
/// PRESENT-but-malformed file now logs the parser error (which names the
/// offending field) instead of being indistinguishable from an absent one — the
/// case where a typo in the config reads on the status surface as "this feature
/// is simply not configured".
///
/// Deliberately the quiet-sidecar helper, not `write_config_status`: these are
/// per-request readers, and publishing a service-level status sidecar on every
/// request is a startup concern (the daemon publishes it once at bring-up).
pub fn load_config_object(path: &Path) -> serde_json::Value {
    let value: serde_json::Value = ados_config::load_yaml_or_default(path, "control");
    if value.is_object() {
        value
    } else {
        serde_json::json!({})
    }
}

/// [`load_config_object`] against the configured config path, honoring the
/// `ADOS_CONFIG` override the systemd unit sets.
pub fn load_config_object_default() -> serde_json::Value {
    let path = std::env::var("ADOS_CONFIG").unwrap_or_else(|_| CONFIG_YAML.to_string());
    load_config_object(Path::new(&path))
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
