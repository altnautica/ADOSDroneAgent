//! ADOS time for the agent's services.
//!
//! The clock itself ([`Clock`], [`SimFeed`], the `simclock.v1` wire) lives in
//! `ados_protocol::simclock`, so plugins reach it through the SDK. This crate
//! adds what only the agent does: picking a service's clock from the node
//! config ([`from_config`]), the UDP port each service follows the simulator
//! on ([`FeedPeer`]), the local relay plugins subscribe to ([`relay`]) and,
//! with `simulation.clock_mode: lockstep`, the barrier that acknowledges each
//! of the simulator's grants once every participant is done ([`barrier`],
//! [`participant`]).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ados_config::simulation::{ClockMode, SimulationGate};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

pub mod barrier;

pub use ados_protocol::simclock::{
    steady, wire, Clock, Interval, Participant, SimFeed, SimInstant, WallInstant, CLOCK_FEED_ENV,
    PARTICIPANTS_SOCKET, STALE_AFTER,
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
    /// The plugin relay ([`relay`]), on `clock_feed` + 2. Under lockstep the
    /// relay is also the barrier, answering each grant from this port.
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

/// The lockstep barrier's participants socket, beside the relay's (inside
/// the same sandbox bind).
pub fn participants_socket(run_dir: &Path) -> PathBuf {
    ados_protocol::simclock::participants_socket_beside(&relay_socket(run_dir))
}

/// The participants socket when the node runs on the simulator's clock in
/// lockstep (`simulation.clock_mode: lockstep`); `None` otherwise.
pub fn lockstep_socket(gate: &SimulationGate, run_dir: &Path) -> Option<PathBuf> {
    (gate.sim_clock() && gate.clock_mode == ClockMode::Lockstep)
        .then(|| participants_socket(run_dir))
}

/// Register one lane with the barrier at `socket` (from
/// [`lockstep_socket`]): `None` off lockstep. Needs a Tokio runtime.
pub fn participant(socket: Option<&Path>) -> Option<Participant> {
    socket.map(|p| Participant::connect(p.to_path_buf()))
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

/// Bind a Unix stream socket every plugin user may connect to (mode 0666),
/// replacing a stale one.
fn bind_open(socket: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(socket);
    let unix = tokio::net::UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))?;
    Ok(unix)
}

async fn accept_on(
    l: Option<&tokio::net::UnixListener>,
) -> std::io::Result<tokio::net::UnixStream> {
    match l {
        Some(l) => l.accept().await.map(|(s, _)| s),
        None => std::future::pending().await,
    }
}

/// One participant's reports, `None` when it left.
type Report = (u64, Option<u64>);

/// Read one participant's `DONE` frames until it disconnects or sends
/// anything else.
async fn read_participant(id: u64, mut s: tokio::net::UnixStream, tx: mpsc::UnboundedSender<Report>) {
    let mut buf = [0u8; wire::DONE_LEN];
    while s.read_exact(&mut buf).await.is_ok() {
        match wire::Done::decode(&buf) {
            Some(d) => {
                if tx.send((id, Some(d.done_ns))).is_err() {
                    return;
                }
            }
            None => break,
        }
    }
    let _ = tx.send((id, None));
}

/// The plugin relay: takes the simulator's `TICK`s on UDP `listen` and
/// writes each valid one, as its 32 bytes, to every follower connected to
/// the Unix stream socket at `socket` (mode 0666: every plugin user may
/// follow). A follower that cannot take a tick at once is dropped; it
/// reconnects.
///
/// With `participants` (lockstep), the relay is also the barrier
/// ([`barrier::Barrier`]): lanes register on that socket and report `DONE`,
/// and each grant's `ACK` goes back to the tick's sender from `listen`. Runs
/// until a socket fails.
pub async fn relay(
    listen: SocketAddr,
    socket: PathBuf,
    participants: Option<PathBuf>,
) -> std::io::Result<()> {
    let udp = tokio::net::UdpSocket::bind(listen).await?;
    let unix = bind_open(&socket)?;
    let parts = participants.as_deref().map(bind_open).transpose()?;
    tracing::info!(%listen, socket = %socket.display(), lockstep = parts.is_some(), "simclock_relay_listening");
    let (report_tx, mut reports) = mpsc::unbounded_channel::<Report>();
    let mut barrier = barrier::Barrier::default();
    let mut followers: Vec<tokio::net::UnixStream> = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let release = tokio::select! {
            a = unix.accept() => {
                let (s, _) = a?;
                if followers.len() < MAX_FOLLOWERS {
                    followers.push(s);
                }
                None
            }
            a = accept_on(parts.as_ref()) => {
                let s = a?;
                match barrier.join() {
                    Some(id) => {
                        tracing::info!(id, parts = barrier.parts(), "simclock_participant_joined");
                        tokio::spawn(read_participant(id, s, report_tx.clone()));
                    }
                    None => tracing::warn!("simclock_participants_full"),
                }
                None
            }
            Some((id, report)) = reports.recv() => match report {
                Some(done_ns) => barrier.done(id, done_ns, Instant::now()),
                None => {
                    let r = barrier.leave(id, Instant::now());
                    tracing::info!(id, parts = barrier.parts(), "simclock_participant_left");
                    r
                }
            },
            r = udp.recv_from(&mut buf) => {
                let (n, from) = r?;
                let d = &buf[..n];
                match wire::Tick::decode(d) {
                    Some(tick) => {
                        // A whole tick or nothing: a partial write would misframe.
                        followers.retain(|s| matches!(s.try_write(d), Ok(w) if w == d.len()));
                        if parts.is_some() {
                            barrier.tick(&tick, from, Instant::now())
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            }
        };
        if let Some((ack, to)) = release {
            if let Err(e) = udp.send_to(&ack.encode(), to).await {
                tracing::warn!(error = %e, %to, "simclock_ack_send_failed");
            }
        }
    }
}

#[cfg(test)]
mod tests;
