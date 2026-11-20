//! The presence watcher: while the box stands by, hear the holder.
//!
//! The receive generation runs one extra `wfb_rx -p 1` keyed to the box's own
//! ground `link_id` (slot 0) while the transmitters are stopped, decoding onto
//! [`watch_port`]. Every ground of the fleet transmits on that link, so what
//! arrives there in standby is the holding phone: its presence beacons every
//! 10 s and its HopAcks. This task, bound once for the service, verifies each
//! frame and stamps the holder's beacons; after [`super::HOLDER_STALE`] with
//! none the box transmits again.

use std::time::Duration;

use ados_radio::hop::{parse_presence_beacon, verify_hop_packet};
use tokio::net::UdpSocket;
use tokio::time::Instant;

use super::{ops, Uplink};
use crate::presence::resolve_pair_key;

/// How often the holder timer is checked.
const TICK: Duration = Duration::from_secs(1);

/// How long to wait before re-binding a port another process holds.
const REBIND: Duration = Duration::from_secs(5);

/// The loopback port the standby watcher's receiver decodes onto: the control
/// egress of the ground's own slot. Drone slots start at 1, so it is free.
pub const fn watch_port() -> u16 {
    crate::wfb_rx::control_rx_port(ados_radio::config::SLOT_GROUND)
}

/// What one decoded frame on the ground link was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroundFrame {
    /// A presence beacon from a ground with this device id.
    Beacon(String),
    /// A verified HopAck.
    HopAck,
    /// Anything that did not verify.
    Unverified,
}

/// Verify and classify one datagram from the watcher's receiver.
pub fn classify(frame: &[u8], pair_key: &[u8; 32]) -> GroundFrame {
    if let Some(peer) = parse_presence_beacon(frame, pair_key) {
        if peer.role == "gs" {
            return GroundFrame::Beacon(peer.device_id);
        }
        return GroundFrame::Unverified;
    }
    if verify_hop_packet(frame, pair_key) {
        return GroundFrame::HopAck;
    }
    GroundFrame::Unverified
}

/// Run the watcher for the service's lifetime.
pub async fn run(u: &'static Uplink) {
    let sock = loop {
        match UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, watch_port())).await {
            Ok(s) => break s,
            Err(e) => {
                tracing::warn!(error = %e, port = watch_port(), "uplink_watcher_bind_failed; retrying");
                tokio::time::sleep(REBIND).await;
            }
        }
    };
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut buf = [0u8; 256];
    let mut hop_acks: u64 = 0;
    loop {
        tokio::select! {
            got = sock.recv(&mut buf) => {
                let Ok(n) = got else { continue };
                let Some(pair_key) = resolve_pair_key().0 else { continue };
                match classify(&buf[..n], &pair_key) {
                    GroundFrame::Beacon(id) => {
                        if !u.note_beacon(&id, Instant::now()) {
                            tracing::warn!(device_id = %id, "uplink_watcher_foreign_ground: a ground other than the holder is transmitting");
                        }
                    }
                    GroundFrame::HopAck => {
                        hop_acks += 1;
                        tracing::debug!(hop_acks, "uplink_watcher_hop_ack");
                    }
                    GroundFrame::Unverified => {}
                }
            }
            _ = tick.tick() => {
                ops::fall_back_if_absent(u, Instant::now()).await;
            }
        }
    }
}
