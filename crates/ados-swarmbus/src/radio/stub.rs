//! The non-Linux [`Radio`]: a compiling stub that refuses to open.
//!
//! Raw 802.11 injection is `AF_PACKET` plus a kernel BPF, both Linux-only, while the
//! workspace is developed on macOS. Everything above this seam — the beacon codec,
//! the cipher, the frame layout, the BPF program bytes, the neighbour table, the
//! dead reckoning, the jitter schedule and the published payload — is
//! platform-independent and fully tested off-target. Only the four syscalls live
//! behind it.
//!
//! [`Radio::open`] returns `Unsupported` rather than pretending: a silent
//! loopback fake would let the daemon report a healthy bus on a host that cannot
//! possibly have one, which is the failure mode the honesty rules exist to prevent.

use std::io;

use crate::frame::MAX_FRAME_LEN;

/// The transmit and receive halves of one monitor interface. Never constructed off
/// Linux.
#[derive(Debug)]
pub struct Radio {
    /// Never populated; the field keeps the type's shape identical to the Linux one
    /// so a signature drift between the two is a compile error.
    iface: String,
}

impl Radio {
    /// Always fails off Linux.
    pub fn open(iface: &str, _fleet_id: u16) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("raw 802.11 injection on {iface} requires Linux AF_PACKET"),
        ))
    }

    /// The interface both sockets are bound to.
    pub fn iface(&self) -> &str {
        &self.iface
    }

    /// Unreachable: no [`Radio`] can be constructed off Linux.
    pub fn send(&self, _frame: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }

    /// Unreachable: no [`Radio`] can be constructed off Linux.
    pub async fn recv(&self, _buf: &mut [u8; MAX_FRAME_LEN]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stub must refuse loudly and name the reason, not fabricate a bus.
    #[test]
    fn opening_off_linux_is_unsupported_and_says_why() {
        let err = Radio::open("wlan1", 1).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        let msg = err.to_string();
        assert!(
            msg.contains("wlan1"),
            "the message names the interface: {msg}"
        );
        assert!(msg.contains("Linux"), "the message names the reason: {msg}");
    }
}
