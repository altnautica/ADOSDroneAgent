//! Raw 802.11 injection and capture over `AF_PACKET`, with the swarm filter
//! attached in the kernel.
//!
//! Two sockets on one monitor interface, mirroring what `wfb_tx` and `wfb_rx` do
//! from C, minus libpcap:
//!
//! - **Transmit** is `AF_PACKET`/`SOCK_RAW` bound to the interface with
//!   `PACKET_QDISC_BYPASS`, exactly as `vendor/wfb-ng/src/tx.cpp:213-249` does. The
//!   frame is written whole (radiotap, 802.11, payload) with one `send`.
//! - **Receive** is `AF_PACKET`/`SOCK_RAW`, carrying the classic BPF from
//!   [`crate::frame::bpf_program`]. The socket is created with protocol 0, which
//!   registers no packet hook, so it receives nothing until it is bound. The filter
//!   is attached, and only then is the socket bound with `ETH_P_ALL` — the bind is
//!   what registers the hook. The socket is therefore never unfiltered for even
//!   one frame; creating it with `ETH_P_ALL` would register an all-device hook at
//!   `socket()` time and queue eth0 and video frames before the filter lands.
//!
//! Both sockets are opened only on a radiotap monitor interface
//! (`/sys/class/net/<iface>/type` = 803). `AF_PACKET` binds to any interface, so
//! without that check an adapter still in managed mode would open cleanly, the
//! filter would reject everything, and every injection would fail quietly.
//!
//! No libpcap, so this cross-compiles to the musl SBC target as a self-contained
//! binary; the whole Linux surface is these few syscalls, `if_nametoindex` and one
//! sysfs read.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use tokio::io::unix::AsyncFd;

use crate::frame::{bpf_program, SockFilter, MAX_FRAME_LEN};

/// `PACKET_QDISC_BYPASS`, absent from the `libc` crate's constants. Skips the
/// qdisc layer on transmit, as wfb-ng does: a beacon has no business queueing
/// behind the video stream, and a queued beacon is a stale beacon.
const PACKET_QDISC_BYPASS: libc::c_int = 20;

/// `ARPHRD_IEEE80211_RADIOTAP`: the link type of a monitor-mode interface that
/// delivers and accepts radiotap-prefixed 802.11 frames.
const ARPHRD_IEEE80211_RADIOTAP: u32 = 803;

/// Where the kernel exposes each interface's link type.
const SYS_CLASS_NET: &str = "/sys/class/net";

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
    /// the process lacks `CAP_NET_RAW`. An interface that exists but is not a
    /// radiotap monitor interface is refused before any socket is opened, so the
    /// caller keeps retrying until the radio manager has switched it over.
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
        require_radiotap(Path::new(SYS_CLASS_NET), iface)?;
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

/// Refuse an interface whose link type is not radiotap monitor mode.
///
/// `sys_class_net` is the sysfs directory holding one entry per interface, split
/// out so the check is testable against a fixture tree.
fn require_radiotap(sys_class_net: &Path, iface: &str) -> io::Result<()> {
    let path = sys_class_net.join(iface).join("type");
    let text = std::fs::read_to_string(&path)?;
    let link_type: u32 = text.trim().parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a link type: {:?}", path.display(), text.trim()),
        )
    })?;
    if link_type != ARPHRD_IEEE80211_RADIOTAP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{iface} is not a radiotap monitor interface (link type {link_type}, \
                 want {ARPHRD_IEEE80211_RADIOTAP})"
            ),
        ));
    }
    Ok(())
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

/// The receive socket: created with no hook, filtered, non-blocking, then bound.
fn open_rx(ifindex: libc::c_uint, fleet_id: u16) -> io::Result<OwnedFd> {
    // Protocol 0: no packet hook exists yet, so nothing can queue before the
    // filter is attached.
    let fd = raw_socket(0)?;
    attach_filter(fd.as_raw_fd(), fleet_id)?;
    set_nonblocking(fd.as_raw_fd())?;
    // Binding with ETH_P_ALL registers the hook, on this interface only, with the
    // filter already in place.
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

    /// Only a radiotap monitor interface passes; a managed-mode adapter, an
    /// unreadable entry and a garbage link type are all refused.
    #[test]
    fn only_a_radiotap_link_type_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let write = |iface: &str, body: &str| {
            std::fs::create_dir_all(dir.path().join(iface)).unwrap();
            std::fs::write(dir.path().join(iface).join("type"), body).unwrap();
        };
        write("mon0", "803\n");
        write("wlan0", "1\n");
        write("junk0", "radiotap\n");

        assert!(require_radiotap(dir.path(), "mon0").is_ok());
        let managed = require_radiotap(dir.path(), "wlan0").unwrap_err();
        assert_eq!(managed.kind(), io::ErrorKind::InvalidInput);
        assert!(managed.to_string().contains("link type 1"), "{managed}");
        assert_eq!(
            require_radiotap(dir.path(), "junk0").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            require_radiotap(dir.path(), "absent0")
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ENOENT)
        );
    }

    /// `lo` exists but is not a monitor interface, so opening the bus on it is
    /// refused before any socket is created, privileged or not.
    #[tokio::test]
    async fn a_non_monitor_interface_is_refused() {
        let Err(e) = Radio::open("lo", 1) else {
            panic!("loopback must not open as a swarm radio");
        };
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
    }
}
