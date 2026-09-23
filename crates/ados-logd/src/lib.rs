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

/// Whether the store runs at all — the `logging.store.enabled` gate, shared with
/// the installer (which decides whether to enable the unit) and the storage
/// diagnostic (which must say "off" rather than "broken").
pub use ados_config::log_store as gate;

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

    /// The runtime dir the tap sockets + JSON sidecars live under, resolved under
    /// `ADOS_RUN_DIR` (else `/run/ados`). On a `$HOME`-rooted install (the macOS
    /// workstation) this is `~/.ados/run`, where the other services actually write
    /// their `state.sock`/`mavlink.sock`/sidecars — so the taps read the real
    /// runtime dir instead of a nonexistent `/run/ados`.
    pub fn tap_root() -> String {
        run_dir().trim_end_matches('/').to_string()
    }

    /// The vehicle-state stream socket, resolved under `ADOS_RUN_DIR`.
    pub fn state_socket() -> String {
        format!("{}/state.sock", tap_root())
    }

    /// The raw-frame broadcast socket, resolved under `ADOS_RUN_DIR`.
    pub fn mavlink_socket() -> String {
        format!("{}/mavlink.sock", tap_root())
    }
}
