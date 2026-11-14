//! The V4L2 capture thread (Linux): open, negotiate, stream, convert, hand
//! off.
//!
//! Both capture APIs are driven: a single-planar node
//! (`V4L2_BUF_TYPE_VIDEO_CAPTURE`, UVC and most bridges) and a multi-planar
//! one (`V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE`, the Rockchip ISP and CIF). On
//! the multi-planar API the frame must arrive in one plane (NV12, GREY,
//! RGB3, YUYV, MJPG and the Bayer formats do); a format split across planes
//! is refused. The node streams the bus format when it offers it, else MJPEG
//! or YUYV converted to it ([`negotiate`], [`Converter`]). Each buffer is
//! converted straight out of its mapping, so its descriptor timing is the
//! driver's. This module holds no `unsafe`: every kernel call goes through
//! `v4l2_sys`.

use std::io;
use std::os::fd::RawFd;

use super::controls::{drain, ControlDevice, ControlInfo};
use super::convert::{fourcc_name, jpeg_decoder, ConvertError, Converter, FailureRun, Wire};
use super::negotiate::{buffer_type, negotiate, FdNode, Layout};
use super::v4l2_sys as sys;
use super::{
    exposure_ns_from_absolute, poll_due, BufferPool, Captured, ControlQueue, V4l2Settings,
    BUF_FLAG_ERROR, CID_ANALOGUE_GAIN, CID_EXPOSURE_ABSOLUTE, CID_EXPOSURE_AUTO, CID_GAIN,
    DEQUEUE_TIMEOUT, DRIVER_BUFFERS, EXPOSURE_MANUAL,
};
use ados_protocol::framebus::mono_ns;

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
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
            default: q.default_value,
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

/// Open, negotiate and start streaming `s.device`.
fn open(s: &V4l2Settings) -> io::Result<(sys::Device, Layout, Wire)> {
    let (mut dev, cap) = sys::Device::open(&s.device)?;
    let fd = dev.fd();
    let caps = cap.node_caps();
    let buf_type = buffer_type(caps).ok_or_else(|| {
        invalid(format!(
            "{} is not a streaming capture node (caps {caps:#x})",
            s.device
        ))
    })?;
    let label = format!("{} ({})", s.device, sys::cstr(&cap.driver));
    let (layout, wire) = negotiate(&mut FdNode(fd), buf_type, s, &label)?;
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
    Ok((dev, layout, wire))
}

/// Stream until the consumer goes away or the device fails.
pub(super) fn run(
    s: &V4l2Settings,
    mut pool: BufferPool,
    tx: &tokio::sync::mpsc::Sender<io::Result<Captured>>,
    mut controls: Option<ControlQueue>,
) -> io::Result<()> {
    let (mut dev, layout, wire) = open(s)?;
    let fd = dev.fd();
    let frame_bytes = s.format.frame_bytes(s.width, s.height);
    let mut convert = Converter::new(
        wire,
        s.format,
        s.width,
        s.height,
        layout.stride,
        jpeg_decoder(),
    );
    let timeout_ms = DEQUEUE_TIMEOUT.as_millis() as i32;
    tracing::info!(
        device = %s.device,
        width = s.width,
        height = s.height,
        buf_type = layout.buf_type,
        pixelformat = %fourcc_name(layout.pixelformat),
        wire = ?wire,
        "v4l2_capture_started"
    );
    let mut exposure = read_exposure(fd);
    let mut frames: u64 = 0;
    let mut corrupt: u32 = 0;
    let mut short_logged = false;
    let mut decode_logged = false;
    // Buffers that keep arriving and keep failing to convert end the capture
    // with the reason: the device is streaming, not silent.
    let mut failing = FailureRun::default();
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
        // Converted straight out of the mapping, then requeued before the
        // frame leaves the thread.
        let converted = buf.data().map_or(Err(ConvertError::Short), |src| {
            convert.convert(src, &mut data)
        });
        let (sequence, flags, ts) = (buf.sequence, buf.flags, buf.timestamp);
        drop(buf);
        if let Err(e) = converted {
            match &e {
                ConvertError::Short => {
                    if !short_logged {
                        tracing::warn!(
                            device = %s.device,
                            frame_bytes,
                            "v4l2_short_buffer_dropped"
                        );
                        short_logged = true;
                    }
                }
                ConvertError::Decode(reason) => {
                    // A truncated or corrupt compressed frame: dropped, counted.
                    if !decode_logged {
                        tracing::warn!(
                            device = %s.device,
                            error = %reason,
                            "v4l2_frame_decode_failed"
                        );
                        decode_logged = true;
                    }
                    corrupt = corrupt.saturating_add(1);
                }
            }
            back.give(data);
            if let Some(err) = failing.failed(arrival_ns, &e) {
                return Err(err);
            }
            continue;
        }
        failing.converted();
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
