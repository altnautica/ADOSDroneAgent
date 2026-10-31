//! Agent-half, permission, resource, and payload declarations.

use super::*;

/// Agent-half isolation levels. `inprocess` is first-party only; the
/// controller enforces that gate before unpack. `subprocess` is the default,
/// matching the Pydantic `AgentBlock.isolation` default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentIsolation {
    #[default]
    Subprocess,
    Inprocess,
}

/// Agent-half runtime: which executor systemd starts for the plugin process.
/// `python` (the default) runs the plugin through the shared Python runner;
/// `rust` execs the plugin's own binary directly. Additive and optional, so an
/// old manifest with no `runtime:` field parses as `python` unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRuntime {
    #[default]
    Python,
    Rust,
}

/// GCS-half isolation levels. `inline` is first-party only; the controller
/// rejects it for third-party signers even though the browser runtime polices
/// it as well. `iframe` is the default, matching the Pydantic
/// `GcsBlock.isolation` default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GcsIsolation {
    #[default]
    Iframe,
    Inline,
}

impl GcsIsolation {
    /// The wire spelling (`iframe` / `inline`).
    pub fn as_str(self) -> &'static str {
        match self {
            GcsIsolation::Iframe => "iframe",
            GcsIsolation::Inline => "inline",
        }
    }
}

/// One declared permission. A bare YAML string parses through the same path as
/// the object form `{id, required, degraded_behavior}` — only `id` is read by
/// the controller.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "PermissionRefRaw")]
pub struct PermissionRef {
    pub id: String,
    pub required: bool,
    pub degraded_behavior: Option<String>,
}

/// Untagged shape that accepts either a bare string or the full object form,
/// matching the Pydantic `_normalize_permission` before-validator.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum PermissionRefRaw {
    Id(String),
    Object {
        id: String,
        #[serde(default = "default_true")]
        required: bool,
        #[serde(default)]
        degraded_behavior: Option<String>,
    },
}

pub(super) fn default_true() -> bool {
    true
}

impl From<PermissionRefRaw> for PermissionRef {
    fn from(raw: PermissionRefRaw) -> Self {
        match raw {
            PermissionRefRaw::Id(id) => PermissionRef {
                id,
                required: true,
                degraded_behavior: None,
            },
            PermissionRefRaw::Object {
                id,
                required,
                degraded_behavior,
            } => PermissionRef {
                id,
                required,
                degraded_behavior,
            },
        }
    }
}

/// Resource class of an agent half. `heavy` lifts the bounds for a workload
/// such as reconstruction or accelerator inference and is first-party only;
/// the controller refuses it from any other signer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceClass {
    #[default]
    Standard,
    Heavy,
}

/// Hard resource limits the generated unit applies. Defaults match the
/// Pydantic `ResourceLimits` (96 MiB / 25% CPU / 12 pids).
#[derive(Debug, Clone, Deserialize)]
pub struct ResourceLimits {
    #[serde(default, rename = "class")]
    pub class: ResourceClass,
    #[serde(default = "default_max_ram_mb")]
    pub max_ram_mb: u32,
    #[serde(default = "default_max_cpu_percent")]
    pub max_cpu_percent: u32,
    #[serde(default = "default_max_pids")]
    pub max_pids: u32,
}

pub(super) fn default_max_ram_mb() -> u32 {
    96
}
pub(super) fn default_max_cpu_percent() -> u32 {
    25
}
pub(super) fn default_max_pids() -> u32 {
    12
}

impl Default for ResourceLimits {
    fn default() -> Self {
        ResourceLimits {
            class: ResourceClass::Standard,
            max_ram_mb: default_max_ram_mb(),
            max_cpu_percent: default_max_cpu_percent(),
            max_pids: default_max_pids(),
        }
    }
}

impl ResourceLimits {
    /// The `(max_ram_mb, max_cpu_percent, max_pids)` ceilings of this class.
    /// The floors are 8 MiB / 1% / 1 pid for both classes.
    pub fn ceilings(&self) -> (u32, u32, u32) {
        match self.class {
            ResourceClass::Standard => (4096, 100, 256),
            ResourceClass::Heavy => (65536, 3200, 4096),
        }
    }

    pub(super) fn validate(&self) -> Result<(), ManifestError> {
        let (ram, cpu, pids) = self.ceilings();
        let class = match self.class {
            ResourceClass::Standard => "standard",
            ResourceClass::Heavy => "heavy",
        };
        for (field, value, min, max) in [
            ("max_ram_mb", self.max_ram_mb, 8, ram),
            ("max_cpu_percent", self.max_cpu_percent, 1, cpu),
            ("max_pids", self.max_pids, 1, pids),
        ] {
            if !(min..=max).contains(&value) {
                return Err(ManifestError(format!(
                    "agent.resources.{field} {value} is outside {min}..={max} for the {class} \
                     resource class"
                )));
            }
        }
        Ok(())
    }
}

/// One file the agent downloads at install instead of shipping it inside the
/// archive (a large per-architecture binary, a model, a wasm asset). The pinned
/// `sha256` is covered by the archive signature because it lives in the signed
/// `manifest.yaml`, so a payload is as trusted as a packed file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PayloadSpec {
    /// Archive-relative destination inside the plugin's install dir.
    pub path: String,
    /// `https://` URL on the download allowlist.
    pub source: String,
    /// Lowercase hex sha256 of the file.
    pub sha256: String,
    /// Exact size in bytes, at most [`PAYLOAD_MAX_BYTES`].
    pub size_bytes: u64,
    /// Node profiles this payload is fetched on; absent means every profile.
    #[serde(default, deserialize_with = "deserialize_opt_profiles")]
    pub profiles: Option<Vec<String>>,
    /// The `<arch>-<os>` host this payload is fetched on; absent means any.
    #[serde(default)]
    pub arch_os: Option<String>,
}

impl PayloadSpec {
    /// Whether a node of `profile` on an `arch_os` host fetches this payload.
    pub fn applies_to(&self, profile: &str, arch_os: &str) -> bool {
        self.profiles
            .as_ref()
            .is_none_or(|p| p.iter().any(|x| x == profile))
            && self.arch_os.as_deref().is_none_or(|a| a == arch_os)
    }
}

/// A capability a plugin defines for its own shared data, grantable to other
/// plugins like a catalog capability. The id is `plugin.<leaf>.<name>`, where
/// `<leaf>` is the last segment of the declaring plugin's id.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DeclaredCapability {
    pub id: String,
    pub risk: String,
    #[serde(default)]
    pub description: String,
}

/// A topic a plugin owns and shares on the event bus: the owner may publish it,
/// and another plugin may subscribe with `event.subscribe` plus
/// `subscribe_capability`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SharedTopicDecl {
    pub topic: String,
    pub subscribe_capability: String,
}

/// A page a plugin adds to a node's Agent sidebar (`gcs.contributes.agent_pages`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPage {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_profiles",
        skip_serializing_if = "Option::is_none"
    )]
    pub profile: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_for: Option<String>,
}

/// A surface a plugin adds to a node's tab strip (`gcs.contributes.node_surfaces`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSurface {
    pub id: String,
    pub title: String,
    #[serde(deserialize_with = "deserialize_profiles")]
    pub profile: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
}

/// The node-surface groups a contribution may join.
pub const NODE_SURFACE_GROUPS: &[&str] = &["status", "vehicle", "link", "device", "compute"];

/// Agent-half manifest block. Only the fields the controller reads are typed;
/// the others are shape-checked by the conformance pass and kept in `extra`.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentBlock {
    pub entrypoint: String,
    #[serde(default)]
    pub isolation: AgentIsolation,
    /// Which executor systemd starts: the shared Python runner (default) or the
    /// plugin's own binary (`rust`). Additive and optional.
    #[serde(default)]
    pub runtime: AgentRuntime,
    #[serde(default)]
    pub permissions: Vec<PermissionRef>,
    #[serde(default)]
    pub resources: ResourceLimits,
    /// Basenames the plugin may `process.spawn` at runtime. The host enforces
    /// the `process.spawn` allowlist against this list (the same field as the
    /// Python `AgentBlock.subprocess_spawn`). Empty (the default) means no spawn is
    /// permitted even with the `process.spawn` capability.
    #[serde(default)]
    pub subprocess_spawn: Vec<String>,
    /// Node profiles the agent half installs on. Defaults to `["drone"]`;
    /// `ground_station` is read as `ground-station`.
    #[serde(
        default = "default_target_profiles",
        deserialize_with = "deserialize_profiles"
    )]
    pub target_profiles: Vec<String>,
    /// Per-architecture binaries: `{<name>: {<arch>-<os>: <relative path>}}`.
    /// `bin:<name>` in `entrypoint` or a service command resolves through it
    /// to this host's path.
    #[serde(default)]
    pub binaries: BTreeMap<String, BTreeMap<String, String>>,
    /// Files fetched at install rather than shipped in the archive.
    #[serde(default)]
    pub payloads: Vec<PayloadSpec>,
    /// When true the host prepares `<run dir>/plugin-http/<id>/` and the
    /// plugin serves HTTP for the operator on `http.sock` there.
    #[serde(default)]
    pub http: bool,
    /// Capabilities this plugin defines for its own shared data.
    #[serde(default)]
    pub declared_capabilities: Vec<DeclaredCapability>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_norway::Value>,
}

impl AgentBlock {
    /// The path `bin:<name>` resolves to on an `arch_os` host, relative to the
    /// plugin's install dir.
    pub fn binary_path(&self, name: &str, arch_os: &str) -> Option<&str> {
        self.binaries
            .get(name)
            .and_then(|by_arch| by_arch.get(arch_os))
            .map(String::as_str)
    }

    /// Whether the agent half installs on a node of `profile`.
    pub fn targets_profile(&self, profile: &str) -> bool {
        self.target_profiles.iter().any(|p| p == profile)
    }

    /// The shared topics declared under `contributes.shared_topics`. Empty when
    /// absent; a malformed block is refused by [`PluginManifest::validate`].
    pub fn shared_topics(&self) -> Vec<SharedTopicDecl> {
        self.parse_shared_topics().unwrap_or_default()
    }

    pub(super) fn parse_shared_topics(&self) -> Result<Vec<SharedTopicDecl>, ManifestError> {
        match self
            .extra
            .get("contributes")
            .and_then(|c| c.get("shared_topics"))
        {
            None => Ok(Vec::new()),
            Some(raw) => serde_norway::from_value(raw.clone()).map_err(|e| {
                ManifestError(format!("agent.contributes.shared_topics is malformed: {e}"))
            }),
        }
    }

    /// `contains_vendor_binary` and a non-empty `vendor_attribution` must
    /// agree: a vendor binary needs its source-offer record, and a record
    /// without the flag is a typo the install dialog would not surface.
    pub(super) fn validate_vendor_attribution(&self) -> Result<(), ManifestError> {
        let contains = match self.extra.get("contains_vendor_binary") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(|| {
                ManifestError("agent.contains_vendor_binary must be a boolean".to_string())
            })?,
        };
        let has_attribution = match self.extra.get("vendor_attribution") {
            None | Some(serde_norway::Value::Null) => false,
            Some(serde_norway::Value::Sequence(records)) => !records.is_empty(),
            Some(_) => {
                return Err(ManifestError(
                    "agent.vendor_attribution must be a list".to_string(),
                ))
            }
        };
        if contains && !has_attribution {
            return Err(ManifestError(
                "agent.contains_vendor_binary is true but agent.vendor_attribution is empty; \
                 at least one source-offer record is required for vendor-binary plugins"
                    .to_string(),
            ));
        }
        if has_attribution && !contains {
            return Err(ManifestError(
                "agent.vendor_attribution is set but agent.contains_vendor_binary is false; \
                 set the flag or remove the attribution block"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// The manifest `risk` levels, lowest first.
pub const RISK_LEVELS: &[&str] = &["low", "medium", "high", "critical"];

/// The name a `bin:<name>` reference carries, or `None` when `value` is not one.
pub fn bin_reference(value: &str) -> Option<&str> {
    value.strip_prefix(BIN_PREFIX)
}
