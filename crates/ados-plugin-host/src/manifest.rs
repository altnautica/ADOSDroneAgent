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

use serde::Deserialize;

use crate::errors::ManifestError;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GcsIsolation {
    #[default]
    Iframe,
    Worker,
    Inline,
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

/// Hard resource limits the generated systemd unit applies. Defaults match the
/// Pydantic `ResourceLimits` (96 MiB / 25% CPU / 12 pids).
#[derive(Debug, Clone, Deserialize)]
pub struct ResourceLimits {
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
            max_ram_mb: default_max_ram_mb(),
            max_cpu_percent: default_max_cpu_percent(),
            max_pids: default_max_pids(),
        }
    }
}

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
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_norway::Value>,
}

/// GCS-half manifest block. Only `entrypoint` + `isolation` + `permissions`
/// are read by the controller.
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

    /// Enforce the identity and path rules the lifecycle relies on, matching the
    /// Pydantic model's validators:
    ///
    /// * `id` is reverse-DNS: `^[a-z0-9]+(\.[a-z0-9-]+)+$`. It becomes a
    ///   directory name under the install dir and a token in unit text, so it
    ///   can never be absolute, contain `/` or `..`, or carry whitespace.
    /// * `version` is semver 2.0.
    /// * each entrypoint is a relative POSIX path over `[A-Za-z0-9._/-]` with no
    ///   empty or `..` segment, or a `module:Class` reference.
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
            validate_entrypoint("agent.entrypoint", &agent.entrypoint)?;
        }
        if let Some(gcs) = &self.gcs {
            validate_entrypoint("gcs.entrypoint", &gcs.entrypoint)?;
        }
        Ok(())
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
}
