//! Durable local logging and telemetry store for the agent.
//!
//! The agent records logs from every process, telemetry history, discrete
//! events, and hardware samples into one WAL-mode SQLite database that survives
//! reboots and is reachable when the network is down. This daemon is the sole
//! writer to that store; every other reader connects read-only.
//!
//! This crate carries the storage layer ([`db`]), the ingest socket
//! ([`ingest`]), the single-writer store loop ([`writer`]), the daemon
//! lifecycle ([`daemon`]), the hardware collector ([`hw`]), the seam taps
//! ([`taps`]) that consume the agent's frozen IPC seams, the retention
//! maintenance ([`retention`]) the writer runs to keep the store bounded, and
//! the read surface ([`query`]) — one axum `/v1` Router served on the trusted
//! Unix query socket and the LAN TCP port — plus a re-export of the shared wire
//! contracts. The binary is functional but ships dark (no systemd unit enabled)
//! until the install layer wires it.

pub mod daemon;
pub mod db;
pub mod hw;
pub mod ingest;
pub mod query;
pub mod retention;
pub mod taps;
pub mod writer;

/// The shared wire contracts: versioned ingest frames, the read-API envelope,
/// and the secret-field redaction applied at ingest.
pub use ados_protocol::logd as wire;

/// Canonical runtime paths. The store lives under `/var/ados` (persistent); the
/// sockets live under `/run/ados` (tmpfs). The TCP port serves the LAN plane.
pub mod paths {
    /// On-disk store path.
    pub const DB_PATH: &str = "/var/ados/logd/logs.db";
    /// Ingest socket: producers write framed msgpack here (trusted, on-box).
    pub const INGEST_SOCKET: &str = "/run/ados/logd.sock";
    /// Query socket: the trusted local read plane (CLI, on-box readers).
    pub const QUERY_SOCKET: &str = "/run/ados/logd-query.sock";
    /// TCP port for the LAN read plane (authenticated, rate-limited).
    pub const QUERY_TCP_PORT: u16 = 8090;

    /// The runtime socket directory. Honours `ADOS_RUN_DIR` — the same override
    /// every other service resolves its `/run/ados` sockets under — else the
    /// `/run/ados` default.
    fn run_dir() -> String {
        std::env::var("ADOS_RUN_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/run/ados".to_string())
    }

    /// Resolve the on-disk store path. Honours `ADOS_LOGD_DB` (an absolute
    /// override a rootless / `$HOME`-rooted install — e.g. the macOS workstation
    /// — sets so the store lives under a writable home instead of the root-owned
    /// `/var/ados`), else the `DB_PATH` default.
    pub fn db_path() -> String {
        std::env::var("ADOS_LOGD_DB")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DB_PATH.to_string())
    }

    /// The ingest socket, resolved under `ADOS_RUN_DIR` (else `/run/ados`).
    pub fn ingest_socket() -> String {
        format!("{}/logd.sock", run_dir().trim_end_matches('/'))
    }

    /// The query socket, resolved under `ADOS_RUN_DIR` (else `/run/ados`).
    pub fn query_socket() -> String {
        format!("{}/logd-query.sock", run_dir().trim_end_matches('/'))
    }
}

/// Hand a freshly-bound socket to the `ados` group so a non-root operator in that
/// group can reach the trusted local plane. The bind sets the mode to `0o660`,
/// which only grants the group once the group actually owns the file — without
/// this the socket stays owned by the daemon's group (root) and a group member
/// falls in "other" with no access. Best-effort: the installer creates the group,
/// and when it is absent (a dev host) this is a quiet no-op so bring-up stays
/// automatic. Linux-only; a stub elsewhere.
#[cfg(target_os = "linux")]
pub(crate) fn set_ados_group(path: &std::path::Path) {
    set_socket_group(path, "ados");
}

/// The testable core of [`set_ados_group`], parameterized over the group name so
/// a test can drive the absent-group path deterministically.
#[cfg(target_os = "linux")]
fn set_socket_group(path: &std::path::Path, group: &str) {
    match nix::unistd::Group::from_name(group) {
        Ok(Some(g)) => {
            if let Err(err) = nix::unistd::chown(path, None, Some(g.gid)) {
                tracing::debug!(error = %err, path = %path.display(), group, "chgrp socket failed");
            }
        }
        Ok(None) => {
            tracing::debug!(group, "group not present; leaving socket group as-is");
        }
        Err(err) => {
            tracing::debug!(error = %err, group, "resolving group failed");
        }
    }
}

/// Non-Linux stub: socket group ownership is a Linux-only concern. Unused on a
/// dev host (the call sites are themselves Linux-gated), hence the allow.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
pub(crate) fn set_ados_group(_path: &std::path::Path) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::set_socket_group;
    use std::io::Write;

    #[test]
    fn set_socket_group_is_a_noop_for_an_absent_group() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"x").unwrap();
        let path = f.path().to_path_buf();
        // A group that cannot exist must not panic and must leave the file intact.
        set_socket_group(&path, "definitely-not-a-real-group-xyzzy");
        assert!(path.exists());
    }
}
