//! The install attestation: what a GCS needs to check a plugin's files
//! against its signature without trusting the node that serves them.
//!
//! At install the controller writes `<install_dir>/<id>/.attestation.json`:
//!
//! ```json
//! {"signature": "<SIGNATURE entry text, or null when unsigned>",
//!  "files": [{"path": "manifest.yaml", "sha256": "<hex>"},
//!            {"path": "bin/tool", "sha256": "<hex>", "payload": true}]}
//! ```
//!
//! `files` lists every archive entry except `SIGNATURE`, path-sorted, with the
//! lowercase hex sha256 of its bytes: the exact list the canonical payload hash
//! is computed over, so a verifier rebuilds the signed digest from it. Payloads
//! fetched at install follow, marked `"payload": true` and carrying the sha256
//! the signed `manifest.yaml` pins; they are outside the signed digest and are
//! trusted through the manifest instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::SupervisorError;

/// The attestation file name inside a plugin's install dir.
pub const ATTESTATION_FILENAME: &str = ".attestation.json";

/// One attested file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttestedFile {
    /// Archive-relative path.
    pub path: String,
    /// Lowercase hex sha256.
    pub sha256: String,
    /// True for a file fetched at install from a manifest-pinned payload.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub payload: bool,
}

/// A plugin's install attestation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    /// The archive's `SIGNATURE` entry, verbatim; `None` for an unsigned
    /// archive.
    pub signature: Option<String>,
    pub files: Vec<AttestedFile>,
}

impl Attestation {
    /// Build from an archive's per-entry digests plus the payloads fetched for
    /// it (`(path, pinned sha256)`).
    pub fn new(
        signature: Option<String>,
        archive_digests: &BTreeMap<String, String>,
        payloads: &[(String, String)],
    ) -> Self {
        let mut files: Vec<AttestedFile> = archive_digests
            .iter()
            .map(|(path, sha256)| AttestedFile {
                path: path.clone(),
                sha256: sha256.clone(),
                payload: false,
            })
            .collect();
        files.extend(payloads.iter().map(|(path, sha256)| AttestedFile {
            path: path.clone(),
            sha256: sha256.to_ascii_lowercase(),
            payload: true,
        }));
        Attestation { signature, files }
    }

    /// The attestation path inside a plugin dir.
    pub fn path_in(plugin_dir: &Path) -> PathBuf {
        plugin_dir.join(ATTESTATION_FILENAME)
    }

    /// Write into `plugin_dir`.
    pub fn write_to(&self, plugin_dir: &Path) -> Result<(), SupervisorError> {
        let path = Self::path_in(plugin_dir);
        let body = serde_json::to_vec_pretty(self)
            .map_err(|e| SupervisorError(format!("encode attestation: {e}")))?;
        std::fs::write(&path, body)
            .map_err(|e| SupervisorError(format!("write {}: {e}", path.display())))
    }

    /// Read from `plugin_dir`.
    pub fn read_from(plugin_dir: &Path) -> Result<Attestation, SupervisorError> {
        let path = Self::path_in(plugin_dir);
        let body = std::fs::read(&path)
            .map_err(|e| SupervisorError(format!("read {}: {e}", path.display())))?;
        serde_json::from_slice(&body).map_err(|e| {
            SupervisorError(format!("attestation {} is malformed: {e}", path.display()))
        })
    }
}
