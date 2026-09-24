//! Plugin manifest model.
//!
//! The manifest is the declarative contract loaded from `manifest.yaml` inside
//! a `.adosplug` archive: identity, the halves shipped, the capabilities
//! requested, the lifecycle settings, and the compatibility constraints. After
//! signature verification the manifest is the only field the host trusts; the
//! install/enable/disable/remove controller is driven entirely from it.
//!
//! This is the lifecycle-facing subset of the contract. The full schema (the
//! one the SDK ships and the docs render) lives at
//! `schemas/plugin-manifest.schema.json`; this struct reads the fields the
//! controller acts on (id, version, name, risk, the agent/gcs halves, the
//! isolation levels, the resource limits, the declared permissions, and the
//! compatibility block) and tolerates every other field through
//! `#[serde(default)]` + an open `extra` map, mirroring the Pydantic model's
//! forward-compatible posture.
//!
//! Identity and path fields are validated at parse time
//! ([`PluginManifest::validate`]), on every install path: the id is joined onto
//! the install dir that a root process removes and unpacks into, and both the
//! id and the entrypoint are interpolated into the generated systemd unit. A
//! signature proves who packed an archive, not that its manifest is well
//! formed, so none of this is left to the SDK packer.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};

use crate::errors::ManifestError;

/// The node profiles a plugin half may target, in their canonical wire form.
pub const NODE_PROFILES: &[&str] = &["drone", "ground-station", "workstation", "compute"];

/// Prefix that marks an entrypoint or a service command's first word as a
/// reference into the manifest's per-architecture `binaries` table.
pub const BIN_PREFIX: &str = "bin:";

/// Hard cap on one downloaded payload, in bytes (1 GiB).
pub const PAYLOAD_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// The `<arch>-<os>` key this host resolves `binaries` and payload `arch_os`
/// against: `aarch64-linux`, `x86_64-linux`, `aarch64-macos`.
pub fn host_arch_os() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The canonical form of a profile name, or `None` when it names no profile.
/// `ground_station` is accepted as the underscore spelling of `ground-station`.
pub fn canonical_profile(raw: &str) -> Option<&'static str> {
    let raw = if raw == "ground_station" {
        "ground-station"
    } else {
        raw
    };
    NODE_PROFILES.iter().copied().find(|p| *p == raw)
}

/// Normalise a profile list at parse: the underscore spelling becomes the
/// hyphen form and duplicates collapse, keeping first-seen order. An unknown
/// name is kept verbatim so [`PluginManifest::validate`] can name it.
fn normalize_profiles(raw: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in raw {
        let entry = canonical_profile(&entry).map_or(entry, str::to_string);
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

fn deserialize_profiles<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Vec::<String>::deserialize(d).map(normalize_profiles)
}

fn deserialize_opt_profiles<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<String>>, D::Error> {
    Option::<Vec<String>>::deserialize(d).map(|v| v.map(normalize_profiles))
}

fn default_target_profiles() -> Vec<String> {
    vec!["drone".to_string()]
}

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
enum PermissionRefRaw {
    Id(String),
    Object {
        id: String,
        #[serde(default = "default_true")]
        required: bool,
        #[serde(default)]
        degraded_behavior: Option<String>,
    },
}

fn default_true() -> bool {
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

fn default_max_ram_mb() -> u32 {
    96
}
fn default_max_cpu_percent() -> u32 {
    25
}
fn default_max_pids() -> u32 {
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

    fn validate(&self) -> Result<(), ManifestError> {
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
/// every other field is tolerated.
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
    /// the `process.spawn` allowlist against this list (mirrors the Python
    /// `AgentBlock.subprocess_spawn`). Empty (the default) means no spawn is
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

    fn parse_shared_topics(&self) -> Result<Vec<SharedTopicDecl>, ManifestError> {
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
}

/// The name a `bin:<name>` reference carries, or `None` when `value` is not one.
pub fn bin_reference(value: &str) -> Option<&str> {
    value.strip_prefix(BIN_PREFIX)
}

/// GCS-half manifest block. Only `entrypoint` + `isolation` + `permissions`
/// are read by the controller; the page and surface contributions are
/// validated here and projected by [`gcs_block_json`].
#[derive(Debug, Clone, Deserialize)]
pub struct GcsBlock {
    pub entrypoint: String,
    #[serde(default)]
    pub isolation: GcsIsolation,
    #[serde(default)]
    pub permissions: Vec<PermissionRef>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_norway::Value>,
}

impl GcsBlock {
    fn contribution<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Vec<T>, ManifestError> {
        match self.extra.get("contributes").and_then(|c| c.get(key)) {
            None => Ok(Vec::new()),
            Some(raw) => serde_norway::from_value(raw.clone())
                .map_err(|e| ManifestError(format!("gcs.contributes.{key} is malformed: {e}"))),
        }
    }

    /// The Agent-sidebar pages this half contributes.
    pub fn agent_pages(&self) -> Vec<AgentPage> {
        self.contribution("agent_pages").unwrap_or_default()
    }

    /// The node surfaces this half contributes.
    pub fn node_surfaces(&self) -> Vec<NodeSurface> {
        self.contribution("node_surfaces").unwrap_or_default()
    }

    fn validate_contributions(&self) -> Result<(), ManifestError> {
        let pages: Vec<AgentPage> = self.contribution("agent_pages")?;
        let surfaces: Vec<NodeSurface> = self.contribution("node_surfaces")?;
        let mut page_ids: BTreeSet<&str> = BTreeSet::new();
        for page in &pages {
            validate_contribution_id("gcs.contributes.agent_pages", &page.id)?;
            validate_title("gcs.contributes.agent_pages", &page.id, &page.title)?;
            if let Some(profiles) = &page.profile {
                validate_profile_list(
                    &format!("gcs.contributes.agent_pages[{}].profile", page.id),
                    profiles,
                )?;
            }
            if !page_ids.insert(page.id.as_str()) {
                return Err(ManifestError(format!(
                    "gcs.contributes.agent_pages id {:?} appears twice",
                    page.id
                )));
            }
        }
        for page in &pages {
            let Some(target) = page.setup_for.as_deref() else {
                continue;
            };
            let host = pages.iter().find(|p| p.id == target);
            if target == page.id || host.is_none() || host.is_some_and(|h| h.setup_for.is_some()) {
                return Err(ManifestError(format!(
                    "gcs.contributes.agent_pages[{}].setup_for {target:?} must name another \
                     page of this plugin that is not itself a setup page",
                    page.id
                )));
            }
        }
        let mut surface_ids: BTreeSet<&str> = BTreeSet::new();
        for surface in &surfaces {
            validate_contribution_id("gcs.contributes.node_surfaces", &surface.id)?;
            validate_title("gcs.contributes.node_surfaces", &surface.id, &surface.title)?;
            validate_profile_list(
                &format!("gcs.contributes.node_surfaces[{}].profile", surface.id),
                &surface.profile,
            )?;
            if let Some(group) = &surface.group {
                if !NODE_SURFACE_GROUPS.contains(&group.as_str()) {
                    return Err(ManifestError(format!(
                        "gcs.contributes.node_surfaces[{}].group {group:?} must be one of \
                         {NODE_SURFACE_GROUPS:?}",
                        surface.id
                    )));
                }
            }
            if !surface_ids.insert(surface.id.as_str()) {
                return Err(ManifestError(format!(
                    "gcs.contributes.node_surfaces id {:?} appears twice",
                    surface.id
                )));
            }
        }
        Ok(())
    }
}

/// The free-form GCS contribution lists the detail projection carries through
/// unchanged, in the order they are emitted.
const GCS_FREEFORM_CONTRIBUTIONS: &[&str] = &[
    "panels",
    "overlays",
    "notifications",
    "skills",
    "tabs",
    "parameters",
    "models",
    "target_actions",
    "settings",
];

/// The `manifest.gcs` block of the plugin detail route: the GCS half's
/// entrypoint, isolation, slot contributions and locales, so a LAN-paired GCS
/// can build the contribution set and locate the bundle with no cloud. `None`
/// for a plugin with no GCS half.
pub fn gcs_block_json(manifest: &PluginManifest) -> Option<serde_json::Value> {
    let gcs = manifest.gcs.as_ref()?;
    let contributes = gcs.extra.get("contributes");
    let mut map = serde_json::Map::new();
    for key in GCS_FREEFORM_CONTRIBUTIONS {
        let list = contributes
            .and_then(|c| c.get(*key))
            .filter(|v| v.is_sequence())
            .and_then(|v| serde_json::to_value(v).ok())
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        map.insert((*key).to_string(), list);
    }
    map.insert(
        "agent_pages".to_string(),
        serde_json::to_value(gcs.agent_pages()).unwrap_or_default(),
    );
    map.insert(
        "node_surfaces".to_string(),
        serde_json::to_value(gcs.node_surfaces()).unwrap_or_default(),
    );
    let locales = gcs
        .extra
        .get("locales")
        .filter(|v| v.is_sequence())
        .and_then(|v| serde_json::to_value(v).ok())
        .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    Some(serde_json::json!({
        "entrypoint": gcs.entrypoint,
        "isolation": gcs.isolation.as_str(),
        "contributes": serde_json::Value::Object(map),
        "locales": locales,
    }))
}

/// Compatibility constraints checked at install time.
#[derive(Debug, Clone, Deserialize)]
pub struct Compatibility {
    /// Semver range, e.g. `>=0.9.0,<1.0.0`.
    pub ados_version: String,
    #[serde(default)]
    pub gcs_version: Option<String>,
    /// HAL board ids this plugin supports. Empty means any board. A list
    /// containing the literal `"*"` also means any board — the scaffolder used
    /// to emit that form and it was an exact-match reject on every real board,
    /// so both spellings of "any" are accepted (mirrors the Pydantic
    /// `Compatibility.supported_boards` note).
    #[serde(default)]
    pub supported_boards: Vec<String>,
    /// Minimum compute tier (1-4) the plugin needs. `None` means no floor.
    ///
    /// Present here because the cloud-relay install path runs this parser, and
    /// dropping the field made the tier floor enforceable over LAN but not
    /// over the relay: an NPU-dependent plugin refused on a tier-1 board by
    /// the Python supervisor installed cleanly when pushed from the cloud, then
    /// crash-looped at runtime instead of being refused up front.
    #[serde(default)]
    pub min_tier: Option<u8>,
}

impl Compatibility {
    /// True when `board` satisfies `supported_boards`. Empty list or a `"*"`
    /// entry means any board.
    pub fn supports_board(&self, board: &str) -> bool {
        self.supported_boards.is_empty()
            || self.supported_boards.iter().any(|b| b == "*" || b == board)
    }
}

fn default_risk() -> String {
    "medium".to_string()
}

/// Top-level plugin manifest. Loaded from `manifest.yaml`.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_risk")]
    pub risk: String,
    pub compatibility: Compatibility,
    #[serde(default)]
    pub agent: Option<AgentBlock>,
    #[serde(default)]
    pub gcs: Option<GcsBlock>,
    /// Every other top-level field (install-dialog copy, schema_version,
    /// author, license, extra) is tolerated and ignored by the controller.
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_norway::Value>,
}

impl PluginManifest {
    /// Parse a manifest from YAML text. Mirrors `PluginManifest.from_yaml_text`:
    /// the top level must be a mapping, otherwise a [`ManifestError`] is raised
    /// with the parse error message.
    pub fn from_yaml_text(text: &str) -> Result<PluginManifest, ManifestError> {
        let value: serde_norway::Value = serde_norway::from_str(text)
            .map_err(|e| ManifestError(format!("manifest is not valid YAML: {e}")))?;
        if !value.is_mapping() {
            return Err(ManifestError(
                "manifest top-level must be a mapping".to_string(),
            ));
        }
        let manifest: PluginManifest =
            serde_norway::from_value(value).map_err(|e| ManifestError(e.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Enforce the identity, path and contribution rules the lifecycle relies
    /// on, matching the Pydantic model's validators:
    ///
    /// * `id` is reverse-DNS: `^[a-z0-9]+(\.[a-z0-9-]+)+$`. It becomes a
    ///   directory name under the install dir and a token in unit text, so it
    ///   can never be absolute, contain `/` or `..`, or carry whitespace.
    /// * `version` is semver 2.0.
    /// * each entrypoint is a relative POSIX path over `[A-Za-z0-9._/-]` with no
    ///   empty or `..` segment, or a `module:Class` reference; a rust agent
    ///   entrypoint may also be `bin:<name>` naming a `binaries` entry.
    /// * target profiles, resource bounds, binaries, payloads, declared
    ///   capabilities, shared topics and the GCS page/surface contributions
    ///   are well formed.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if !is_plugin_id(&self.id) {
            return Err(ManifestError(format!(
                "plugin id {:?} must be reverse-DNS lowercase, e.g. com.example.thermal",
                self.id
            )));
        }
        if !is_semver(&self.version) {
            return Err(ManifestError(format!(
                "plugin version {:?} is not valid semver",
                self.version
            )));
        }
        if let Some(agent) = &self.agent {
            self.validate_agent(agent)?;
        }
        if let Some(gcs) = &self.gcs {
            validate_entrypoint("gcs.entrypoint", &gcs.entrypoint)?;
            gcs.validate_contributions()?;
        }
        Ok(())
    }

    fn validate_agent(&self, agent: &AgentBlock) -> Result<(), ManifestError> {
        match bin_reference(&agent.entrypoint) {
            Some(name) => {
                if agent.runtime != AgentRuntime::Rust {
                    return Err(ManifestError(format!(
                        "agent.entrypoint {:?} names a binary, which needs runtime: rust",
                        agent.entrypoint
                    )));
                }
                if !agent.binaries.contains_key(name) {
                    return Err(ManifestError(format!(
                        "agent.entrypoint {:?} names no entry of agent.binaries",
                        agent.entrypoint
                    )));
                }
            }
            None => validate_entrypoint("agent.entrypoint", &agent.entrypoint)?,
        }
        if agent.target_profiles.is_empty() {
            return Err(ManifestError(
                "agent.target_profiles must list at least one profile".to_string(),
            ));
        }
        validate_profile_list("agent.target_profiles", &agent.target_profiles)?;
        agent.resources.validate()?;
        for (name, by_arch) in &agent.binaries {
            if !is_bin_name(name) {
                return Err(ManifestError(format!(
                    "agent.binaries name {name:?} must match [a-z0-9][a-z0-9_-]{{0,63}}"
                )));
            }
            for (arch_os, path) in by_arch {
                if !is_arch_os(arch_os) {
                    return Err(ManifestError(format!(
                        "agent.binaries.{name} key {arch_os:?} must be <arch>-<os>, e.g. \
                         aarch64-linux"
                    )));
                }
                if !is_relative_posix_path(path) {
                    return Err(ManifestError(format!(
                        "agent.binaries.{name}.{arch_os} {path:?} must be a relative posix path"
                    )));
                }
            }
        }
        let mut payload_paths: BTreeSet<&str> = BTreeSet::new();
        for payload in &agent.payloads {
            validate_payload(payload)?;
            if !payload_paths.insert(payload.path.as_str()) {
                return Err(ManifestError(format!(
                    "agent.payloads path {:?} appears twice",
                    payload.path
                )));
            }
        }
        let leaf = self.leaf();
        let mut declared: BTreeSet<&str> = BTreeSet::new();
        for cap in &agent.declared_capabilities {
            let name = cap
                .id
                .strip_prefix("plugin.")
                .and_then(|rest| rest.strip_prefix(leaf))
                .and_then(|rest| rest.strip_prefix('.'));
            if !name.is_some_and(is_declared_cap_name) {
                return Err(ManifestError(format!(
                    "agent.declared_capabilities id {:?} must be plugin.{leaf}.<name>",
                    cap.id
                )));
            }
            if !matches!(cap.risk.as_str(), "low" | "medium" | "high" | "critical") {
                return Err(ManifestError(format!(
                    "agent.declared_capabilities[{}].risk {:?} must be low, medium, high or \
                     critical",
                    cap.id, cap.risk
                )));
            }
            if !declared.insert(cap.id.as_str()) {
                return Err(ManifestError(format!(
                    "agent.declared_capabilities id {:?} appears twice",
                    cap.id
                )));
            }
        }
        let prefix = format!("plugin.{leaf}.");
        let mut topics: BTreeSet<String> = BTreeSet::new();
        for shared in agent.parse_shared_topics()? {
            if !is_shared_topic(&shared.topic) || !shared.topic.starts_with(&prefix) {
                return Err(ManifestError(format!(
                    "agent.contributes.shared_topics topic {:?} must be {prefix}<name> over \
                     [a-z0-9._-], at most 128 characters",
                    shared.topic
                )));
            }
            let cap = shared.subscribe_capability.as_str();
            if ados_protocol::capabilities::get_agent_capability(cap).is_none()
                && !declared.contains(cap)
            {
                return Err(ManifestError(format!(
                    "agent.contributes.shared_topics[{}].subscribe_capability {cap:?} is \
                     neither a catalog capability nor one this plugin declares",
                    shared.topic
                )));
            }
            if !topics.insert(shared.topic.clone()) {
                return Err(ManifestError(format!(
                    "agent.contributes.shared_topics topic {:?} appears twice",
                    shared.topic
                )));
            }
        }
        Ok(())
    }

    /// The last dot-segment of the plugin id: the namespace its shared topics
    /// and declared capabilities live under (`com.example.mapper` -> `mapper`).
    pub fn leaf(&self) -> &str {
        self.id.rsplit('.').next().unwrap_or(&self.id)
    }

    /// The payloads a node of `profile` on an `arch_os` host fetches.
    pub fn applicable_payloads(&self, profile: &str, arch_os: &str) -> Vec<&PayloadSpec> {
        self.agent
            .as_ref()
            .map(|a| {
                a.payloads
                    .iter()
                    .filter(|p| p.applies_to(profile, arch_os))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Flat set of declared permission ids across both halves. Used by the
    /// state filter (a granted permission the manifest no longer declares is
    /// dropped) and the install-dialog permission preview.
    pub fn declared_permissions(&self) -> BTreeSet<String> {
        let mut ids: BTreeSet<String> = BTreeSet::new();
        if let Some(agent) = &self.agent {
            ids.extend(agent.permissions.iter().map(|p| p.id.clone()));
        }
        if let Some(gcs) = &self.gcs {
            ids.extend(gcs.permissions.iter().map(|p| p.id.clone()));
        }
        ids
    }

    /// True when this plugin's agent half runs as a generated systemd unit.
    /// `inprocess` (built-in, first-party) and gcs-only plugins do not.
    pub fn is_subprocess_agent(&self) -> bool {
        matches!(
            &self.agent,
            Some(a) if a.isolation == AgentIsolation::Subprocess
        )
    }

    /// The agent half's runtime, or `None` when there is no agent half. A
    /// gcs-only plugin has no agent runtime.
    pub fn agent_runtime(&self) -> Option<AgentRuntime> {
        self.agent.as_ref().map(|a| a.runtime)
    }
}

/// `^[a-z0-9]+(\.[a-z0-9-]+)+$`: at least two dot-separated segments, the first
/// lowercase alnum, the rest lowercase alnum or hyphen.
fn is_plugin_id(id: &str) -> bool {
    let mut segments = id.split('.');
    let first_ok = segments.next().is_some_and(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    });
    let mut rest = 0usize;
    let rest_ok = segments.all(|s| {
        rest += 1;
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    first_ok && rest_ok && rest > 0
}

/// Semver 2.0: `MAJOR.MINOR.PATCH`, no leading zeros, with an optional
/// dot-separated pre-release (`-...`) and build (`+...`) suffix.
fn is_semver(v: &str) -> bool {
    let (rest, build) = match v.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (v, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let numeric = |s: &str| {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'))
    };
    let alnum =
        |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| numeric(p)) {
        return false;
    }
    // A pre-release identifier is numeric without leading zeros, or
    // alphanumeric with at least one non-digit.
    let pre_ok = pre.is_none_or(|p| {
        p.split('.')
            .all(|id| alnum(id) && (numeric(id) || id.bytes().any(|b| !b.is_ascii_digit())))
    });
    let build_ok = build.is_none_or(|b| b.split('.').all(alnum));
    pre_ok && build_ok
}

/// Refuse an entrypoint that could escape the plugin's install dir or break
/// out of the unit line it is interpolated into.
fn validate_entrypoint(field: &str, value: &str) -> Result<(), ManifestError> {
    let ok = match value.split_once(':') {
        Some((module, class)) => is_module_path(module) && is_identifier(class),
        None => is_relative_posix_path(value),
    };
    if ok {
        Ok(())
    } else {
        Err(ManifestError(format!(
            "{field} {value:?} must be a relative posix path over [A-Za-z0-9._/-] \
             with no empty or '..' segment, or a module:Class reference"
        )))
    }
}

fn validate_profile_list(field: &str, profiles: &[String]) -> Result<(), ManifestError> {
    if profiles.is_empty() {
        return Err(ManifestError(format!(
            "{field} must list at least one profile"
        )));
    }
    for p in profiles {
        if canonical_profile(p).is_none() {
            return Err(ManifestError(format!(
                "{field} entry {p:?} must be one of {NODE_PROFILES:?}"
            )));
        }
    }
    Ok(())
}

fn validate_payload(payload: &PayloadSpec) -> Result<(), ManifestError> {
    let path = &payload.path;
    if !is_relative_posix_path(path) {
        return Err(ManifestError(format!(
            "agent.payloads path {path:?} must be a relative posix path"
        )));
    }
    if !payload.source.starts_with("https://") {
        return Err(ManifestError(format!(
            "agent.payloads[{path}].source must be an https:// URL"
        )));
    }
    if payload.sha256.len() != 64
        || !payload
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ManifestError(format!(
            "agent.payloads[{path}].sha256 must be 64 lowercase hex characters"
        )));
    }
    if !(1..=PAYLOAD_MAX_BYTES).contains(&payload.size_bytes) {
        return Err(ManifestError(format!(
            "agent.payloads[{path}].size_bytes must be 1..={PAYLOAD_MAX_BYTES}"
        )));
    }
    if let Some(profiles) = &payload.profiles {
        validate_profile_list(&format!("agent.payloads[{path}].profiles"), profiles)?;
    }
    if let Some(arch_os) = &payload.arch_os {
        if !is_arch_os(arch_os) {
            return Err(ManifestError(format!(
                "agent.payloads[{path}].arch_os {arch_os:?} must be <arch>-<os>"
            )));
        }
    }
    Ok(())
}

fn validate_contribution_id(field: &str, id: &str) -> Result<(), ManifestError> {
    if is_bin_name(id) {
        Ok(())
    } else {
        Err(ManifestError(format!(
            "{field} id {id:?} must match [a-z0-9][a-z0-9_-]{{0,63}}"
        )))
    }
}

fn validate_title(field: &str, id: &str, title: &str) -> Result<(), ManifestError> {
    let len = title.chars().count();
    if (1..=80).contains(&len) && !title.chars().any(char::is_control) {
        Ok(())
    } else {
        Err(ManifestError(format!(
            "{field}[{id}].title must be 1-80 printable characters"
        )))
    }
}

/// `[a-z0-9][a-z0-9_-]{0,63}`: a `binaries` name, and a GCS contribution id.
pub(crate) fn is_bin_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// `[a-z0-9_]+-[a-z0-9_]+`: an `<arch>-<os>` key.
fn is_arch_os(s: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    s.split_once('-')
        .is_some_and(|(arch, os)| part(arch) && part(os))
}

/// `[a-z0-9][a-z0-9._-]{0,63}`: the `<name>` of a declared capability.
fn is_declared_cap_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.len() <= 64
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// `plugin.<segment>.<name>` over `[a-z0-9._-]`, at most 128 characters.
fn is_shared_topic(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("plugin.") else {
        return false;
    };
    let Some((segment, name)) = rest.split_once('.') else {
        return false;
    };
    s.len() <= 128
        && !segment.is_empty()
        && segment
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn is_relative_posix_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && !segment.starts_with(".."))
}

fn is_identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn is_module_path(s: &str) -> bool {
    s.split('.').all(is_identifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
id: com.example.thermal
version: 1.0.0
name: Thermal
compatibility:
  ados_version: ">=0.9.0,<1.0.0"
agent:
  entrypoint: agent/py/thermal.py
  permissions:
    - hardware.spi
    - id: mission.write
      required: false
"#;

    #[test]
    fn parses_minimal_and_reads_lifecycle_fields() {
        let m = PluginManifest::from_yaml_text(MINIMAL).unwrap();
        assert_eq!(m.id, "com.example.thermal");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.risk, "medium");
        let agent = m.agent.as_ref().unwrap();
        assert_eq!(agent.isolation, AgentIsolation::Subprocess);
        assert_eq!(agent.resources.max_ram_mb, 96);
        assert!(m.is_subprocess_agent());
        assert_eq!(
            m.declared_permissions(),
            ["hardware.spi", "mission.write"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
    }

    #[test]
    fn bare_string_and_object_permissions_both_parse() {
        let m = PluginManifest::from_yaml_text(MINIMAL).unwrap();
        let perms = &m.agent.as_ref().unwrap().permissions;
        assert_eq!(perms[0].id, "hardware.spi");
        assert!(perms[0].required);
        assert_eq!(perms[1].id, "mission.write");
        assert!(!perms[1].required);
    }

    #[test]
    fn subprocess_spawn_allowlist_parses_and_defaults_empty() {
        // Default: absent → empty allowlist.
        let m = PluginManifest::from_yaml_text(MINIMAL).unwrap();
        assert!(m.agent.as_ref().unwrap().subprocess_spawn.is_empty());

        // Explicit: a list of basenames the plugin may process.spawn.
        let yaml = r#"
id: com.example.spawner
version: 1.0.0
compatibility:
  ados_version: ">=0.1.0"
agent:
  entrypoint: agent/py/x.py
  subprocess_spawn:
    - ffmpeg
    - v4l2-ctl
"#;
        let m = PluginManifest::from_yaml_text(yaml).unwrap();
        assert_eq!(
            m.agent.as_ref().unwrap().subprocess_spawn,
            vec!["ffmpeg".to_string(), "v4l2-ctl".to_string()]
        );
    }

    #[test]
    fn inprocess_isolation_parses() {
        let yaml = r#"
id: com.altnautica.builtin
version: 0.1.0
compatibility:
  ados_version: ">=0.1.0"
agent:
  entrypoint: pkg.mod:Class
  isolation: inprocess
"#;
        let m = PluginManifest::from_yaml_text(yaml).unwrap();
        assert_eq!(m.agent.unwrap().isolation, AgentIsolation::Inprocess);
        assert!(!PluginManifest::from_yaml_text(yaml)
            .unwrap()
            .is_subprocess_agent());
    }

    #[test]
    fn agent_runtime_defaults_to_python_when_absent() {
        // An old manifest with no runtime: field parses and is python.
        let m = PluginManifest::from_yaml_text(MINIMAL).unwrap();
        assert_eq!(m.agent.as_ref().unwrap().runtime, AgentRuntime::Python);
        assert_eq!(m.agent_runtime(), Some(AgentRuntime::Python));
    }

    #[test]
    fn agent_runtime_rust_parses() {
        let yaml = r#"
id: com.example.rustplug
version: 1.0.0
compatibility:
  ados_version: ">=0.1.0"
agent:
  entrypoint: agent/bin/com.example.rustplug
  runtime: rust
"#;
        let m = PluginManifest::from_yaml_text(yaml).unwrap();
        assert_eq!(m.agent.as_ref().unwrap().runtime, AgentRuntime::Rust);
        assert_eq!(m.agent_runtime(), Some(AgentRuntime::Rust));
    }

    #[test]
    fn gcs_only_plugin_has_no_agent_runtime() {
        let yaml = r#"
id: com.example.panel
version: 0.1.0
compatibility:
  ados_version: ">=0.1.0"
gcs:
  entrypoint: gcs/dist/index.js
"#;
        let m = PluginManifest::from_yaml_text(yaml).unwrap();
        assert_eq!(m.agent_runtime(), None);
    }

    #[test]
    fn unknown_top_level_fields_are_tolerated() {
        let yaml = r#"
id: com.example.future
version: 2.0.0
schema_version: 2
author: someone
features: [a, b]
compatibility:
  ados_version: ">=0.1.0"
gcs:
  entrypoint: gcs/dist/index.js
  isolation: inline
"#;
        let m = PluginManifest::from_yaml_text(yaml).unwrap();
        assert_eq!(m.gcs.as_ref().unwrap().isolation, GcsIsolation::Inline);
        assert!(!m.is_subprocess_agent());
    }

    #[test]
    fn non_mapping_top_level_is_rejected() {
        let err = PluginManifest::from_yaml_text("- a\n- b").unwrap_err();
        assert!(err.0.contains("top-level must be a mapping"), "{}", err.0);
    }

    /// A manifest built from YAML with the given id, version and agent
    /// entrypoint. Values are emitted as JSON-quoted scalars (valid YAML) so a
    /// test can carry a newline or quote through the parser intact.
    fn manifest_with(id: &str, version: &str, entrypoint: &str) -> String {
        let q = |s: &str| serde_json::to_string(s).unwrap();
        format!(
            "id: {}\nversion: {}\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: {}\n  runtime: rust\n",
            q(id),
            q(version),
            q(entrypoint)
        )
    }

    #[test]
    fn ids_that_escape_the_install_dir_or_the_unit_line_are_rejected() {
        for id in [
            "../../etc",
            "/etc",
            "../../../etc/cron.d",
            "com.example.x\nExecStartPre=+/bin/sh -c id",
            "com.example.x y",
            "com.example/x",
            "Com.Example.x",
            "single",
            "com..example",
            "com.example.",
            ".com.example",
            "",
        ] {
            let err = PluginManifest::from_yaml_text(&manifest_with(id, "1.0.0", "bin/x"))
                .expect_err(&format!("id {id:?} must be rejected"));
            assert!(err.0.contains("plugin id"), "{id:?}: {}", err.0);
        }
        for id in [
            "com.example.thermal-lepton",
            "com.example.x",
            "io.ados.a1.b-2",
        ] {
            PluginManifest::from_yaml_text(&manifest_with(id, "1.0.0", "bin/x"))
                .unwrap_or_else(|e| panic!("id {id:?} must be accepted: {}", e.0));
        }
    }

    #[test]
    fn entrypoints_that_escape_or_inject_are_rejected() {
        for entrypoint in [
            "bin/x\nExecStartPre=+/bin/sh -c 'id>/root/p'",
            "bin/x\rExecStartPre=+/bin/sh",
            "bin/x\tflag",
            "bin/x --flag",
            "../bin/x",
            "bin/../../x",
            "/usr/bin/x",
            "bin//x",
            "bin/x/",
            "",
            "pkg.mod:Class\nExecStartPre=+/bin/sh",
            "pkg.mod:Class:Extra",
            "../pkg:Class",
            ":Class",
            "pkg:",
        ] {
            let err = PluginManifest::from_yaml_text(&manifest_with(
                "com.example.x",
                "1.0.0",
                entrypoint,
            ))
            .expect_err(&format!("entrypoint {entrypoint:?} must be rejected"));
            assert!(
                err.0.contains("agent.entrypoint"),
                "{entrypoint:?}: {}",
                err.0
            );
        }
        for entrypoint in [
            "agent/py/thermal.py",
            "agent/bin/com.example.rustplug",
            "bin/vision-nav",
            "gcs/plugin.bundle.js",
            "pkg.mod:Class",
            "altnautica_thermal_camera.plugin:ThermalUsbPlugin",
        ] {
            PluginManifest::from_yaml_text(&manifest_with("com.example.x", "1.0.0", entrypoint))
                .unwrap_or_else(|e| panic!("entrypoint {entrypoint:?} must be accepted: {}", e.0));
        }
    }

    #[test]
    fn the_gcs_entrypoint_is_held_to_the_same_rules() {
        let yaml = "id: com.example.panel\nversion: 0.1.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: \"../../dist/index.js\"\n";
        let err = PluginManifest::from_yaml_text(yaml).unwrap_err();
        assert!(err.0.contains("gcs.entrypoint"), "{}", err.0);
    }

    #[test]
    fn version_must_be_semver() {
        for version in [
            "1.0", "01.0.0", "1.0.0-", "1.0.0-01", "1.0.0+", "1.0.0 ", "v1.0.0", "1.0.0\n",
        ] {
            let err =
                PluginManifest::from_yaml_text(&manifest_with("com.example.x", version, "bin/x"))
                    .expect_err(&format!("version {version:?} must be rejected"));
            assert!(err.0.contains("semver"), "{version:?}: {}", err.0);
        }
        for version in [
            "0.0.0",
            "1.2.3",
            "1.0.0-rc.1",
            "1.0.0-alpha-2.0a",
            "1.0.0+build.5",
            "10.20.30-0.x+b",
        ] {
            PluginManifest::from_yaml_text(&manifest_with("com.example.x", version, "bin/x"))
                .unwrap_or_else(|e| panic!("version {version:?} must be accepted: {}", e.0));
        }
    }

    fn agent_manifest(id: &str, agent_extra: &str) -> Result<PluginManifest, ManifestError> {
        PluginManifest::from_yaml_text(&format!(
            "id: {id}\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: agent/py/x.py\n{agent_extra}"
        ))
    }

    #[test]
    fn target_profiles_default_to_drone_and_normalize_the_underscore_form() {
        let m = agent_manifest("com.example.x", "").unwrap();
        assert_eq!(m.agent.unwrap().target_profiles, vec!["drone".to_string()]);
        let m = agent_manifest(
            "com.example.x",
            "  target_profiles: [ground_station, compute, ground-station]\n",
        )
        .unwrap();
        assert_eq!(
            m.agent.unwrap().target_profiles,
            vec!["ground-station".to_string(), "compute".to_string()]
        );
        for bad in ["  target_profiles: []\n", "  target_profiles: [laptop]\n"] {
            let err = agent_manifest("com.example.x", bad).unwrap_err();
            assert!(err.0.contains("agent.target_profiles"), "{}", err.0);
        }
    }

    #[test]
    fn resource_bounds_follow_the_resource_class() {
        let over = "  resources:\n    max_ram_mb: 8192\n";
        let err = agent_manifest("com.example.x", over).unwrap_err();
        assert!(err.0.contains("max_ram_mb 8192"), "{}", err.0);
        let heavy = "  resources:\n    class: heavy\n    max_ram_mb: 16384\n    max_cpu_percent: 800\n    max_pids: 1024\n";
        let m = agent_manifest("com.example.x", heavy).unwrap();
        assert_eq!(m.agent.unwrap().resources.class, ResourceClass::Heavy);
        let err = agent_manifest(
            "com.example.x",
            "  resources:\n    class: heavy\n    max_cpu_percent: 4000\n",
        )
        .unwrap_err();
        assert!(err.0.contains("max_cpu_percent"), "{}", err.0);
        assert!(agent_manifest("com.example.x", "  resources:\n    max_pids: 0\n").is_err());
    }

    #[test]
    fn a_bin_entrypoint_needs_rust_and_a_binaries_entry() {
        let yaml = |runtime: &str, binaries: &str| {
            format!(
                "id: com.example.x\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\nagent:\n  entrypoint: \"bin:x-link\"\n  runtime: {runtime}\n{binaries}"
            )
        };
        let binaries = "  binaries:\n    x-link:\n      aarch64-linux: bin/aarch64-linux/x-link\n";
        let m = PluginManifest::from_yaml_text(&yaml("rust", binaries)).unwrap();
        assert_eq!(
            m.agent.unwrap().binary_path("x-link", "aarch64-linux"),
            Some("bin/aarch64-linux/x-link")
        );
        let err = PluginManifest::from_yaml_text(&yaml("python", binaries)).unwrap_err();
        assert!(err.0.contains("runtime: rust"), "{}", err.0);
        let err = PluginManifest::from_yaml_text(&yaml("rust", "")).unwrap_err();
        assert!(err.0.contains("agent.binaries"), "{}", err.0);
        for bad in [
            "  binaries:\n    x-link:\n      aarch64: bin/x\n",
            "  binaries:\n    x-link:\n      aarch64-linux: ../x\n",
            "  binaries:\n    X:\n      aarch64-linux: bin/x\n    x-link:\n      aarch64-linux: bin/x\n",
        ] {
            assert!(PluginManifest::from_yaml_text(&yaml("rust", bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn payloads_are_pinned_https_files_inside_the_tree() {
        let payload = |path: &str, source: &str, sha: &str, size: u64| {
            format!(
                "  payloads:\n    - path: {path}\n      source: {source}\n      sha256: {sha}\n      size_bytes: {size}\n      profiles: [ground_station]\n"
            )
        };
        let sha = "ab".repeat(32);
        let good = payload("bin/tool", "https://github.com/o/r/tool", &sha, 10);
        let m = agent_manifest("com.example.x", &good).unwrap();
        let spec = &m.agent.as_ref().unwrap().payloads[0];
        assert_eq!(spec.profiles, Some(vec!["ground-station".to_string()]));
        assert!(spec.applies_to("ground-station", "aarch64-linux"));
        assert!(!spec.applies_to("drone", "aarch64-linux"));
        assert_eq!(m.applicable_payloads("drone", "aarch64-linux").len(), 0);
        for bad in [
            payload("../escape", "https://github.com/o/r/tool", &sha, 10),
            payload("bin/tool", "http://github.com/o/r/tool", &sha, 10),
            payload(
                "bin/tool",
                "https://github.com/o/r/tool",
                &sha.to_uppercase(),
                10,
            ),
            payload("bin/tool", "https://github.com/o/r/tool", "abc", 10),
            payload("bin/tool", "https://github.com/o/r/tool", &sha, 0),
            payload(
                "bin/tool",
                "https://github.com/o/r/tool",
                &sha,
                PAYLOAD_MAX_BYTES + 1,
            ),
        ] {
            assert!(agent_manifest("com.example.x", &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn declared_capabilities_and_shared_topics_live_under_the_id_leaf() {
        let ok = "  declared_capabilities:\n    - id: plugin.mapper.world.read\n      risk: low\n  contributes:\n    shared_topics:\n      - topic: plugin.mapper.occupancy\n        subscribe_capability: plugin.mapper.world.read\n      - topic: plugin.mapper.pose\n        subscribe_capability: telemetry.read\n";
        let m = agent_manifest("com.example.mapper", ok).unwrap();
        let agent = m.agent.unwrap();
        assert_eq!(agent.shared_topics().len(), 2);
        assert_eq!(agent.declared_capabilities[0].risk, "low");
        // The same block under another id is outside that plugin's namespace.
        let err = agent_manifest("com.example.other", ok).unwrap_err();
        assert!(err.0.contains("plugin.other."), "{}", err.0);
        // A subscribe capability must be known or declared by the owner.
        let unknown = "  contributes:\n    shared_topics:\n      - topic: plugin.mapper.pose\n        subscribe_capability: plugin.mapper.nope\n";
        let err = agent_manifest("com.example.mapper", unknown).unwrap_err();
        assert!(err.0.contains("subscribe_capability"), "{}", err.0);
        let bad_risk =
            "  declared_capabilities:\n    - id: plugin.mapper.world.read\n      risk: extreme\n";
        assert!(agent_manifest("com.example.mapper", bad_risk).is_err());
    }

    fn gcs_manifest(gcs_extra: &str) -> Result<PluginManifest, ManifestError> {
        PluginManifest::from_yaml_text(&format!(
            "id: com.example.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/index.mjs\n{gcs_extra}"
        ))
    }

    #[test]
    fn gcs_isolation_is_iframe_or_inline() {
        assert_eq!(
            gcs_manifest("").unwrap().gcs.unwrap().isolation,
            GcsIsolation::Iframe
        );
        assert!(gcs_manifest("  isolation: worker\n").is_err());
    }

    #[test]
    fn agent_pages_and_node_surfaces_are_validated() {
        let ok = "  contributes:\n    agent_pages:\n      - {id: world, title: World, section: videoVision, profile: [drone]}\n      - {id: world-setup, title: World setup, setup_for: world}\n    node_surfaces:\n      - {id: overview, title: Compute, profile: [workstation, compute], group: status, order: 10}\n";
        let gcs = gcs_manifest(ok).unwrap().gcs.unwrap();
        assert_eq!(gcs.agent_pages().len(), 2);
        assert_eq!(
            gcs.node_surfaces()[0].profile,
            vec!["workstation", "compute"]
        );
        for bad in [
            // setup_for names a missing page, itself, or another setup page.
            "  contributes:\n    agent_pages:\n      - {id: a, title: A, setup_for: nope}\n",
            "  contributes:\n    agent_pages:\n      - {id: a, title: A, setup_for: a}\n",
            "  contributes:\n    agent_pages:\n      - {id: a, title: A}\n      - {id: b, title: B, setup_for: a}\n      - {id: c, title: C, setup_for: b}\n",
            // Duplicate ids, a bad id, an empty title.
            "  contributes:\n    agent_pages:\n      - {id: a, title: A}\n      - {id: a, title: B}\n",
            "  contributes:\n    agent_pages:\n      - {id: A!, title: A}\n",
            "  contributes:\n    agent_pages:\n      - {id: a, title: ''}\n",
            // A surface needs a profile and a known group.
            "  contributes:\n    node_surfaces:\n      - {id: s, title: S}\n",
            "  contributes:\n    node_surfaces:\n      - {id: s, title: S, profile: []}\n",
            "  contributes:\n    node_surfaces:\n      - {id: s, title: S, profile: [drone], group: sidebar}\n",
        ] {
            assert!(gcs_manifest(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_gcs_block_projection_carries_isolation_and_every_contribution() {
        let m = PluginManifest::from_yaml_text(
            "id: com.altnautica.panel\nversion: 1.0.0\ncompatibility:\n  ados_version: \">=0.1.0\"\ngcs:\n  entrypoint: gcs/index.mjs\n  isolation: inline\n  locales: [en]\n  contributes:\n    panels: [{id: p}]\n    settings: [{key: a}]\n    agent_pages:\n      - {id: world, title: World, profile: [ground_station]}\n    node_surfaces:\n      - {id: overview, title: Compute, profile: [compute]}\n",
        )
        .unwrap();
        let block = gcs_block_json(&m).unwrap();
        assert_eq!(block["entrypoint"], "gcs/index.mjs");
        assert_eq!(block["isolation"], "inline");
        assert_eq!(block["locales"], serde_json::json!(["en"]));
        let c = &block["contributes"];
        assert_eq!(c["panels"], serde_json::json!([{"id": "p"}]));
        assert_eq!(c["settings"], serde_json::json!([{"key": "a"}]));
        assert_eq!(c["overlays"], serde_json::json!([]));
        assert_eq!(
            c["agent_pages"],
            serde_json::json!([{"id": "world", "title": "World", "profile": ["ground-station"]}])
        );
        assert_eq!(
            c["node_surfaces"],
            serde_json::json!([{"id": "overview", "title": "Compute", "profile": ["compute"]}])
        );
        let agent_only = agent_manifest("com.example.x", "").unwrap();
        assert!(gcs_block_json(&agent_only).is_none());
    }
}
