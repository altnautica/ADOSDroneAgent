//! The V4L2 capture thread (Linux): open, configure, stream, hand off.
//!
//! Both capture APIs are driven: a single-planar node
//! (`V4L2_BUF_TYPE_VIDEO_CAPTURE`, UVC and most bridges) and a multi-planar
//! one (`V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE`, the Rockchip ISP and CIF). On
//! the multi-planar API the frame must arrive in one plane (NV12, GREY,
//! RGB3 and the Bayer formats do); a format split across planes is refused.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use super::v4l2_sys as sys;
use super::{
    copy_rows, exposure_ns_from_absolute, fourcc, poll_due, BufferPool, Captured, V4l2Settings,
    CID_ANALOGUE_GAIN, CID_EXPOSURE_ABSOLUTE, CID_EXPOSURE_AUTO, CID_GAIN, DEQUEUE_TIMEOUT,
    DRIVER_BUFFERS, EXPOSURE_MANUAL,
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
    let mut f = sys::Format::zeroed(buf_type);
    let code = sys::fourcc_code(fourcc(s.format));
    if buf_type == sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE {
        f.fmt.pix_mp = sys::PixFormatMplane {
            width: s.width,
            height: s.height,
            pixelformat: code,
            field: sys::FIELD_ANY,
            num_planes: 1,
            ..Default::default()
        };
    } else {
        f.fmt.pix = sys::PixFormat {
            width: s.width,
            height: s.height,
            pixelformat: code,
            field: sys::FIELD_ANY,
            ..Default::default()
        };
    }
    f
}

/// The layout the driver accepted, or why it cannot carry the frame.
pub(super) fn accepted_layout(f: &sys::Format) -> io::Result<Layout> {
    if f.type_ == sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE {
        // SAFETY: a multi-planar format struct the driver filled.
        let mp = unsafe { f.fmt.pix_mp };
        if mp.num_planes != 1 {
            return Err(invalid(format!(
                "the format arrives in {} planes; one-plane formats only",
                mp.num_planes
            )));
        }
        let plane = mp.plane_fmt[0];
        Ok(Layout {
            buf_type: f.type_,
            width: mp.width,
            height: mp.height,
            pixelformat: mp.pixelformat,
            stride: plane.bytesperline,
        })
    } else {
        // SAFETY: a single-planar format struct the driver filled.
        let pix = unsafe { f.fmt.pix };
        Ok(Layout {
            buf_type: f.type_,
            width: pix.width,
            height: pix.height,
            pixelformat: pix.pixelformat,
            stride: pix.bytesperline,
        })
    }
}

fn set_ctrl(fd: RawFd, id: u32, value: i32) -> io::Result<()> {
    sys::xioctl(fd, sys::VIDIOC_S_CTRL, &mut sys::Control { id, value })
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
    sys::xioctl(fd, sys::VIDIOC_G_CTRL, &mut c).ok()?;
    exposure_ns_from_absolute(i64::from(c.value))
}

/// A buffer descriptor for `index` (planes array attached when multi-planar).
fn buffer(layout: &Layout, index: u32, planes: &mut [sys::Plane; 1]) -> sys::Buffer {
    let mut b = sys::Buffer {
        index,
        type_: layout.buf_type,
        memory: sys::MEMORY_MMAP,
        ..Default::default()
    };
    if layout.buf_type == sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE {
        b.m = planes.as_mut_ptr() as u64;
        b.length = 1;
    }
    b
}

/// Offset, length and used bytes of a buffer's (only) plane.
fn plane_of(
    layout: &Layout,
    b: &sys::Buffer,
    planes: &[sys::Plane; 1],
) -> (u32, usize, usize, usize) {
    if layout.buf_type == sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE {
        let p = planes[0];
        (
            p.m as u32,
            p.length as usize,
            p.bytesused as usize,
            p.data_offset as usize,
        )
    } else {
        (b.m as u32, b.length as usize, b.bytesused as usize, 0)
    }
}

/// The open, configured, streaming node.
struct Node {
    file: std::fs::File,
    layout: Layout,
    maps: Vec<sys::Mapping>,
}

impl Node {
    fn open(s: &V4l2Settings) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(&s.device)?;
        let fd = file.as_raw_fd();
        let mut cap = sys::Capability {
            driver: [0; 16],
            card: [0; 32],
            bus_info: [0; 32],
            version: 0,
            capabilities: 0,
            device_caps: 0,
            reserved: [0; 3],
        };
        sys::xioctl(fd, sys::VIDIOC_QUERYCAP, &mut cap)?;
        let caps = cap.node_caps();
        let buf_type = buffer_type(caps).ok_or_else(|| {
            invalid(format!(
                "{} is not a streaming capture node (caps {caps:#x})",
                s.device
            ))
        })?;
        let mut f = format_request(buf_type, s);
        sys::xioctl(fd, sys::VIDIOC_S_FMT, &mut f)?;
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
            if let Err(e) = sys::xioctl(fd, sys::VIDIOC_S_PARM, &mut parm) {
                tracing::warn!(device = %s.device, error = %e, "v4l2_frame_rate_not_set");
            }
        }
        apply_controls(fd, s);
        let mut req = sys::RequestBuffers {
            count: DRIVER_BUFFERS,
            type_: buf_type,
            memory: sys::MEMORY_MMAP,
            ..Default::default()
        };
        sys::xioctl(fd, sys::VIDIOC_REQBUFS, &mut req)?;
        let mut maps = Vec::with_capacity(req.count as usize);
        for index in 0..req.count {
            let mut planes = [sys::Plane::default(); 1];
            let mut b = buffer(&layout, index, &mut planes);
            sys::xioctl(fd, sys::VIDIOC_QUERYBUF, &mut b)?;
            let (offset, len, _, _) = plane_of(&layout, &b, &planes);
            maps.push(sys::Mapping::map(fd, len, offset)?);
            sys::xioctl(
                fd,
                sys::VIDIOC_QBUF,
                &mut buffer(&layout, index, &mut planes),
            )?;
        }
        sys::stream(fd, true, buf_type)?;
        Ok(Self { file, layout, maps })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = sys::stream(self.fd(), false, self.layout.buf_type);
    }
}

/// Stream until the consumer goes away or the device fails.
pub(super) fn run(
    s: &V4l2Settings,
    mut pool: BufferPool,
    tx: &tokio::sync::mpsc::Sender<io::Result<Captured>>,
) -> io::Result<()> {
    let node = Node::open(s)?;
    let fd = node.fd();
    let frame_bytes = s.format.frame_bytes(s.width, s.height);
    let row = row_bytes(s.format, s.width);
    let timeout_ms = DEQUEUE_TIMEOUT.as_millis() as i32;
    tracing::info!(device = %s.device, width = s.width, height = s.height, buf_type = node.layout.buf_type, "v4l2_capture_started");
    let mut exposure = read_exposure(fd);
    let mut frames: u64 = 0;
    let mut short_logged = false;
    let back = pool.recycler();
    loop {
        if tx.is_closed() {
            return Ok(());
        }
        if !sys::poll_readable(fd, timeout_ms)? {
            continue;
        }
        let mut planes = [sys::Plane::default(); 1];
        let mut b = buffer(&node.layout, 0, &mut planes);
        sys::xioctl(fd, sys::VIDIOC_DQBUF, &mut b)?;
        let arrival_ns = mono_ns();
        frames += 1;
        if poll_due(frames) {
            exposure = read_exposure(fd);
        }
        let (_, _, used, data_offset) = plane_of(&node.layout, &b, &planes);
        let mut data = pool.take(frame_bytes);
        let copied = node
            .maps
            .get(b.index as usize)
            .and_then(|m| m.as_slice().get(data_offset..used.max(data_offset)))
            .is_some_and(|src| copy_rows(src, node.layout.stride as usize, row, &mut data));
        let (sequence, flags, ts) = (b.sequence, b.flags, b.timestamp);
        // Hand the buffer back to the driver before the frame leaves.
        let index = b.index;
        sys::xioctl(
            fd,
            sys::VIDIOC_QBUF,
            &mut buffer(&node.layout, index, &mut planes),
        )?;
        if !copied {
            if !short_logged {
                tracing::warn!(device = %s.device, used, frame_bytes, "v4l2_short_buffer_dropped");
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
        };
        if tx.blocking_send(Ok(captured)).is_err() {
            return Ok(());
        }
    }
}
