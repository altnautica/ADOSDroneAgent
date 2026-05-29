//! Update poller.
//!
//! Polls the GitHub Releases API for a newer full-agent release, with ETag
//! caching, the `^v\d+\.\d+\.\d+$` full-agent tag filter, and SHA256 verify of
//! the downloaded wheel. Ports `src/ados/services/ota/checker.py` +
//! `verifier.py`. It is a oneshot poll (the daily loop calls it), so the HTTP
//! client is synchronous (`ureq`).

pub mod checker;
pub mod verifier;

pub use checker::{version_tuple, UpdateChecker, UpdateConfig, UpdateManifest, FULL_AGENT_TAG_RE};
pub use verifier::verify_sha256;
