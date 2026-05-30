//! Hardware H.264 encoder probe.
//!
//! The live bug this exists to kill: a board can ship an ffmpeg whose
//! `-encoders` list advertises a memory-to-memory wrapper (`h264_v4l2m2m`)
//! while the SoC has **no real V4L2 H.264 encoder device**. The wrapper passes
//! the listing check, ffmpeg then exits when it cannot open a backing device,
//! and the camera streams zero bytes.
//!
//! The fix this module carries: enumerate the real V4L2 nodes, confirm one
//! reports the memory-to-memory capability via `VIDIOC_QUERYCAP` and an H.264
//! output FourCC via `VIDIOC_ENUM_FMT`, then trial-init it through the
//! sandboxed [`crate::runner`] (a hung ioctl is reaped by the process-group
//! SIGKILL, never blocks the agent). Only `Present` when a real device passes;
//! otherwise `Absent`, and the encoder falls back to software libx264.
//!
//! ## Two stages, two isolation strategies
//! - **Stage 1 (read-only enumeration):** open each `/dev/video*` and issue
//!   `VIDIOC_QUERYCAP` + `VIDIOC_ENUM_FMT`. These ioctls only *query* the
//!   device, they never configure it, so the destructive-phase gate does not
//!   apply. A query ioctl can still wedge on a broken driver, so each node is
//!   probed on a **dedicated worker thread with a bounded join**: if the thread
//!   does not finish in time the function returns anyway (the stuck thread is
//!   detached and the agent survives — the in-process analog of the runner's
//!   process-group kill, since a D-state syscall is unreapable either way).
//! - **Stage 2 (destructive trial-init):** spawn a real `ffmpeg` that opens and
//!   configures the encoder. This *does* mutate device state, so it runs only
//!   when [`ProbePhase::allows_destructive`] is true, and always through
//!   [`crate::runner::run_probe_sandboxed`] so a hung init is SIGKILLed at the
//!   process-group level.

#[cfg(target_os = "linux")]
use ados_protocol::hwcaps::Evidence;
use ados_protocol::hwcaps::{AbsenceReason, EncoderDevice, ProbePhase, Probed};

/// The H.264 FourCC a real encoder produces on its capture side.
#[cfg(target_os = "linux")]
const FOURCC_H264: [u8; 4] = *b"H264";

/// Per-node Stage-1 query budget. A healthy driver answers a QUERYCAP in well
/// under a millisecond; this is a generous ceiling that still keeps the whole
/// enumeration bounded even if one node's driver misbehaves.
#[cfg(target_os = "linux")]
const STAGE1_NODE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// Stage-2 trial-init budget. ffmpeg opening a real V4L2 M2M encoder, running a
/// 0.3s test pattern, and exiting clears this comfortably; a lying wrapper
/// either exits fast (failure) or wedges (the runner kills the group).
#[cfg(target_os = "linux")]
const STAGE2_TRIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Probe for a real hardware H.264 encoder device.
///
/// `phase` gates the destructive Stage-2 trial-init. When the phase forbids
/// destructive probes (armed runtime), this returns [`Probed::NotProbed`] so the
/// caller reads the cached Setup / BootPreArm result rather than opening and
/// configuring an encoder device in flight.
pub fn probe_h264_encoder(phase: ProbePhase) -> Probed<EncoderDevice> {
    // Armed runtime: never open or trial-init an encoder. The caller holds the
    // cached snapshot from the Setup / BootPreArm probe.
    if !phase.allows_destructive() {
        return Probed::NotProbed;
    }
    probe_h264_encoder_impl()
}

#[cfg(target_os = "linux")]
fn probe_h264_encoder_impl() -> Probed<EncoderDevice> {
    // Stage 1: find a real V4L2 M2M node that enumerates an H.264 output FourCC.
    let node = match find_h264_m2m_node() {
        Some(node) => node,
        None => {
            // No backing device exists. THIS is the case the live bug hit: the
            // ffmpeg wrapper advertised h264_v4l2m2m but no node implements it,
            // so the caller must fall back to software libx264.
            return Probed::absent(AbsenceReason::NodeMissing);
        }
    };

    // Stage 2: the node exists and claims H.264; prove it accepts a bounded
    // trial init. Routed through the process-group runner so a hung kernel
    // ioctl is SIGKILLed without blocking the agent.
    let argv = [
        "ffmpeg",
        "-hide_banner",
        "-f",
        "lavfi",
        "-i",
        "testsrc=size=320x240:rate=5",
        "-t",
        "0.3",
        "-c:v",
        "h264_v4l2m2m",
        "-f",
        "null",
        "-",
    ];
    match crate::runner::run_probe_sandboxed(&argv, STAGE2_TRIAL_TIMEOUT) {
        crate::runner::ProbeOutcome::ExitedOk { ms } => Probed::present(
            EncoderDevice {
                node: node.clone(),
                fourcc: FOURCC_H264,
            },
            Evidence::TrialInit { node, ms },
        ),
        crate::runner::ProbeOutcome::ExitedAfterMs { ms } => {
            // The device advertised H.264 but the real init failed (exited).
            Probed::absent(AbsenceReason::TrialInitFailed { exit_after_ms: ms })
        }
        crate::runner::ProbeOutcome::TimedOutHung => {
            // The init wedged the ioctl. The runner already SIGKILLed the group;
            // treat the wrapper as a lying advertisement.
            Probed::absent(AbsenceReason::TrialInitHung)
        }
    }
}

/// Non-Linux dev host: there are no V4L2 nodes, so there is no real encoder
/// device. Report a genuine absence so the builder picks software libx264.
#[cfg(not(target_os = "linux"))]
fn probe_h264_encoder_impl() -> Probed<EncoderDevice> {
    Probed::absent(AbsenceReason::NodeMissing)
}

/// Enumerate `/dev/video*` and return the path of the first node that is a V4L2
/// memory-to-memory device AND enumerates an H.264 output FourCC. `None` when no
/// such node exists (the software-fallback case).
#[cfg(target_os = "linux")]
fn find_h264_m2m_node() -> Option<String> {
    for path in video_nodes() {
        // Probe each node on its own worker thread with a bounded join so a
        // misbehaving driver cannot wedge the enumeration. A stuck query is the
        // same unreapable D-state a process-group kill cannot help either, so we
        // detach the worker and move on — the agent never blocks.
        let probe_path = path.clone();
        let handle = std::thread::spawn(move || node_is_h264_m2m(&probe_path));
        let deadline = std::time::Instant::now() + STAGE1_NODE_TIMEOUT;
        loop {
            if handle.is_finished() {
                if let Ok(true) = handle.join() {
                    return Some(path);
                }
                break;
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(node = %path, "v4l2 query timed out; skipping node");
                // Detach the stuck worker; do not join (it may never return).
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    None
}

/// List `/dev/video*` device paths in ascending node order.
#[cfg(target_os = "linux")]
fn video_nodes() -> Vec<String> {
    let mut nodes: Vec<String> = match std::fs::read_dir("/dev") {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| {
                        n.starts_with("video")
                            && n["video".len()..].chars().all(|c| c.is_ascii_digit())
                    })
                    .unwrap_or(false)
            })
            .filter_map(|p| p.to_str().map(|s| s.to_string()))
            .collect(),
        Err(_) => Vec::new(),
    };
    nodes.sort();
    nodes
}

/// Open one node and decide whether it is a V4L2 M2M device that enumerates an
/// H.264 output FourCC. Runs on a worker thread (see [`find_h264_m2m_node`]).
#[cfg(target_os = "linux")]
fn node_is_h264_m2m(path: &str) -> bool {
    use std::ffi::CString;

    let cpath = match CString::new(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // O_NONBLOCK avoids a blocking open on a node that is busy; O_RDWR is needed
    // for VIDIOC_ENUM_FMT on the output queue.
    let fd = unsafe { nix::libc::open(cpath.as_ptr(), nix::libc::O_RDWR | nix::libc::O_NONBLOCK) };
    if fd < 0 {
        return false;
    }
    let result = (|| {
        let caps = match query_cap(fd) {
            Some(c) => c,
            None => return false,
        };
        // device_caps reports this node's own caps; capabilities is the union
        // across the whole device. A V4L2_CAP_DEVICE_CAPS device exposes the
        // per-node set in device_caps, otherwise fall back to capabilities.
        let effective = if caps.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
            caps.device_caps
        } else {
            caps.capabilities
        };
        let is_m2m = effective & (V4L2_CAP_VIDEO_M2M | V4L2_CAP_VIDEO_M2M_MPLANE) != 0;
        if !is_m2m {
            return false;
        }
        // A real encoder produces H.264 on the side it captures from the SoC.
        // Different drivers expose the encoded stream on the OUTPUT or the
        // CAPTURE queue depending on M2M direction convention, so check both
        // (single-plane and multi-plane variants).
        for buf_type in [
            V4L2_BUF_TYPE_VIDEO_OUTPUT,
            V4L2_BUF_TYPE_VIDEO_CAPTURE,
            V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE,
        ] {
            if enum_fmt_has_h264(fd, buf_type) {
                return true;
            }
        }
        false
    })();
    unsafe {
        nix::libc::close(fd);
    }
    result
}

/// `VIDIOC_QUERYCAP` minimal mirror of `struct v4l2_capability` (104 bytes).
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_fmtdesc` (64 bytes). `reserved[4]` covers the `mbus_code` field
/// added in later kernels (it occupies `reserved[0]`), so this layout is
/// binary-compatible across the kernel versions the supported boards ship.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Fmtdesc {
    index: u32,
    buf_type: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    reserved: [u32; 4],
}

// V4L2 capability flags (uapi/linux/videodev2.h, stable kernel ABI).
#[cfg(target_os = "linux")]
const V4L2_CAP_VIDEO_M2M: u32 = 0x0000_8000;
#[cfg(target_os = "linux")]
const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
#[cfg(target_os = "linux")]
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;

// V4L2 buffer types.
#[cfg(target_os = "linux")]
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
#[cfg(target_os = "linux")]
const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
#[cfg(target_os = "linux")]
const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
#[cfg(target_os = "linux")]
const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;

// ioctl request-code construction (asm-generic/ioctl.h encoding). The nix
// `ioctl_*!` macros need the gated `ioctl` feature, which this crate does not
// pull in; computing the request code here keeps the dependency surface minimal
// and the encoding auditable in one place.
#[cfg(target_os = "linux")]
const IOC_NRBITS: u32 = 8;
#[cfg(target_os = "linux")]
const IOC_TYPEBITS: u32 = 8;
#[cfg(target_os = "linux")]
const IOC_SIZEBITS: u32 = 14;
#[cfg(target_os = "linux")]
const IOC_NRSHIFT: u32 = 0;
#[cfg(target_os = "linux")]
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
#[cfg(target_os = "linux")]
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
#[cfg(target_os = "linux")]
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
#[cfg(target_os = "linux")]
const IOC_WRITE: u32 = 1;
#[cfg(target_os = "linux")]
const IOC_READ: u32 = 2;
/// `'V'` — the V4L2 ioctl type byte.
#[cfg(target_os = "linux")]
const V4L2_IOC_TYPE: u32 = 0x56;

#[cfg(target_os = "linux")]
const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u32 {
    (dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT)
}

/// `VIDIOC_QUERYCAP = _IOR('V', 0, struct v4l2_capability)` raw request code.
#[cfg(target_os = "linux")]
fn vidioc_querycap_code() -> u32 {
    ioc(
        IOC_READ,
        V4L2_IOC_TYPE,
        0,
        std::mem::size_of::<V4l2Capability>() as u32,
    )
}

/// `VIDIOC_ENUM_FMT = _IOWR('V', 2, struct v4l2_fmtdesc)` raw request code.
#[cfg(target_os = "linux")]
fn vidioc_enum_fmt_code() -> u32 {
    ioc(
        IOC_READ | IOC_WRITE,
        V4L2_IOC_TYPE,
        2,
        std::mem::size_of::<V4l2Fmtdesc>() as u32,
    )
}

/// Reinterpret a raw request code as the platform's `ioctl` request type
/// (`c_int` on musl, `c_ulong` on glibc) without changing the bit pattern.
#[cfg(target_os = "linux")]
fn ioctl_request(code: u32) -> nix::libc::Ioctl {
    code as nix::libc::Ioctl
}

/// Issue `VIDIOC_QUERYCAP` on an open fd.
#[cfg(target_os = "linux")]
fn query_cap(fd: i32) -> Option<V4l2Capability> {
    // Safety: V4l2Capability is repr(C) and matches the kernel's struct size;
    // VIDIOC_QUERYCAP fills it. The pointer is valid for the call's duration.
    let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        nix::libc::ioctl(
            fd,
            ioctl_request(vidioc_querycap_code()),
            &mut cap as *mut V4l2Capability,
        )
    };
    if rc == 0 {
        Some(cap)
    } else {
        None
    }
}

/// Walk the `VIDIOC_ENUM_FMT` index for `buf_type` and report whether an H.264
/// pixel format is enumerated.
#[cfg(target_os = "linux")]
fn enum_fmt_has_h264(fd: i32, buf_type: u32) -> bool {
    let want = u32::from_le_bytes(FOURCC_H264);
    let req = ioctl_request(vidioc_enum_fmt_code());
    // Bound the walk: drivers enumerate a small handful of formats. The loop
    // stops the moment the ioctl returns non-zero (past the last index).
    for index in 0u32..64 {
        // Safety: repr(C) struct sized to match the kernel's v4l2_fmtdesc; the
        // ioctl reads `index`/`buf_type` and writes the rest in place.
        let mut desc: V4l2Fmtdesc = unsafe { std::mem::zeroed() };
        desc.index = index;
        desc.buf_type = buf_type;
        let rc = unsafe { nix::libc::ioctl(fd, req, &mut desc as *mut V4l2Fmtdesc) };
        if rc != 0 {
            break;
        }
        if desc.pixelformat == want {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_phase_defers_probe() {
        // Armed runtime must never open or trial-init a device.
        assert!(matches!(
            probe_h264_encoder(ProbePhase::Runtime),
            Probed::NotProbed
        ));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_reports_node_missing() {
        // No V4L2 on the dev host → genuine absence (software fallback).
        assert!(matches!(
            probe_h264_encoder(ProbePhase::Setup),
            Probed::Absent {
                reason: AbsenceReason::NodeMissing
            }
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ioctl_codes_match_uapi() {
        // The well-known stable VIDIOC values; a regression in the encoding
        // helpers would silently break every probe.
        assert_eq!(vidioc_querycap_code(), 0x8068_5600);
        assert_eq!(vidioc_enum_fmt_code(), 0xC040_5602);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn struct_sizes_match_kernel_abi() {
        assert_eq!(std::mem::size_of::<V4l2Capability>(), 104);
        assert_eq!(std::mem::size_of::<V4l2Fmtdesc>(), 64);
    }
}
