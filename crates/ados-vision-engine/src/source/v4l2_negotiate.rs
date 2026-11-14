//! Format negotiation of a capture node (Linux): which capture API it speaks,
//! which pixel format it streams in, and the layout the driver accepted.
//!
//! The node's offered formats are enumerated (`VIDIOC_ENUM_FMT`) and tried
//! best first ([`candidates`]) until the driver accepts one at the configured
//! size; when none fits, the open is refused with what the node offered and
//! what each attempt returned.

use std::io;
use std::os::fd::RawFd;

use ados_protocol::framebus::FrameFormat;

use super::convert::{candidates, fourcc_name, Wire};
use super::v4l2_sys as sys;
use super::{fourcc, V4l2Settings};

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
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

/// The request for `pixelformat` at `width` x `height` on `buf_type`.
pub(super) fn format_request(
    buf_type: u32,
    pixelformat: u32,
    width: u32,
    height: u32,
) -> sys::Format {
    if buf_type == sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE {
        sys::Format::multi(sys::PixFormatMplane {
            width,
            height,
            pixelformat,
            field: sys::FIELD_ANY,
            num_planes: 1,
            ..Default::default()
        })
    } else {
        sys::Format::single(sys::PixFormat {
            width,
            height,
            pixelformat,
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

/// The format calls negotiation makes on a node.
pub(super) trait FormatNode {
    /// The pixel formats the node offers on `buf_type`, in its order.
    fn offered(&mut self, buf_type: u32) -> io::Result<Vec<u32>>;
    /// `VIDIOC_S_FMT`: `f` comes back as the driver adjusted it.
    fn set_format(&mut self, f: &mut sys::Format) -> io::Result<()>;
}

/// An open node's descriptor.
pub(super) struct FdNode(pub RawFd);

impl FormatNode for FdNode {
    fn offered(&mut self, buf_type: u32) -> io::Result<Vec<u32>> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut d = sys::FmtDesc {
                index,
                type_: buf_type,
                ..Default::default()
            };
            match sys::enum_format(self.0, &mut d) {
                Ok(()) => out.push(d.pixelformat),
                // EINVAL past the last index ends the list.
                Err(e) if e.raw_os_error() == Some(nix::libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn set_format(&mut self, f: &mut sys::Format) -> io::Result<()> {
        sys::set_format(self.0, f)
    }
}

/// The lowercase name a bus format is configured by.
fn bus_name(f: FrameFormat) -> String {
    format!("{f:?}").to_lowercase()
}

/// Settle the format `s` is captured in on `node` (`label` names the node in
/// a refusal): the accepted layout and how its buffers become bus frames.
pub(super) fn negotiate(
    node: &mut impl FormatNode,
    buf_type: u32,
    s: &V4l2Settings,
    label: &str,
) -> io::Result<(Layout, Wire)> {
    let offered = node.offered(buf_type)?;
    let names: Vec<String> = offered.iter().map(|c| fourcc_name(*c)).collect();
    let tries = candidates(s.format, s.width, s.height, &offered);
    if tries.is_empty() {
        return Err(invalid(format!(
            "{label} offers [{}]; {} frames need {} from the camera, or MJPG/YUYV to \
             convert to rgb24, nv12, yuv420p or gray8 (4:2:0 at an even size)",
            names.join(", "),
            bus_name(s.format),
            fourcc_name(u32::from_le_bytes(fourcc(s.format))),
        )));
    }
    let mut got = Vec::new();
    for (code, wire) in tries {
        let mut f = format_request(buf_type, code, s.width, s.height);
        node.set_format(&mut f)?;
        let layout = match accepted_layout(&f) {
            Ok(l) => l,
            Err(e) => {
                got.push(format!("{}: {e}", fourcc_name(code)));
                continue;
            }
        };
        if (layout.width, layout.height, layout.pixelformat) == (s.width, s.height, code) {
            return Ok((layout, wire));
        }
        got.push(format!(
            "{} gave {}x{} {}",
            fourcc_name(code),
            layout.width,
            layout.height,
            fourcc_name(layout.pixelformat)
        ));
    }
    Err(invalid(format!(
        "{label} offers [{}] but none at {}x{} for {} frames: {}",
        names.join(", "),
        s.width,
        s.height,
        bus_name(s.format),
        got.join("; ")
    )))
}
