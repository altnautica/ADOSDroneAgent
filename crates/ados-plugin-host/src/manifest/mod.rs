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
//! compatibility block). The rest of the schema is checked for shape by the
//! strict conformance pass (no unknown key in any block), so the install path
//! refuses exactly what the schema-source model refuses; the shared fixture
//! corpus under `schemas/fixtures/plugin-manifest/` pins that agreement.
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

/// The node profiles a plugin half may target, in their canonical wire form
/// (the `contracts.toml` `[profiles]` list with the hyphen spelling).
pub const NODE_PROFILES: &[&str] = &["drone", "ground-station", "workstation", "compute", "visor"];

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

mod conformance;
mod gcs;
mod plugin;
mod types;
mod validate;

pub use gcs::*;
pub use plugin::*;
pub use types::*;
pub use validate::*;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod profile_tests {
    use super::{canonical_profile, NODE_PROFILES};

    #[test]
    fn node_profiles_match_the_canonical_list() {
        let want: Vec<String> = ados_protocol::contracts::PROFILES
            .iter()
            .map(|p| p.replace('_', "-"))
            .collect();
        assert_eq!(NODE_PROFILES, want.as_slice());
        assert_eq!(canonical_profile("visor"), Some("visor"));
    }
}
