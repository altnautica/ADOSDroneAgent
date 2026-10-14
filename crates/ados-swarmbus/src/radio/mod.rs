//! The platform seam: raw 802.11 injection and capture.
//!
//! Two backends behind one type, chosen at run time by `ADOS_SWARMBUS_TRANSPORT`:
//!
//! - unset or empty: the native radio, selected at compile time. The Linux one is
//!   four syscalls over `AF_PACKET`; the other refuses to open.
//! - `loopback`: [`loopback`], the same frame bytes over UDP to a shared-medium
//!   relay, on any OS.
//!
//! Any other value is refused rather than guessed. Everything above this seam is
//! platform-independent, which is what lets the whole wire format, cipher,
//! neighbour table and scheduling be developed and tested on a non-Linux host.

#[cfg(target_os = "linux")]
mod linux;
pub mod loopback;
mod loopback_config;
#[cfg(not(target_os = "linux"))]
mod stub;

use std::io;

use crate::frame::MAX_FRAME_LEN;
use loopback::LoopbackConfig;

#[cfg(target_os = "linux")]
use linux::Radio as NativeRadio;
#[cfg(not(target_os = "linux"))]
use stub::Radio as NativeRadio;

/// The variable that selects the backend.
pub const ENV_TRANSPORT: &str = "ADOS_SWARMBUS_TRANSPORT";

/// The interface name the service opens the loopback backend with when no
/// monitor interface is configured or reported; the backend ignores it.
pub const LOOPBACK_IFACE: &str = "loopback";

/// Which backend a [`Radio`] opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// The platform's raw 802.11 radio.
    Native,
    /// UDP to a shared-medium relay.
    Loopback(LoopbackConfig),
}

impl Transport {
    /// The transport the process environment selects.
    pub fn from_env() -> io::Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// The transport selected through `get`, which maps a variable name to its
    /// value.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> io::Result<Self> {
        match get(ENV_TRANSPORT).as_deref().map(str::trim) {
            None | Some("") => Ok(Self::Native),
            Some("loopback") => LoopbackConfig::from_lookup(get).map(Self::Loopback),
            Some(other) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{ENV_TRANSPORT}={other} is not a swarm bus transport (loopback, or unset)"
                ),
            )),
        }
    }

    /// Whether this is the loopback transport.
    pub fn is_loopback(&self) -> bool {
        matches!(self, Self::Loopback(_))
    }
}

/// The transmit and receive halves of the bus's radio.
pub enum Radio {
    Native(NativeRadio),
    Loopback(loopback::Radio),
}

impl Radio {
    /// Open the backend the environment selects on `iface`, filtering receive to
    /// `fleet_id`.
    pub fn open(iface: &str, fleet_id: u16) -> io::Result<Self> {
        Self::open_on(&Transport::from_env()?, iface, fleet_id)
    }

    /// Open `transport` on `iface`, filtering receive to `fleet_id`. The loopback
    /// backend ignores `iface`.
    pub fn open_on(transport: &Transport, iface: &str, fleet_id: u16) -> io::Result<Self> {
        match transport {
            Transport::Native => NativeRadio::open(iface, fleet_id).map(Self::Native),
            Transport::Loopback(cfg) => loopback::Radio::open(cfg, fleet_id).map(Self::Loopback),
        }
    }

    /// The interface the radio is bound to (`loopback:<node>` for loopback).
    pub fn iface(&self) -> &str {
        match self {
            Self::Native(r) => r.iface(),
            Self::Loopback(r) => r.iface(),
        }
    }

    /// Transmit one complete frame (radiotap, 802.11 header, payload).
    pub fn send(&self, frame: &[u8]) -> io::Result<usize> {
        match self {
            Self::Native(r) => r.send(frame),
            Self::Loopback(r) => r.send(frame),
        }
    }

    /// Await one frame that passed the fleet filter into `buf`, returning its
    /// length.
    pub async fn recv(&self, buf: &mut [u8; MAX_FRAME_LEN]) -> io::Result<usize> {
        match self {
            Self::Native(r) => r.recv(buf).await,
            Self::Loopback(r) => r.recv(buf).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn unset_or_empty_selects_the_native_radio() {
        assert_eq!(
            Transport::from_lookup(lookup(&[])).unwrap(),
            Transport::Native
        );
        let empty = lookup(&[(ENV_TRANSPORT, " ")]);
        assert_eq!(Transport::from_lookup(empty).unwrap(), Transport::Native);
    }

    #[test]
    fn loopback_reads_its_relay_settings() {
        let t = Transport::from_lookup(lookup(&[
            (ENV_TRANSPORT, "loopback"),
            (loopback::ENV_NODE, "drone-1-swarm"),
        ]))
        .unwrap();
        assert!(t.is_loopback());
        let Transport::Loopback(cfg) = t else {
            unreachable!()
        };
        assert_eq!(cfg.node, "drone-1-swarm");
        assert_eq!(cfg.relay_port, loopback::DEFAULT_RELAY_PORT);
    }

    #[test]
    fn an_unknown_transport_is_refused_by_name() {
        let err = Transport::from_lookup(lookup(&[(ENV_TRANSPORT, "multicast")])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("multicast"), "{err}");
    }
}
