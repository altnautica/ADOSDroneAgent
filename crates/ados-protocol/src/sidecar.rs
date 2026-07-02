//! Best-effort schema-version checking for on-disk state sidecars.
//!
//! A state sidecar is a small JSON snapshot a service drops under the run
//! directory (`/run/ados/*.json`) so the REST and heartbeat surfaces can read
//! its state without an IPC round-trip. Unlike the frozen wire contracts, a
//! sidecar is *best-effort*: a reader must tolerate a file written by an older
//! or newer agent rather than reject or crash on it.
//!
//! So the version discipline here is deliberately softer than the wire
//! contracts: when a sidecar's declared `version` does not match the value this
//! build expects, [`check_sidecar_version`] logs a warning and returns `false`
//! but the reader keeps using the data. A stale sidecar written by an older
//! agent (which had no `version` field at all, so it reads back as `0`) must
//! never brick a reader — it warns once and proceeds.
//!
//! Each sidecar's expected version is registered in
//! [`crate::contracts`] and mirrored by a per-file constant on the writer side;
//! a reader passes both to [`check_sidecar_version`] at its deserialize site.

use serde::{Deserialize, Serialize};

/// Compare a sidecar's declared schema version against the version this build
/// expects.
///
/// Returns `true` when they match. On a mismatch it logs a warning naming the
/// sidecar and both versions, then returns `false` — the caller should still
/// use the parsed data (sidecars are best-effort). This function never panics
/// and never rejects: it is a soft, self-describing drift signal, not a gate.
///
/// `got` is the version read off the file (an older file with no `version`
/// field deserializes to `0` via the `#[serde(default)]` mixin), and `ours` is
/// the version constant this build was compiled against for that sidecar.
pub fn check_sidecar_version(file: &str, got: u16, ours: u16) -> bool {
    if got != ours {
        // Fires in every consumer (tracing is a required dependency), so a
        // schema drift is never silent regardless of which crate reads the
        // sidecar; still signal the mismatch via the return value.
        tracing::warn!(
            sidecar = file,
            got,
            ours,
            "sidecar schema version mismatch; reading best-effort"
        );
        return false;
    }
    true
}

/// A minimal mixin for peeking a sidecar's schema version.
///
/// Deserializing a sidecar file into `Versioned` extracts only the `version`
/// field (serde ignores every other field) and defaults it to `0` when absent,
/// so a reader can learn a file's schema version without deserializing the whole
/// snapshot and without failing on an older, version-less file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versioned {
    /// The sidecar schema version. Defaults to `0` for a file written before the
    /// field existed.
    #[serde(default)]
    pub version: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_versions_pass() {
        assert!(check_sidecar_version("example-sidecar", 1, 1));
        assert!(check_sidecar_version("example-sidecar", 0, 0));
    }

    #[test]
    fn mismatched_versions_fail() {
        // Older file (version-less, reads back as 0) against a v1 build.
        assert!(!check_sidecar_version("example-sidecar", 0, 1));
        // Newer file than this build expects.
        assert!(!check_sidecar_version("example-sidecar", 2, 1));
    }

    #[test]
    fn versioned_defaults_to_zero_and_ignores_other_fields() {
        let empty: Versioned = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.version, 0);

        let with_extra: Versioned =
            serde_json::from_str(r#"{"version":3,"link_state":"healthy"}"#).unwrap();
        assert_eq!(with_extra.version, 3);
    }
}
