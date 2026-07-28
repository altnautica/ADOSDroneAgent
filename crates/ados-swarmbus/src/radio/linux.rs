//! Raw 802.11 injection and capture over `AF_PACKET`, with the swarm filter
//! attached in the kernel.
//!
//! Two sockets on one monitor interface, mirroring what `wfb_tx` and `wfb_rx` do
//! from C, minus libpcap:
//!
//! - **Transmit** is `AF_PACKET`/`SOCK_RAW` bound to the interface with
//!   `PACKET_QDISC_BYPASS`, exactly as `vendor/wfb-ng/src/tx.cpp:213-249` does. The
//!   frame is written whole (radiotap, 802.11, payload) with one `send`.
//! - **Receive** is `AF_PACKET`/`SOCK_RAW` with `ETH_P_ALL`, carrying the classic
//!   BPF from [`crate::frame::bpf_program`]. The filter is attached **before** the
//!   bind, so the socket is never unfiltered for even one frame — attaching after
//!   binding leaves a window in which the adapter's entire video stream queues into
//!   the receive buffer.
//!
//! No libpcap, so this cross-compiles to the musl SBC target as a self-contained
//! binary; the whole Linux surface is these four syscalls plus `if_nametoindex`.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use tokio::io::unix::AsyncFd;

use crate::frame::{bpf_program, SockFilter, MAX_FRAME_LEN};

/// `PACKET_QDISC_BYPASS`, absent from the `libc` crate's constants. Skips the
/// qdisc layer on transmit, as wfb-ng does: a beacon has no business queueing
/// behind the video stream, and a queued beacon is a stale beacon.
const PACKET_QDISC_BYPASS: libc::c_int = 20;

/// The kernel's `struct sock_fprog`: a length and a pointer to the program.
#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

/// The transmit and receive halves of one monitor interface.
pub struct Radio {
    tx: OwnedFd,
    rx: AsyncFd<OwnedFd>,
    iface: String,
}

impl Radio {
    /// Open both sockets on `iface`, filtering the receive side to `fleet_id`.
    ///
    /// Every failure is reported with the interface name and the errno, because the
    /// realistic causes are all operational and all distinguishable that way: the
    /// adapter is not in monitor mode, the radio manager has not selected it yet, or
    /// the process lacks `CAP_NET_RAW`.
    ///
    /// # Panics
    ///
    /// MUST be called from inside a Tokio runtime. The receive half is an
    /// [`AsyncFd`], and registering one without a reactor panics rather than
    /// returning an error. The service always runs under `#[tokio::main]`, so this
    /// only bites a caller that opens the radio from a bare thread or a plain
    /// `#[test]` — and only once it gets past the `CAP_NET_RAW` check, which is why
    /// an unprivileged host never sees it.
    pub fn open(iface: &str, fleet_id: u16) -> io::Result<Self> {
        let ifindex = if_nametoindex(iface)?;
        let tx = open_tx(ifindex)?;
        let rx = open_rx(ifindex, fleet_id)?;
        Ok(Self {
            tx,
            rx: AsyncFd::new(rx)?,
            iface: iface.to_string(),
        })
    }

    /// The interface both sockets are bound to.
    pub fn iface(&self) -> &str {
        &self.iface
    }

    /// Inject one complete frame (radiotap, 802.11 header, payload).
    ///
    /// A full driver queue surfaces as `WouldBlock`/`ENOBUFS` rather than blocking.
    /// The caller treats that as a dropped beacon: at 2 Hz the next one is 500 ms
    /// away, and a beacon delayed behind a backed-up video queue is worse than a
    /// beacon skipped — the receiver's dead reckoning covers the gap, but it cannot
    /// undo a stale position presented as current.
    pub fn send(&self, frame: &[u8]) -> io::Result<usize> {
        // SAFETY: `frame` is a valid initialised slice; the fd is owned and open for
        // the lifetime of `self`.
        let n = unsafe {
            libc::send(
                self.tx.as_raw_fd(),
                frame.as_ptr() as *const libc::c_void,
                frame.len(),
                0,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Await one captured frame into `buf`, returning its length.
    ///
    /// Only frames the kernel filter accepted reach here, so this wakes at the
    /// beacon rate rather than at the video rate.
    pub async fn recv(&self, buf: &mut [u8; MAX_FRAME_LEN]) -> io::Result<usize> {
        loop {
            let mut guard = self.rx.readable().await?;
            match guard.try_io(|inner| {
                // SAFETY: `buf` is a valid initialised array; `inner` is an owned,
                // open fd held by the AsyncFd for the call's duration.
                let n = unsafe {
                    libc::recv(
                        inner.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        buf.len(),
                        0,
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Ok(res) => return res,
                // The readiness was spurious; wait again.
                Err(_would_block) => continue,
            }
        }
    }
}

/// Resolve an interface name to its kernel index.
fn if_nametoindex(iface: &str) -> io::Result<libc::c_uint> {
    let name = std::ffi::CString::new(iface).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "interface name has a nul byte")
    })?;
    // SAFETY: `name` is a valid nul-terminated C string for the call's duration.
    let idx = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if idx == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(idx)
}

/// Bind an `AF_PACKET` socket to an interface index.
fn bind_to_iface(fd: RawFd, ifindex: libc::c_uint, protocol: u16) -> io::Result<()> {
    let mut sll: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    sll.sll_family = libc::AF_PACKET as u16;
    sll.sll_ifindex = ifindex as i32;
    sll.sll_protocol = protocol.to_be();
    // SAFETY: `sll` is a fully-initialised sockaddr_ll and the length matches it.
    let rc = unsafe {
        libc::bind(
            fd,
            &sll as *const libc::sockaddr_ll as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Create a raw packet socket, taking ownership of the descriptor immediately so
/// every later failure path closes it.
fn raw_socket(protocol: u16) -> io::Result<OwnedFd> {
    // SAFETY: a plain socket(2) call with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, protocol.to_be() as i32) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, exclusively-owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The transmit socket: qdisc-bypassed and bound, protocol 0 (send only).
fn open_tx(ifindex: libc::c_uint) -> io::Result<OwnedFd> {
    let fd = raw_socket(0)?;
    let on: libc::c_int = 1;
    // SAFETY: `on` outlives the call and its size is passed correctly.
    let rc = unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::SOL_PACKET,
            PACKET_QDISC_BYPASS,
            &on as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        // Not fatal: the bypass is an optimisation, and a kernel without it still
        // transmits. Losing it costs queueing latency, not correctness.
        tracing::warn!(
            error = %io::Error::last_os_error(),
            "swarm_tx_qdisc_bypass_unavailable: beacons may queue behind video"
        );
    }
    bind_to_iface(fd.as_raw_fd(), ifindex, 0)?;
    Ok(fd)
}

/// The receive socket: filtered, non-blocking, then bound.
fn open_rx(ifindex: libc::c_uint, fleet_id: u16) -> io::Result<OwnedFd> {
    let fd = raw_socket(libc::ETH_P_ALL as u16)?;
    attach_filter(fd.as_raw_fd(), fleet_id)?;
    set_nonblocking(fd.as_raw_fd())?;
    // Bind LAST: an unbound packet socket receives nothing, so the filter is in
    // place before the first frame can arrive and the adapter's video stream never
    // gets a window to queue into the receive buffer.
    bind_to_iface(fd.as_raw_fd(), ifindex, libc::ETH_P_ALL as u16)?;
    Ok(fd)
}

/// Attach the swarm BPF program.
///
/// A refusal is fatal rather than a warning. An unfiltered socket on a shared
/// adapter copies the entire video stream into userspace — hundreds of packets a
/// second of pure waste on the flight computer — and every one of them lands in
/// `beacons_bad_magic`. Failing to open the bus is the honest outcome; the service
/// retries and the failure is visible in the unit's state.
fn attach_filter(fd: RawFd, fleet_id: u16) -> io::Result<()> {
    let program = bpf_program(fleet_id);
    let fprog = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };
    // SAFETY: `program` and `fprog` both outlive the call; the kernel copies the
    // program during it.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ATTACH_FILTER,
            &fprog as *const SockFprog as *const libc::c_void,
            std::mem::size_of::<SockFprog>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Put a descriptor in non-blocking mode so `AsyncFd` can drive it.
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl(2) on an owned descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sock_fprog` is read by the kernel through a raw pointer, so its layout must
    /// match the C struct exactly — a padded or reordered one makes the kernel read
    /// a bogus program length.
    #[test]
    fn the_filter_program_struct_matches_the_kernel_layout() {
        assert_eq!(
            std::mem::size_of::<SockFprog>(),
            std::mem::size_of::<u16>() + std::mem::size_of::<usize>() + padding(),
        );
        assert_eq!(
            std::mem::align_of::<SockFprog>(),
            std::mem::align_of::<usize>()
        );
    }

    /// The pointer field forces the struct's alignment, so the 2-byte length is
    /// followed by padding up to a pointer boundary. Naming it keeps the size
    /// assertion above readable rather than magic.
    fn padding() -> usize {
        std::mem::align_of::<usize>() - std::mem::size_of::<u16>()
    }

    /// Asserted on `raw_os_error()`, deliberately not on `ErrorKind`. Since Rust
    /// 1.55 an errno with no stable `ErrorKind` maps to `Uncategorized`, which is
    /// unnameable in stable code and is NOT equal to `Other` — so
    /// `matches!(err.kind(), … | ErrorKind::Other | …)` silently stops matching
    /// the very errors it was written to catch. `if_nametoindex` on an absent
    /// interface yields `ENODEV`, which is exactly such an errno. This test only
    /// runs on Linux, so a host-only workspace never executes it and CI is the
    /// first place the mismatch shows.
    #[test]
    fn an_absent_interface_is_reported_rather_than_panicking() {
        let err = if_nametoindex("nonexistent-swarm-iface0").unwrap_err();
        let errno = err
            .raw_os_error()
            .expect("an absent interface must surface a real OS error, not a fabricated one");
        assert!(
            errno == libc::ENODEV || errno == libc::ENXIO,
            "unexpected errno {errno} for an absent interface: {err}"
        );

        // A name with an interior nul is rejected before it reaches the kernel, so
        // it carries OUR error kind and no errno at all. That absence is the
        // assertion: an errno here would mean the guard let the name through.
        let nul = if_nametoindex("wlan\0x").unwrap_err();
        assert_eq!(nul.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(nul.raw_os_error(), None);
    }

    /// `Radio::open` must fail cleanly at the syscall rather than panicking or
    /// hanging when the process lacks `CAP_NET_RAW`, and must succeed when it has
    /// it. Both outcomes are legitimate here, so the assertion is on the shape of
    /// the failure, not on which branch is taken.
    ///
    /// `#[tokio::test]`, not `#[test]`: a PRIVILEGED runner (root in a container,
    /// and the agent's own service account on a board) gets past the permission
    /// check and reaches the `AsyncFd` registration, which panics without a
    /// reactor. An unprivileged runner fails earlier and never reaches it, so a
    /// plain `#[test]` passes on CI and panics for anyone running as root.
    ///
    /// Same `raw_os_error()` discipline as above: a sandbox that refuses
    /// `AF_PACKET` outright answers `EAFNOSUPPORT`, another errno with no stable
    /// `ErrorKind`.
    #[tokio::test]
    async fn opening_the_radio_either_succeeds_or_fails_with_a_real_errno() {
        let Err(e) = Radio::open("lo", 1) else {
            // Privileged: the sockets opened. `lo` is not a monitor interface, but
            // AF_PACKET binds to any interface, so this is the expected root path.
            return;
        };
        let errno = e
            .raw_os_error()
            .expect("a refused raw socket must surface a real OS error");
        assert!(
            errno == libc::EPERM
                || errno == libc::EACCES
                || errno == libc::EAFNOSUPPORT
                || errno == libc::ENODEV,
            "unexpected errno {errno} opening a raw packet socket: {e}"
        );
    }
}
