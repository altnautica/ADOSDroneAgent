//! The V4L2 capture thread (Linux): open, configure, stream, hand off.
//!
//! Both capture APIs are driven: a single-planar node
//! (`V4L2_BUF_TYPE_VIDEO_CAPTURE`, UVC and most bridges) and a multi-planar
//! one (`V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE`, the Rockchip ISP and CIF). On
//! the multi-planar API the frame must arrive in one plane (NV12, GREY,
//! RGB3 and the Bayer formats do); a format split across planes is refused.
//! This module holds no `unsafe`: every kernel call goes through `v4l2_sys`.

use std::io;
use std::os::fd::RawFd;

use super::controls::{drain, ControlDevice, ControlInfo};
use super::v4l2_sys as sys;
use super::{
    copy_rows, exposure_ns_from_absolute, fourcc, poll_due, BufferPool, Captured, ControlQueue,
    V4l2Settings, BUF_FLAG_ERROR, CID_ANALOGUE_GAIN, CID_EXPOSURE_ABSOLUTE, CID_EXPOSURE_AUTO,
    CID_GAIN, DEQUEUE_TIMEOUT, DRIVER_BUFFERS, EXPOSURE_MANUAL,
};
use ados_protocol::framebus::{mono_ns, FrameFormat};

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Bytes per pixel row of the first plane (the stride a driver pads).
pub(super) fn row_bytes(format: FrameFormat, width: u32) -> usize {
    let w = width as usize;
    match format {
        FrameFormat::Rgb24 => w * 3,
        FrameFormat::Gray16 | FrameFormat::BayerRggb16 => w * 2,
        FrameFormat::Gray8 | FrameFormat::BayerRggb8 | FrameFormat::Nv12 | FrameFormat::Yuv420p => {
            w
        }
    }
}

/// The negotiated layout of a capture node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Layout {
    pub buf_type: u32,
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub stride: u32,
}

/// Which capture API a node speaks, from its capabilities.
pub(super) fn buffer_type(caps: u32) -> Option<u32> {
    if caps & sys::CAP_STREAMING == 0 {
        return None;
    }
    if caps & sys::CAP_VIDEO_CAPTURE != 0 {
        Some(sys::BUF_TYPE_VIDEO_CAPTURE)
    } else if caps & sys::CAP_VIDEO_CAPTURE_MPLANE != 0 {
        Some(sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE)
    } else {
        None
    }
}

/// The request format for `s` on `buf_type`.
pub(super) fn format_request(buf_type: u32, s: &V4l2Settings) -> sys::Format {
    let code = sys::fourcc_code(fourcc(s.format));
    if buf_type == sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE {
        sys::Format::multi(sys::PixFormatMplane {
            width: s.width,
            height: s.height,
            pixelformat: code,
            field: sys::FIELD_ANY,
            num_planes: 1,
            ..Default::default()
        })
    } else {
        sys::Format::single(sys::PixFormat {
            width: s.width,
            height: s.height,
            pixelformat: code,
            field: sys::FIELD_ANY,
            ..Default::default()
        })
    }
}

/// The layout the driver accepted, or why it cannot carry the frame.
pub(super) fn accepted_layout(f: &sys::Format) -> io::Result<Layout> {
    if f.buf_type() == sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE {
        let mp = f.pix_mp();
        if mp.num_planes != 1 {
            return Err(invalid(format!(
                "the format arrives in {} planes; one-plane formats only",
                mp.num_planes
            )));
        }
        let plane = mp.plane_fmt[0];
        Ok(Layout {
            buf_type: f.buf_type(),
            width: mp.width,
            height: mp.height,
            pixelformat: mp.pixelformat,
            stride: plane.bytesperline,
        })
    } else {
        let pix = f.pix();
        Ok(Layout {
            buf_type: f.buf_type(),
            width: pix.width,
            height: pix.height,
            pixelformat: pix.pixelformat,
            stride: pix.bytesperline,
        })
    }
}

fn set_ctrl(fd: RawFd, id: u32, value: i32) -> io::Result<()> {
    sys::set_ctrl(fd, &mut sys::Control { id, value })
}

/// Manual exposure and gain, when configured. A control the driver lacks is
/// logged, not fatal: the capture still runs at the driver's setting.
fn apply_controls(fd: RawFd, s: &V4l2Settings) {
    if let Some(us) = s.exposure_us {
        let absolute = i32::try_from((us / 100).max(1)).unwrap_or(i32::MAX);
        for (id, value) in [
            (CID_EXPOSURE_AUTO, EXPOSURE_MANUAL as i32),
            (CID_EXPOSURE_ABSOLUTE, absolute),
        ] {
            if let Err(e) = set_ctrl(fd, id, value) {
                tracing::warn!(device = %s.device, control = id, error = %e, "v4l2_exposure_not_set");
            }
        }
    }
    if let Some(gain) = s.gain {
        let value = gain.round() as i32;
        if set_ctrl(fd, CID_ANALOGUE_GAIN, value).is_err() {
            if let Err(e) = set_ctrl(fd, CID_GAIN, value) {
                tracing::warn!(device = %s.device, error = %e, "v4l2_gain_not_set");
            }
        }
    }
}

/// The exposure the device reports, in ns.
fn read_exposure(fd: RawFd) -> Option<u32> {
    let mut c = sys::Control {
        id: CID_EXPOSURE_ABSOLUTE,
        value: 0,
    };
    sys::get_ctrl(fd, &mut c).ok()?;
    exposure_ns_from_absolute(i64::from(c.value))
}

/// The control surface of the open, streaming device.
struct FdControls(RawFd);

impl ControlDevice for FdControls {
    fn query(&mut self, id: u32) -> Option<ControlInfo> {
        let mut q = sys::QueryCtrl {
            id,
            ..Default::default()
        };
        sys::query_ctrl(self.0, &mut q).ok()?;
        if q.flags & (sys::CTRL_FLAG_DISABLED | sys::CTRL_FLAG_READ_ONLY) != 0 {
            return None;
        }
        Some(ControlInfo {
            minimum: q.minimum,
            maximum: q.maximum,
            step: q.step,
        })
    }

    fn set(&mut self, id: u32, value: i32) -> io::Result<()> {
        set_ctrl(self.0, id, value)
    }

    fn get(&mut self, id: u32) -> io::Result<i32> {
        let mut c = sys::Control { id, value: 0 };
        sys::get_ctrl(self.0, &mut c)?;
        Ok(c.value)
    }
}

/// Open, configure and start streaming `s.device`.
fn open(s: &V4l2Settings) -> io::Result<(sys::Device, Layout)> {
    let (mut dev, cap) = sys::Device::open(&s.device)?;
    let fd = dev.fd();
    let caps = cap.node_caps();
    let buf_type = buffer_type(caps).ok_or_else(|| {
        invalid(format!(
            "{} is not a streaming capture node (caps {caps:#x})",
            s.device
        ))
    })?;
    let mut f = format_request(buf_type, s);
    sys::set_format(fd, &mut f)?;
    let layout = accepted_layout(&f)?;
    let want = sys::fourcc_code(fourcc(s.format));
    if (layout.width, layout.height, layout.pixelformat) != (s.width, s.height, want) {
        return Err(invalid(format!(
            "{} ({}) delivers {}x{} {:?} instead of {}x{} {:?}",
            s.device,
            sys::cstr(&cap.driver),
            layout.width,
            layout.height,
            layout.pixelformat.to_le_bytes(),
            s.width,
            s.height,
            fourcc(s.format)
        )));
    }
    if s.fps > 0 {
        let mut parm = sys::StreamParm {
            type_: buf_type,
            capture: sys::CaptureParm {
                timeperframe_num: 1,
                timeperframe_den: s.fps,
                ..Default::default()
            },
            raw: [0; 160],
        };
        if let Err(e) = sys::set_parm(fd, &mut parm) {
            tracing::warn!(device = %s.device, error = %e, "v4l2_frame_rate_not_set");
        }
    }
    apply_controls(fd, s);
    dev.start(buf_type, DRIVER_BUFFERS)?;
    Ok((dev, layout))
}

/// Stream until the consumer goes away or the device fails.
pub(super) fn run(
    s: &V4l2Settings,
    mut pool: BufferPool,
    tx: &tokio::sync::mpsc::Sender<io::Result<Captured>>,
    mut controls: Option<ControlQueue>,
) -> io::Result<()> {
    let (mut dev, layout) = open(s)?;
    let fd = dev.fd();
    let frame_bytes = s.format.frame_bytes(s.width, s.height);
    let row = row_bytes(s.format, s.width);
    let timeout_ms = DEQUEUE_TIMEOUT.as_millis() as i32;
    tracing::info!(device = %s.device, width = s.width, height = s.height, buf_type = layout.buf_type, "v4l2_capture_started");
    let mut exposure = read_exposure(fd);
    let mut frames: u64 = 0;
    let mut corrupt: u32 = 0;
    let mut short_logged = false;
    let back = pool.recycler();
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        // Control requests are applied between buffers, on this thread's own
        // handle: the device is never opened a second time.
        if let Some(queue) = controls.as_mut() {
            if drain(queue, &mut FdControls(fd)) {
                exposure = read_exposure(fd);
            }
        }
        if !dev.poll_readable(timeout_ms)? {
            continue;
        }
        let buf = dev.dequeue()?;
        let arrival_ns = mono_ns();
        frames += 1;
        if poll_due(frames) {
            exposure = read_exposure(fd);
        }
        if buf.flags & BUF_FLAG_ERROR != 0 {
            // The driver says this frame is corrupt: drop it, count it.
            corrupt = corrupt.saturating_add(1);
            continue;
        }
        let mut data = pool.take(frame_bytes);
        let copied = buf
            .data()
            .is_some_and(|src| copy_rows(src, layout.stride as usize, row, &mut data));
        let (sequence, flags, ts) = (buf.sequence, buf.flags, buf.timestamp);
        // Requeue before the frame leaves the thread.
        drop(buf);
        if !copied {
            if !short_logged {
                tracing::warn!(device = %s.device, frame_bytes, "v4l2_short_buffer_dropped");
                short_logged = true;
            }
            back.give(data);
            continue;
        }
        let captured = Captured {
            data,
            sequence,
            flags,
            sec: ts.sec,
            usec: ts.usec,
            exposure_ns: exposure,
            arrival_ns,
            dropped_corrupt: std::mem::take(&mut corrupt),
        };
        if tx.blocking_send(Ok(captured)).is_err() {
            return Ok(());
        }
    }
}
