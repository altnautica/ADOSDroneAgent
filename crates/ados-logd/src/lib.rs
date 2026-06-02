//! Durable local logging and telemetry store for the agent.
//!
//! The agent records logs from every process, telemetry history, discrete
//! events, and hardware samples into one WAL-mode SQLite database that survives
//! reboots and is reachable when the network is down. This daemon is the sole
//! writer to that store; every other reader connects read-only.
//!
//! This crate carries the storage layer ([`db`]), the ingest socket
//! ([`ingest`]), the single-writer store loop ([`writer`]), and the daemon
//! lifecycle ([`daemon`]), plus a re-export of the shared wire contracts. The
//! hardware collector and the query API land in later chunks; the binary is
//! functional but ships dark (no systemd unit) until the install layer wires it.

pub mod daemon;
pub mod db;
pub mod ingest;
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
}
