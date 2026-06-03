//! Local-radio WFB bind orchestrator.
//!
//! Ports `services/wfb/bind_orchestrator.py` + the key-apply half of
//! `services/ground_station/pair_manager.py` to Rust, hosted in the supervisor
//! because the bind sequence `systemctl stop`s the very `ados-wfb` unit the
//! radio service runs under — it cannot live inside the radio service without
//! killing its own host. The supervisor already orchestrates systemd and
//! survives every service restart, so it owns the bind FSM.
//!
//! What stays C/shell (Rust *supervises*, never rewrites): `socat`,
//! `wfb_keygen`, `systemctl`, `pkill`, `ip`, and the upstream
//! `wfb_bind_{server,client}.sh` wrappers that own the wire protocol.
//!
//! The only legitimately-Python part is the thin FastAPI `/wfb/pair/local-bind`
//! route, which forwards the trigger to this orchestrator over the cross-process
//! seam (the supervisor control socket, wired at cutover). For out-of-process
//! liveness checks (the hop supervisor + the radio service, which both need
//! `is_bind_active()`), every transition writes an atomic sentinel at
//! [`BIND_STATE_SENTINEL`] that any process can read lock-free.

pub mod bind_event;
pub mod control;
pub mod fsm;
pub mod iface;
pub mod keys;
pub mod orchestrator;
pub mod socat;

use std::time::Duration;

// ── Upstream wfb-ng artifacts (provisioned by install.sh). ──────────────────
/// Upstream bind public key. Preflight requires it before touching the radio.
pub const UPSTREAM_BIND_KEY: &str = "/etc/bind.key";
/// Upstream bind profile config. Preflight requires it.
pub const UPSTREAM_BIND_YAML: &str = "/etc/bind.yaml";
/// Where `wfb_bind_server.sh` deposits the drone's key after a successful bind.
pub const UPSTREAM_DRONE_KEY: &str = "/etc/drone.key";
/// Where `wfb_keygen` (GS side) / `wfb_bind_client.sh` deposits the gs key.
pub const UPSTREAM_GS_KEY: &str = "/etc/gs.key";

/// Upstream shell script that owns the drone-side bind wire protocol.
pub const WFB_BIND_SERVER_SH: &str = "/usr/bin/wfb_bind_server.sh";
/// Upstream shell script that owns the gs-side bind wire protocol.
pub const WFB_BIND_CLIENT_SH: &str = "/usr/bin/wfb_bind_client.sh";

// ── systemd units. ──────────────────────────────────────────────────────────
/// wfb-ng template bind unit for the drone side.
pub const DRONE_BIND_UNIT: &str = "wifibroadcast@drone_bind.service";
/// wfb-ng template bind unit for the gs side.
pub const GS_BIND_UNIT: &str = "wifibroadcast@gs_bind.service";
/// Agent-managed normal-operation wfb TX unit (drone profile).
pub const ADOS_WFB_DRONE_UNIT: &str = "ados-wfb.service";
/// Agent-managed normal-operation wfb RX unit (gs profile).
pub const ADOS_WFB_GS_UNIT: &str = "ados-wfb-rx.service";

// ── Bind tunnel (10.5.99.x L3 over WFB). ────────────────────────────────────
/// L3 bind tunnel interface created by the drone bind profile.
pub const DRONE_BIND_IFACE: &str = "drone-bind";
/// L3 bind tunnel interface created by the gs bind profile.
pub const GS_BIND_IFACE: &str = "gs-bind";
/// Rendezvous IP the drone listens on / the gs connects to over the tunnel.
pub const DRONE_BIND_PEER_IP: &str = "10.5.99.2";
/// Rendezvous TCP port for the key-transfer tunnel.
pub const BIND_TCP_PORT: u16 = 5555;

// ── Key + config paths. ─────────────────────────────────────────────────────
/// Agent canonical TX key (drone air-side; written from `/etc/drone.key`).
pub const TX_KEY_PATH: &str = "/etc/ados/wfb/tx.key";
/// Agent canonical RX key (gs side; written from `/etc/gs.key`).
pub const RX_KEY_PATH: &str = "/etc/ados/wfb/rx.key";
/// Agent config file. Pair state is persisted under `video.wfb.*`.
pub const CONFIG_YAML: &str = "/etc/ados/config.yaml";
/// flock serialising concurrent config rewrites (GCS PUT vs CLI vs bind).
pub const CONFIG_LOCK_PATH: &str = "/run/ados/config.yaml.lock";
/// Setup-complete sentinel — captive_dns stops redirecting once it exists.
pub const SETUP_COMPLETE_PATH: &str = "/var/lib/ados/setup-complete";
/// Cross-process bind-liveness sentinel. Written atomically on every
/// transition so the radio service + hop supervisor (separate processes) can
/// answer `is_bind_active()` without an in-process singleton.
pub const BIND_STATE_SENTINEL: &str = "/run/ados/bind-state.json";

// ── Timeouts (mirror the Python constants exactly). ─────────────────────────
/// Tunnel bring-up budget (`OPENING_TUNNEL`).
pub const TUNNEL_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Iface poll cadence while waiting for the tunnel.
pub const TUNNEL_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Global wedge watchdog — a session that neither progresses nor errors. Kept
/// deliberately short so a wedged bind surfaces (and releases the single-flight
/// guard) in ~2 min instead of ~30, keeping the auto-pair retry cycle tight. A
/// peer that genuinely connects completes the handshake in seconds, so this only
/// bounds how long an unanswered rendezvous holds the radio.
pub const WAITING_PEER_WATCHDOG: Duration = Duration::from_secs(120);
/// Combined `TRANSFERRING_KEYS` + `APPLYING_KEYS` budget (one timer, by design:
/// splitting it would change which phase the GCS badge reports on timeout). Once
/// a peer rendezvous occurs the key exchange completes in seconds; a longer
/// budget only delays surfacing a peer that connected but then stalled.
pub const KEY_TRANSFER_TIMEOUT: Duration = Duration::from_secs(90);
/// Service-restart budget (`RESTARTING_SERVICES`).
pub const RESTART_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounded wait for a wedged prior session to release the single-flight guard
/// after it is asked to cancel, before a new bind gives up with `Busy`. Only
/// reached when the held session is terminal or its phase clock is stale past the
/// watchdog — a genuinely active, progressing session returns `Busy` immediately.
pub const LOCK_RECLAIM_TIMEOUT: Duration = Duration::from_secs(8);
/// Cadence of the fast regulatory-domain reconcile that runs for the whole bind
/// window. Starting the bind unit re-enters monitor mode on the self-managed
/// injection PHY, which can re-assert its EEPROM-baked country as the GLOBAL
/// cfg80211 domain on every retry; reconciling at this cadence keeps the
/// configured domain pinned so the foreign domain never lingers long enough to
/// blip the onboard management WiFi.
pub const BIND_REG_RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// Which side of the radio pair this bind runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BindRole {
    Drone,
    Gs,
}

impl BindRole {
    /// Wire string used in the session JSON + config mirror (`"drone"`/`"gs"`).
    pub fn as_str(&self) -> &'static str {
        match self {
            BindRole::Drone => "drone",
            BindRole::Gs => "gs",
        }
    }

    /// Parse the wire string; `None` for anything that is not a known role.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "drone" => Some(BindRole::Drone),
            "gs" | "ground_station" | "ground-station" => Some(BindRole::Gs),
            _ => None,
        }
    }

    /// The normal-operation wfb unit to stop before / restart after bind.
    pub fn normal_unit(&self) -> &'static str {
        match self {
            BindRole::Drone => ADOS_WFB_DRONE_UNIT,
            BindRole::Gs => ADOS_WFB_GS_UNIT,
        }
    }

    /// The wfb-ng template bind unit that brings up the L3 tunnel.
    pub fn bind_unit(&self) -> &'static str {
        match self {
            BindRole::Drone => DRONE_BIND_UNIT,
            BindRole::Gs => GS_BIND_UNIT,
        }
    }

    /// The bind tunnel interface to wait for.
    pub fn bind_iface(&self) -> &'static str {
        match self {
            BindRole::Drone => DRONE_BIND_IFACE,
            BindRole::Gs => GS_BIND_IFACE,
        }
    }

    /// The agent canonical key file this role writes (tx for drone, rx for gs).
    pub fn key_path(&self) -> &'static str {
        match self {
            BindRole::Drone => TX_KEY_PATH,
            BindRole::Gs => RX_KEY_PATH,
        }
    }

    /// The upstream key file the bind protocol deposits for this role.
    pub fn upstream_key(&self) -> &'static str {
        match self {
            BindRole::Drone => UPSTREAM_DRONE_KEY,
            BindRole::Gs => UPSTREAM_GS_KEY,
        }
    }
}

/// True when `bin` is resolvable on `$PATH` as an executable file. Replaces the
/// Python `shutil.which` preflight check (`socat` / `wfb_keygen`).
pub fn on_path(bin: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips_and_maps_units() {
        assert_eq!(BindRole::parse("drone"), Some(BindRole::Drone));
        assert_eq!(BindRole::parse("gs"), Some(BindRole::Gs));
        assert_eq!(BindRole::parse("ground_station"), Some(BindRole::Gs));
        assert_eq!(BindRole::parse("nope"), None);
        assert_eq!(BindRole::Drone.as_str(), "drone");
        assert_eq!(BindRole::Drone.normal_unit(), "ados-wfb.service");
        assert_eq!(BindRole::Gs.normal_unit(), "ados-wfb-rx.service");
        assert_eq!(
            BindRole::Drone.bind_unit(),
            "wifibroadcast@drone_bind.service"
        );
        assert_eq!(BindRole::Drone.bind_iface(), "drone-bind");
        assert_eq!(BindRole::Drone.key_path(), "/etc/ados/wfb/tx.key");
        assert_eq!(BindRole::Gs.key_path(), "/etc/ados/wfb/rx.key");
        assert_eq!(BindRole::Drone.upstream_key(), "/etc/drone.key");
    }

    #[test]
    fn bind_timeouts_surface_a_wedge_fast() {
        assert_eq!(TUNNEL_WAIT_TIMEOUT.as_secs(), 30);
        // Short windows: a wedged bind frees the single-flight guard in ~2 min,
        // not ~30, so a stuck rendezvous self-clears and auto-pair retries tightly.
        assert_eq!(WAITING_PEER_WATCHDOG.as_secs(), 120);
        assert_eq!(KEY_TRANSFER_TIMEOUT.as_secs(), 90);
        // The reclaim wait must be shorter than the watchdog, or a stale guard
        // could never be reclaimed before the wedged session frees it anyway.
        assert!(LOCK_RECLAIM_TIMEOUT < WAITING_PEER_WATCHDOG);
        assert_eq!(RESTART_TIMEOUT.as_secs(), 60);
        assert_eq!(BIND_TCP_PORT, 5555);
    }
}
