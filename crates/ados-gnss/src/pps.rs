//! PPS: pair each pulse's host time with the UTC the receiver announced.
//!
//! The kernel stamps an assert edge on `CLOCK_REALTIME`; the reader converts
//! it to host monotonic with a realtime − monotonic delta sampled right after
//! the fetch. The UTC of a pulse is the latest UBX-TIM-TP (which announces the
//! next pulse), else the latest NAV-PVT time carried forward to the pulse and
//! rounded to the whole second.

use std::io;
use std::time::Duration;

/// A pulse older than this drops the lock.
pub const LOCK_WINDOW_NS: u64 = 2_000_000_000;
/// A TIM-TP is used for a pulse arriving at most this long after it.
const TP_WINDOW_NS: u64 = 1_500_000_000;
/// A NAV-PVT time is carried forward at most this long.
const PVT_WINDOW_NS: u64 = 2_000_000_000;
const SECOND_NS: i64 = 1_000_000_000;

/// One captured assert edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    /// Kernel `CLOCK_REALTIME` stamp of the edge, ns.
    pub realtime_ns: i64,
    /// `CLOCK_REALTIME − CLOCK_MONOTONIC` sampled at the fetch, ns.
    pub rt_minus_mono_ns: i64,
}

impl Edge {
    /// The edge on the host monotonic clock.
    pub fn host_ns(&self) -> u64 {
        (self.realtime_ns - self.rt_minus_mono_ns).max(0) as u64
    }
}

/// A PPS device: a blocking fetch of the next assert edge.
pub trait PpsSource: Send {
    /// The next new edge, or `None` when none arrived within `timeout`.
    fn fetch(&mut self, timeout: Duration) -> io::Result<Option<Edge>>;
}

/// Pairs pulses with receiver time.
#[derive(Debug, Default, Clone)]
pub struct Pairer {
    tp: Option<(u64, i64)>,
    pvt: Option<(u64, i64)>,
    last_paired_ns: Option<u64>,
}

impl Pairer {
    /// UBX-TIM-TP arrived at host `host_ns`, announcing the next pulse's UTC.
    pub fn on_tim_tp(&mut self, host_ns: u64, utc_ns: i64) {
        self.tp = Some((host_ns, utc_ns));
    }

    /// A NAV-PVT with a valid UTC epoch arrived at host `host_ns`.
    pub fn on_pvt_time(&mut self, host_ns: u64, utc_ns: i64) {
        self.pvt = Some((host_ns, utc_ns));
    }

    /// A pulse at host `host_ns`: its UTC, when known.
    pub fn on_pulse(&mut self, host_ns: u64) -> Option<i64> {
        let from_tp = self
            .tp
            .filter(|(t, _)| host_ns > *t && host_ns - *t <= TP_WINDOW_NS)
            .map(|(_, utc)| utc);
        let utc = match from_tp {
            Some(utc) => {
                // One announcement belongs to one pulse.
                self.tp = None;
                Some(utc)
            }
            None => self
                .pvt
                .filter(|(t, _)| host_ns.abs_diff(*t) <= PVT_WINDOW_NS)
                .map(|(t, utc)| {
                    let at_pulse = utc + (host_ns as i64 - t as i64);
                    (at_pulse + SECOND_NS / 2).div_euclid(SECOND_NS) * SECOND_NS
                }),
        };
        if utc.is_some() {
            self.last_paired_ns = Some(host_ns);
        }
        utc
    }

    /// A paired pulse within the last [`LOCK_WINDOW_NS`].
    pub fn locked(&self, now: u64) -> bool {
        self.last_paired_ns
            .is_some_and(|t| now.saturating_sub(t) <= LOCK_WINDOW_NS)
    }
}

#[cfg(target_os = "linux")]
pub use linux::DevPps;

#[cfg(target_os = "linux")]
mod linux {
    use super::{Edge, PpsSource};
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;
    use std::time::Duration;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct PpsKtime {
        sec: i64,
        nsec: i32,
        flags: u32,
    }

    // Kernel ABI layout; only the assert half is read.
    #[allow(dead_code)]
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct PpsKinfo {
        assert_sequence: u32,
        clear_sequence: u32,
        assert_tu: PpsKtime,
        clear_tu: PpsKtime,
        current_mode: i32,
    }

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct PpsFdata {
        info: PpsKinfo,
        timeout: PpsKtime,
    }

    // linux/pps.h: PPS_FETCH = _IOWR('p', 0xa4, struct pps_fdata *); the size
    // field encodes the pointer, not the struct.
    nix::ioctl_readwrite_bad!(
        pps_fetch,
        nix::request_code_readwrite!(b'p', 0xa4, std::mem::size_of::<*mut PpsFdata>()),
        PpsFdata
    );

    /// A `/dev/ppsN` device.
    pub struct DevPps {
        file: File,
        last_seq: Option<u32>,
    }

    impl DevPps {
        pub fn open(path: &Path) -> io::Result<Self> {
            Ok(Self {
                file: File::open(path)?,
                last_seq: None,
            })
        }
    }

    impl PpsSource for DevPps {
        fn fetch(&mut self, timeout: Duration) -> io::Result<Option<Edge>> {
            let mut data = PpsFdata {
                timeout: PpsKtime {
                    sec: timeout.as_secs() as i64,
                    nsec: timeout.subsec_nanos() as i32,
                    flags: 0,
                },
                ..PpsFdata::default()
            };
            // SAFETY: `data` is a live, correctly laid out pps_fdata the
            // kernel reads the timeout from and writes the info into.
            let res = unsafe { pps_fetch(self.file.as_raw_fd(), &mut data) };
            let delta =
                ados_sensorbus::time::realtime_ns() - ados_sensorbus::time::mono_ns() as i64;
            match res {
                Ok(_) => {}
                Err(nix::errno::Errno::ETIMEDOUT) | Err(nix::errno::Errno::EINTR) => {
                    return Ok(None)
                }
                Err(e) => return Err(io::Error::from(e)),
            }
            let seq = data.info.assert_sequence;
            if self.last_seq == Some(seq) || (data.info.assert_tu.sec == 0 && seq == 0) {
                return Ok(None);
            }
            self.last_seq = Some(seq);
            let tu = data.info.assert_tu;
            Ok(Some(Edge {
                realtime_ns: tu.sec * 1_000_000_000 + tu.nsec as i64,
                rt_minus_mono_ns: delta,
            }))
        }
    }
}

#[cfg(test)]
mod tests;
