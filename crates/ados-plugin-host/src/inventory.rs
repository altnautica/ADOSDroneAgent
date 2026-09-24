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
//! A manifest that cannot be read, or that declares a value that is not a
//! contract version, resolves to [`UNKNOWN_CONTRACT_VERSION`]: it is listed,
//! but it never matches a peer's version.

use crate::manifest::PluginManifest;
use crate::state::load_state;
use crate::supervisor::Paths;

/// The contract version assumed for a plugin whose manifest declares none.
///
/// Absence is treated as version 1 rather than unknown: the first shipped
/// contract carried no explicit field, so a manifest without one is that
/// contract, not an unversioned one.
pub const DEFAULT_CONTRACT_VERSION: u16 = 1;

/// The contract version advertised when it cannot be determined: the manifest
/// is unreadable, or `extra.contract_version` is present but not an integer in
/// `1..=u16::MAX`. No real contract uses it, so a peer comparing versions sees
/// a mismatch rather than a false agreement on version 1.
pub const UNKNOWN_CONTRACT_VERSION: u16 = 0;

/// Read the contract version a manifest declares under `extra.contract_version`.
///
/// The top-level `extra:` block is a tolerant free-form map (the live schema
/// forbids extras elsewhere), so it lands in the manifest's `other` catch-all.
/// An absent value resolves to [`DEFAULT_CONTRACT_VERSION`]; a present value
/// that is not an integer in `1..=u16::MAX` (a string `"2"`, `70000`, `0`)
/// resolves to [`UNKNOWN_CONTRACT_VERSION`].
pub fn manifest_contract_version(manifest: &PluginManifest) -> u16 {
    let Some(value) = manifest
        .other
        .get("extra")
        .and_then(|extra| extra.get("contract_version"))
    else {
        return DEFAULT_CONTRACT_VERSION;
    };
    value
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .filter(|&n| n != UNKNOWN_CONTRACT_VERSION)
        .unwrap_or(UNKNOWN_CONTRACT_VERSION)
}

/// The installed plugins and each one's contract version, read from the plugin
/// state file plus each install's own `manifest.yaml`.
///
/// A plugin whose manifest cannot be read or parsed still appears, at
/// [`UNKNOWN_CONTRACT_VERSION`]: the inventory's job is to let a peer notice a
/// mismatch, and a plugin silently absent from it (or reported at a version it
/// may not speak) is the failure this exists to prevent. The list is unsorted;
/// the identity builder sorts it for a stable wire form.
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
                .unwrap_or(UNKNOWN_CONTRACT_VERSION);
            (install.plugin_id, contract)
        })
        .collect()
}

/// The installed inventory read from the production default paths.
pub fn installed_contract_versions_default() -> Vec<(String, u16)> {
    installed_contract_versions(&Paths::from_env())
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

    #[test]
    fn a_present_but_invalid_contract_version_is_unknown_not_one() {
        for bad in ["\"2\"", "70000", "0", "-1", "1.5"] {
            let text = CONTRACT_MANIFEST
                .replace("contract_version: 4", &format!("contract_version: {bad}"));
            let m = PluginManifest::from_yaml_text(&text).unwrap();
            assert_eq!(
                manifest_contract_version(&m),
                UNKNOWN_CONTRACT_VERSION,
                "{bad}"
            );
        }
    }

    #[test]
    fn an_unreadable_manifest_is_listed_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            install_dir: dir.path().join("plugins"),
            unit_dir: dir.path().join("units"),
            state_path: dir.path().join("state/plugin-state.json"),
            log_dir: dir.path().join("logs"),
            control_dir: dir.path().join("plugin-host"),
            loopback_guard_state: dir.path().join("plugin-loopback-guard.json"),
            socket_dir: dir.path().join("sockets"),
            token_secret: dir.path().join("secrets/plugin-token-secret"),
            runner: dir.path().join("bin/ados-plugin-runner"),
            run_dir: dir.path().join("run"),
        };
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::write(
            &paths.state_path,
            r#"{"schema":1,"installs":[{"plugin_id":"com.example.gone","version":"1.0.0","source":"local_file","manifest_hash":"x","status":"enabled","installed_at":0}]}"#,
        )
        .unwrap();
        let listed = installed_contract_versions(&paths);
        assert_eq!(
            listed,
            vec![("com.example.gone".to_string(), UNKNOWN_CONTRACT_VERSION)]
        );
    }
}
