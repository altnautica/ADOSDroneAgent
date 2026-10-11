//! The daemon's runtime paths and the watchdog cadence.

use super::*;

/// How often the daemon pings the systemd watchdog while running. Comfortably
/// under the unit's `WatchdogSec` (a ~3x margin) so a single missed tick from a
/// brief scheduler stall does not trip a restart, but a genuinely wedged async
/// runtime does. Mirrors the sibling daemons.
pub const WATCHDOG_PING_INTERVAL: Duration = Duration::from_secs(10);

/// Resolved paths a daemon run needs: the two listener addresses it owns, the
/// pairing-state file the LAN-edge auth reads, and the vehicle-state socket the
/// status/telemetry routes read.
#[derive(Debug, Clone)]
pub struct DaemonPaths {
    /// The control socket the trusted local plane binds.
    pub control_socket: PathBuf,
    /// The TCP port the LAN plane binds.
    pub control_tcp_port: u16,
    /// The agent pairing-state file the LAN-edge auth reads AND the pairing
    /// routes read + write. Injectable so a test points it at a tempdir.
    pub pairing_path: PathBuf,
    /// The dashboard-access PIN record the `pin/*` routes read + write and the
    /// LAN-edge auth reads a session salt from. Injectable so a test points it at
    /// a tempdir.
    pub dashboard_pin_path: PathBuf,
    /// The MCP-token record the `/api/mcp/*` routes read + write and (when the
    /// accept flag is on) the LAN-edge auth reads on a would-be-401. Injectable so
    /// a test points it at a tempdir.
    pub mcp_token_path: PathBuf,
    /// The vehicle-state socket the status + telemetry routes read. Injectable so
    /// a test points it at a mock-IPC socket in a tempdir.
    pub state_socket: PathBuf,
    /// The swarm neighbour-table socket `ados-swarmbus` publishes on, read by
    /// `/api/swarm/neighbors` and `/api/status/full`. Injectable so a test points it
    /// at a mock-IPC socket in a tempdir.
    pub swarm_socket: PathBuf,
    /// The MAVLink command socket the command route writes frames to (the router
    /// forwards them to the FC). Injectable so a test points it at a mock-IPC
    /// socket in a tempdir.
    pub mavlink_socket: PathBuf,
    /// The logging-store query socket the status route reads system health from.
    /// Injectable so a test points it at a mock query server in a tempdir.
    pub logd_query_socket: PathBuf,
    /// The HAL board sidecar (`/run/ados/board.json`) the status route reads the
    /// full board dict from. Injectable for tests.
    pub board_path: PathBuf,
    /// The MAVLink router's on-disk parameter cache
    /// (`/var/lib/ados/params.json`) the `/api/params*` reads and the signing
    /// reads project. It replaced the state snapshot's `params` blob, which used
    /// to carry the whole map on every 10 Hz publish. Injectable for tests.
    pub params_path: PathBuf,
    /// The agent config (`/etc/ados/config.yaml`) the pairing-info route projects
    /// for device identity, profile, and the radio peer. Injectable for tests.
    pub config_path: PathBuf,
    /// The WFB key directory (`/etc/ados/wfb`); the presence of `tx.key`/`rx.key`
    /// is the pairing-info `radio_paired` signal. Injectable for tests.
    pub wfb_key_dir: PathBuf,
    /// The WFB bind-session sentinel (`/run/ados/bind-state.json`) the
    /// pairing-info route folds into `bind_state`. Injectable for tests.
    pub bind_state_path: PathBuf,
    /// The profile-source sentinel (`/etc/ados/profile.conf`) the pairing-info
    /// profile resolver falls back to. Injectable for tests.
    pub profile_conf_path: PathBuf,
    /// The ground-station role sentinel (`/etc/ados/mesh/role`) the pairing-info
    /// profile resolver reads for a ground station. Injectable for tests.
    pub mesh_role_path: PathBuf,
    /// The per-pair relay peer secret (`/etc/ados/secrets/relay-peer-secret`)
    /// unpair removes, the ground station delivers, and full status reports
    /// the presence of. Injectable for tests.
    pub relay_secret_path: PathBuf,
    /// The runtime directory (`$ADOS_RUN_DIR`, default `/run/ados`) the status
    /// and Wi-Fi client routes read live sidecars and sockets under.
    /// Injectable for tests.
    pub run_dir: PathBuf,
    /// Where the native service binaries are installed (`/opt/ados/bin`), swept
    /// for the runtime badge. Injectable for tests.
    pub bin_dir: PathBuf,
}

impl Default for DaemonPaths {
    fn default() -> Self {
        // The env overrides mirror the sibling daemons so a test can redirect
        // the port and the socket/pairing paths without a real `/run/ados` or
        // `/etc/ados`.
        let control_socket = std::env::var("ADOS_CONTROL_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(crate::paths::CONTROL_SOCKET));
        let control_tcp_port = std::env::var("ADOS_CONTROL_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(crate::paths::FRONT_TCP_PORT);
        let pairing_path = std::env::var("ADOS_PAIRING_JSON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(crate::auth::DEFAULT_PAIRING_PATH));
        // The dashboard-PIN record honours `ADOS_DASHBOARD_PIN_JSON` (the same
        // override the store's own `DashboardPin::new` reads), defaulting to
        // `/etc/ados/dashboard-pin.json`.
        let dashboard_pin_path = std::env::var("ADOS_DASHBOARD_PIN_JSON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(crate::dashboard_pin::DEFAULT_DASHBOARD_PIN_PATH));
        // The MCP-token record honours `ADOS_MCP_TOKEN_JSON` (the same override the
        // store's own `McpTokenStore::new` reads), defaulting to
        // `/etc/ados/mcp-token.json`.
        let mcp_token_path = std::env::var("ADOS_MCP_TOKEN_JSON")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(crate::mcp::DEFAULT_MCP_TOKEN_PATH));
        // The state socket resolves under `ADOS_RUN_DIR` (the same override the
        // Python `ados.core.ipc` honours), defaulting to `/run/ados/state.sock`.
        let state_socket = default_state_socket();
        // The MAVLink command socket resolves under `ADOS_RUN_DIR` the same way,
        // defaulting to `/run/ados/mavlink.sock`.
        let mavlink_socket = default_mavlink_socket();
        // The swarm neighbour-table socket resolves under `ADOS_RUN_DIR` the same
        // way, defaulting to `/run/ados/swarm.sock`.
        let swarm_socket = default_swarm_socket();
        // The logging-store query socket resolves under `ADOS_RUN_DIR` the same
        // way, defaulting to `/run/ados/logd-query.sock`.
        let logd_query_socket = default_logd_socket();
        // The config path honours `ADOS_CONFIG` (the same override the sibling
        // crates use), defaulting to `/etc/ados/config.yaml`.
        let config_path = std::env::var("ADOS_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(crate::config::CONFIG_YAML));
        // The WFB key dir is fixed at `/etc/ados/wfb` (no env override in the
        // Python source); injectable directly on `DaemonPaths` for tests.
        let wfb_key_dir = PathBuf::from("/etc/ados/wfb");
        // The bind-state sentinel resolves under `ADOS_RUN_DIR`, defaulting to
        // `/run/ados/bind-state.json` (matching the Python `BIND_STATE_SENTINEL`).
        let run_dir = std::env::var("ADOS_RUN_DIR").unwrap_or_else(|_| "/run/ados".to_string());
        let bind_state_path = Path::new(&run_dir).join("bind-state.json");
        // The board sidecar resolves under `ADOS_RUN_DIR`, defaulting to
        // `/run/ados/board.json` (the detector persists the HAL board dict there).
        let board_path = Path::new(&run_dir).join("board.json");
        // The router's parameter cache honours `ADOS_PARAMS_JSON`, defaulting to
        // `/var/lib/ados/params.json` — the file the router persists atomically.
        let params_path = crate::param_store::default_params_path();
        // The profile/role sentinels honour `ADOS_PROFILE_CONF` / `ADOS_MESH_ROLE`
        // (`ados_config::profile_conf_path` and `ados_config::mesh_role_path`),
        // defaulting to `/etc/ados/profile.conf` + `/etc/ados/mesh/role`.
        let profile_conf_path = ados_config::profile_conf_path();
        let mesh_role_path = ados_config::mesh_role_path();
        Self {
            control_socket,
            control_tcp_port,
            pairing_path,
            dashboard_pin_path,
            mcp_token_path,
            state_socket,
            swarm_socket,
            mavlink_socket,
            logd_query_socket,
            board_path,
            params_path,
            config_path,
            wfb_key_dir,
            bind_state_path,
            profile_conf_path,
            mesh_role_path,
            relay_secret_path: PathBuf::from(ados_protocol::relay_ticket::RELAY_SECRET_PATH),
            run_dir: PathBuf::from(run_dir),
            bin_dir: PathBuf::from(crate::runtime_mode::DEFAULT_BIN_DIR),
        }
    }
}
