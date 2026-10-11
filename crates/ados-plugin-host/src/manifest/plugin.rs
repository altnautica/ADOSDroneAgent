//! The top-level manifest and its parse and validation entry points.

use super::*;

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
    /// author, license, extra), shape-checked and ignored by the controller.
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
        conformance::check(&value)?;
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
        if !RISK_LEVELS.contains(&self.risk.as_str()) {
            return Err(ManifestError(format!(
                "risk {:?} must be one of {RISK_LEVELS:?}",
                self.risk
            )));
        }
        if let Some(tier) = self.compatibility.min_tier {
            if !(1..=4).contains(&tier) {
                return Err(ManifestError(format!(
                    "compatibility.min_tier {tier} must be between 1 and 4"
                )));
            }
        }
        if self.agent.is_none() && self.gcs.is_none() {
            return Err(ManifestError(format!(
                "plugin {} declares neither agent nor gcs half; at least one is required",
                self.id
            )));
        }
        if let Some(agent) = &self.agent {
            self.validate_agent(agent)?;
        }
        if let Some(gcs) = &self.gcs {
            match (&gcs.entrypoint, gcs.bundle_contribution()) {
                (Some(entrypoint), _) => validate_entrypoint("gcs.entrypoint", entrypoint)?,
                (None, Some(key)) => {
                    return Err(ManifestError(format!(
                        "gcs.contributes.{key} is rendered from a bundle and needs gcs.entrypoint"
                    )));
                }
                (None, None) => {}
            }
            gcs.validate_contributions()?;
        }
        Ok(())
    }

    pub(super) fn validate_agent(&self, agent: &AgentBlock) -> Result<(), ManifestError> {
        if agent.runtime == AgentRuntime::Rust && agent.isolation == AgentIsolation::Inprocess {
            return Err(ManifestError(
                "Rust plugins have no in-process analog; rust runtime requires subprocess \
                 isolation"
                    .to_string(),
            ));
        }
        if !agent.subprocess_spawn.is_empty()
            && !agent.permissions.iter().any(|p| p.id == "process.spawn")
        {
            return Err(ManifestError(format!(
                "agent.subprocess_spawn lists {} binary path(s) but the process.spawn \
                 capability is not declared in agent.permissions; add it so the operator can \
                 review the spawn allowlist at install time",
                agent.subprocess_spawn.len()
            )));
        }
        agent.validate_vendor_attribution()?;
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
            if !RISK_LEVELS.contains(&cap.risk.as_str()) {
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
