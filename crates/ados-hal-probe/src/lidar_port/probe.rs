//! The RPLIDAR `GET_INFO` probe: one short exchange on a port nobody holds,
//! released before it returns.

use std::time::Duration;

/// The response descriptor of `GET_INFO`: sync bytes, a 20-byte single
/// response, type `0x04`.
pub const INFO_DESCRIPTOR: [u8; 7] = [0xA5, 0x5A, 0x14, 0x00, 0x00, 0x00, 0x04];
/// How long the probe waits for the descriptor.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(300);
#[cfg(target_os = "linux")]
const REQ_STOP: [u8; 2] = [0xA5, 0x25];
#[cfg(target_os = "linux")]
const REQ_GET_INFO: [u8; 2] = [0xA5, 0x50];
/// Quiet time the device needs after `STOP` before the next request.
#[cfg(target_os = "linux")]
const STOP_SETTLE: Duration = Duration::from_millis(10);
/// Bytes read before giving up on finding the descriptor.
#[cfg(target_os = "linux")]
const READ_LIMIT: usize = 4096;

/// What the probe found on a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RplidarProbe {
    /// The port answered `GET_INFO` with the RPLIDAR descriptor.
    Answered,
    /// Another process holds the port exclusively (the LiDAR driver opens
    /// it that way); nothing was written.
    Busy,
    /// No descriptor within the timeout, or the port could not be opened.
    Silent,
}

impl RplidarProbe {
    /// Whether the result identifies the LiDAR (for a declared LiDAR's
    /// CP2102 port).
    pub fn is_lidar(self) -> bool {
        !matches!(self, Self::Silent)
    }
}

/// Whether `bytes` carry the `GET_INFO` response descriptor anywhere.
pub fn contains_info_descriptor(bytes: &[u8]) -> bool {
    bytes
        .windows(INFO_DESCRIPTOR.len())
        .any(|w| w == INFO_DESCRIPTOR)
}

/// Probe `path` for an RPLIDAR: respect another process's exclusive hold,
/// otherwise send `STOP` then `GET_INFO` at 460800 baud (raw 8N1) and look
/// for the response descriptor for up to [`PROBE_TIMEOUT`]. The port is closed
/// before this returns.
#[cfg(target_os = "linux")]
pub fn probe_rplidar(path: &str) -> RplidarProbe {
    probe_with_timeout(path, PROBE_TIMEOUT)
}

/// `auto` discovery needs the Linux serial stack.
#[cfg(not(target_os = "linux"))]
pub fn probe_rplidar(_path: &str) -> RplidarProbe {
    RplidarProbe::Silent
}

#[cfg(target_os = "linux")]
pub(crate) fn probe_with_timeout(path: &str, timeout: Duration) -> RplidarProbe {
    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};
    use std::os::unix::fs::OpenOptionsExt;

    let opened = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
        .open(path);
    let file = match opened {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(Errno::EBUSY as i32) => return RplidarProbe::Busy,
        Err(_) => return RplidarProbe::Silent,
    };
    // The LiDAR driver (like every serialport-crate opener) takes an
    // exclusive flock; root bypasses TIOCEXCL, so the lock is the check.
    let mut file = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(locked) => locked,
        Err((_, Errno::EWOULDBLOCK)) => return RplidarProbe::Busy,
        Err(_) => return RplidarProbe::Silent,
    };
    if configure(&file).is_err() {
        return RplidarProbe::Silent;
    }
    exchange(&mut file, timeout)
}

/// Raw 8N1 at [`PROBE_BAUD`], no flow control.
#[cfg(target_os = "linux")]
fn configure(file: &std::fs::File) -> nix::Result<()> {
    use nix::sys::termios::{self, BaudRate, ControlFlags, SetArg};

    let mut t = termios::tcgetattr(file)?;
    termios::cfmakeraw(&mut t);
    termios::cfsetspeed(&mut t, BaudRate::B460800)?;
    t.control_flags |= ControlFlags::CLOCAL | ControlFlags::CREAD;
    t.control_flags &= !ControlFlags::CRTSCTS;
    termios::tcsetattr(file, SetArg::TCSANOW, &t)
}

#[cfg(target_os = "linux")]
fn exchange(file: &mut std::fs::File, timeout: Duration) -> RplidarProbe {
    use nix::poll::{poll, PollFd, PollFlags};
    use nix::sys::termios::{tcflush, FlushArg};
    use std::io::{ErrorKind, Read, Write};
    use std::os::fd::AsFd;
    use std::time::Instant;

    if file.write_all(&REQ_STOP).is_err() {
        return RplidarProbe::Silent;
    }
    std::thread::sleep(STOP_SETTLE);
    let _ = tcflush(&*file, FlushArg::TCIFLUSH);
    if file.write_all(&REQ_GET_INFO).is_err() {
        return RplidarProbe::Silent;
    }
    let deadline = Instant::now() + timeout;
    let mut got: Vec<u8> = Vec::new();
    let mut buf = [0u8; 256];
    while got.len() < READ_LIMIT {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let ms = u16::try_from(left.as_millis()).unwrap_or(u16::MAX).max(1);
        let ready = poll(&mut [PollFd::new(file.as_fd(), PollFlags::POLLIN)], ms);
        match ready {
            Ok(0) => break,
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                if contains_info_descriptor(&got) {
                    return RplidarProbe::Answered;
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
            Err(_) => break,
        }
    }
    RplidarProbe::Silent
}
