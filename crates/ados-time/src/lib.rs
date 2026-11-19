//! ADOS time for the agent's services.
//!
//! The clock itself ([`Clock`], [`SimFeed`], the `simclock.v1` wire) lives in
//! `ados_protocol::simclock`, so plugins reach it through the SDK. This crate
//! adds what only the agent does: picking a service's clock from the node
//! config ([`from_config`]), the UDP port each service follows the simulator
//! on ([`FeedPeer`]), and the local relay plugins subscribe to ([`relay`]).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use ados_config::simulation::SimulationGate;

pub use ados_protocol::simclock::{
    steady, wire, Clock, Interval, SimFeed, SimInstant, WallInstant, CLOCK_FEED_ENV, STALE_AFTER,
};

/// The UDP port the sim-clock feed is received on when `clock_feed` is unset.
pub const DEFAULT_FEED_PORT: u16 = 14650;

/// Which agent process receives the simulator's feed. Each binds its own
/// port, offset from `simulation.clock_feed`; the simulator sends every tick
/// to each peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedPeer {
    /// The pilot, on `clock_feed` itself.
    Pilot,
    /// The log store, on `clock_feed` + 1.
    Logd,
    /// The plugin relay ([`relay`]), on `clock_feed` + 2.
    Plugins,
    /// The MAVLink router's control lanes, on `clock_feed` + 3.
    Router,
}

impl FeedPeer {
    pub fn addr(self, base: SocketAddr) -> SocketAddr {
        let off = match self {
            FeedPeer::Pilot => 0,
            FeedPeer::Logd => 1,
            FeedPeer::Plugins => 2,
            FeedPeer::Router => 3,
        };
        SocketAddr::new(base.ip(), base.port().wrapping_add(off))
    }
}

/// The feed base address: `simulation.clock_feed`, else loopback
/// [`DEFAULT_FEED_PORT`].
pub fn feed_base(gate: &SimulationGate) -> SocketAddr {
    gate.clock_feed
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], DEFAULT_FEED_PORT)))
}

/// A service's clock: sim time only when `simulation.enabled` and
/// `simulation.clock: sim`, binding `peer`'s feed port (needs a Tokio
/// runtime); wall otherwise.
pub fn from_config(gate: &SimulationGate, peer: FeedPeer) -> std::io::Result<Clock> {
    if !gate.sim_clock() {
        return Ok(Clock::wall());
    }
    Ok(Clock::Sim(SimFeed::listen(peer.addr(feed_base(gate)))?))
}

/// The UDP address the plugin relay takes the simulator's ticks on: `None`
/// unless the node runs on the simulator's clock.
pub fn plugin_feed(gate: &SimulationGate) -> Option<SocketAddr> {
    gate.sim_clock()
        .then(|| FeedPeer::Plugins.addr(feed_base(gate)))
}

/// The relay's socket under the agent run dir (`ADOS_CLOCK_FEED`). It sits
/// in a directory of its own so the plugin host can bind that directory into
/// a plugin's sandbox before the socket exists.
pub fn relay_socket(run_dir: &Path) -> PathBuf {
    run_dir.join("simclock").join("relay.sock")
}

/// A clock that follows the plugin relay under `run_dir` on a node running
/// on the simulator's clock, wall otherwise (an agent service that ticks with
/// its plugins, such as the plugin host). Needs a Tokio runtime.
pub fn relay_clock(gate: &SimulationGate, run_dir: &Path) -> Clock {
    match plugin_feed(gate) {
        Some(_) => Clock::Sim(SimFeed::follow_relay(relay_socket(run_dir))),
        None => Clock::wall(),
    }
}

/// At most this many followers are relayed to; one past it is refused.
pub const MAX_FOLLOWERS: usize = 64;

/// The plugin relay: takes the simulator's `TICK`s on UDP `listen` and
/// writes each valid one, as its 32 bytes, to every follower connected to
/// the Unix stream socket at `socket` (mode 0666: every plugin user may
/// follow). A follower that cannot take a tick at once is dropped; it
/// reconnects. Runs until a socket fails.
pub async fn relay(listen: SocketAddr, socket: PathBuf) -> std::io::Result<()> {
    let udp = tokio::net::UdpSocket::bind(listen).await?;
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&socket);
    let unix = tokio::net::UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))?;
    }
    tracing::info!(%listen, socket = %socket.display(), "simclock_relay_listening");
    let mut followers: Vec<tokio::net::UnixStream> = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        tokio::select! {
            a = unix.accept() => {
                let (s, _) = a?;
                if followers.len() < MAX_FOLLOWERS {
                    followers.push(s);
                }
            }
            r = udp.recv_from(&mut buf) => {
                let (n, _) = r?;
                let d = &buf[..n];
                if wire::Tick::decode(d).is_some() {
                    // A whole tick or nothing: a partial write would misframe.
                    followers.retain(|s| matches!(s.try_write(d), Ok(w) if w == d.len()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
