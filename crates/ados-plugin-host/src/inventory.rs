//! The installed-plugin inventory that feeds the node-identity handshake.
//!
//! A ground station and the drone it relays each run their own copy of a
//! two-halves plugin, and each half only trusts the other's wire if both speak
//! the same contract version. That version is not on the flight-control lane and
//! there is no IP between the two over the radio, so it rides the auxiliary
//! node-identity frame ([`ados_protocol::node_status::NodeIdentity`]). This
//! module is the single reader both the identity producer and the identity
//! consumer use to answer "what is installed here, and at what contract
//! version", so the two ends can never derive it differently.
//!
//! The contract version lives in each plugin manifest under
//! `extra.contract_version`; a manifest that declares none resolves to
//! [`DEFAULT_CONTRACT_VERSION`] rather than being dropped, so an older plugin
//! still advertises a comparable value instead of vanishing from the inventory.

use crate::manifest::PluginManifest;
use crate::state::load_state;
use crate::supervisor::Paths;

/// The contract version assumed for a plugin whose manifest declares none.
///
/// Absence is treated as version 1 rather than unknown: the first shipped
/// contract carried no explicit field, so a manifest without one is that
/// contract, not an unversioned one.
pub const DEFAULT_CONTRACT_VERSION: u16 = 1;

/// Read the contract version a manifest declares under `extra.contract_version`.
///
/// The top-level `extra:` block is a tolerant free-form map (the live schema
/// forbids extras elsewhere), so it lands in the manifest's `other` catch-all.
/// A missing, non-integer, or out-of-range value resolves to
/// [`DEFAULT_CONTRACT_VERSION`].
pub fn manifest_contract_version(manifest: &PluginManifest) -> u16 {
    manifest
        .other
        .get("extra")
        .and_then(|extra| extra.get("contract_version"))
        .and_then(|v| v.as_u64())
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(DEFAULT_CONTRACT_VERSION)
}

/// The installed plugins and each one's contract version, read from the plugin
/// state file plus each install's own `manifest.yaml`.
///
/// A plugin whose manifest cannot be read or parsed still appears, at
/// [`DEFAULT_CONTRACT_VERSION`]: the inventory's job is to let a peer notice a
/// mismatch, and a plugin silently absent from it is the failure this exists to
/// prevent. The list is unsorted; the identity builder sorts it for a stable
/// wire form.
pub fn installed_contract_versions(paths: &Paths) -> Vec<(String, u16)> {
    load_state(Some(&paths.state_path))
        .into_iter()
        .map(|install| {
            let manifest_path = paths
                .install_dir
                .join(&install.plugin_id)
                .join("manifest.yaml");
            let contract = std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|text| PluginManifest::from_yaml_text(&text).ok())
                .map(|m| manifest_contract_version(&m))
                .unwrap_or(DEFAULT_CONTRACT_VERSION);
            (install.plugin_id, contract)
        })
        .collect()
}

/// The installed inventory read from the production default paths.
pub fn installed_contract_versions_default() -> Vec<(String, u16)> {
    installed_contract_versions(&Paths::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT_MANIFEST: &str = r#"
schema_version: 2
id: com.example.two-halves
version: 1.0.0
name: Two Halves
risk: high
compatibility:
  ados_version: ">=0.9.0"
agent:
  runtime: rust
  entrypoint: bin/two-halves
  isolation: subprocess
extra:
  contract_version: 4
"#;

    const NO_CONTRACT_MANIFEST: &str = r#"
schema_version: 2
id: com.example.plain
version: 1.0.0
name: Plain
risk: low
compatibility:
  ados_version: ">=0.9.0"
agent:
  runtime: python
  entrypoint: module:Class
  isolation: subprocess
"#;

    #[test]
    fn reads_the_declared_contract_version() {
        let m = PluginManifest::from_yaml_text(CONTRACT_MANIFEST).unwrap();
        assert_eq!(manifest_contract_version(&m), 4);
    }

    #[test]
    fn a_manifest_without_a_contract_version_defaults_not_drops() {
        let m = PluginManifest::from_yaml_text(NO_CONTRACT_MANIFEST).unwrap();
        assert_eq!(manifest_contract_version(&m), DEFAULT_CONTRACT_VERSION);
    }
}
