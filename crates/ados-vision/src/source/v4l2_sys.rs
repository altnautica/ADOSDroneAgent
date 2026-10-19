//! The V4L2 kernel interface this crate uses: the structs and ioctls of
//! `<linux/videodev2.h>` for querying a capture node, setting its format,
//! MMAP streaming (single- and multi-planar) and integer controls.
//!
//! Layouts are the 64-bit kernel ABI (`unsigned long` and pointers are 8
//! bytes), declared field for field from `videodev2.h`; each struct size is
//! asserted in the tests against the size the ioctl number encodes.
//!
//! Every `unsafe` block of the V4L2 path lives in this module and its
//! [`device`] submodule: the raw `ioctl` is private, each request is reached
//! through a function typed to its struct, the format union is read through
//! [`Format::pix`] / [`Format::pix_mp`], and a buffer's mapped bytes are only
//! reachable through the `Dequeued` guard that holds it out of the driver.

use std::io;
use std::os::fd::RawFd;

use nix::libc;

pub const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
pub const MEMORY_MMAP: u32 = 1;
pub const FIELD_ANY: u32 = 0;
pub const VIDEO_MAX_PLANES: usize = 8;

pub const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
pub const CAP_VIDEO_CAPTURE_MPLANE: u32 = 0x0000_1000;
pub const CAP_STREAMING: u32 = 0x0400_0000;
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;

/// `struct v4l2_capability` (104 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

impl Capability {
    /// The capabilities of this node (`device_caps` when the driver reports
    /// them, else the whole-device set).
    pub fn node_caps(&self) -> u32 {
        if self.capabilities & CAP_DEVICE_CAPS != 0 {
            self.device_caps
        } else {
            self.capabilities
        }
    }
}

/// `struct v4l2_pix_format` (48 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PixFormat {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub priv_: u32,
    pub flags: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
}

/// `struct v4l2_plane_pix_format` (20 bytes, packed).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanePixFormat {
    pub sizeimage: u32,
    pub bytesperline: u32,
    pub reserved: [u16; 6],
}

/// `struct v4l2_pix_format_mplane` (192 bytes, packed).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PixFormatMplane {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub colorspace: u32,
    pub plane_fmt: [PlanePixFormat; VIDEO_MAX_PLANES],
    pub num_planes: u8,
    pub flags: u8,
    pub ycbcr_enc: u8,
    pub quantization: u8,
    pub xfer_func: u8,
    pub reserved: [u8; 7],
}

/// The `fmt` union of `struct v4l2_format`: 200 bytes, 8-byte aligned (it
/// holds `struct v4l2_window`, which carries pointers).
#[repr(C)]
#[derive(Clone, Copy)]
pub union FormatUnion {
    pix: PixFormat,
    pix_mp: PixFormatMplane,
    raw: [u8; 200],
    _align: [u64; 25],
}

/// `struct v4l2_format` (208 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Format {
    type_: u32,
    fmt: FormatUnion,
}

impl Format {
    fn zeroed(type_: u32) -> Self {
        Format {
            type_,
            fmt: FormatUnion { raw: [0; 200] },
        }
    }

    /// A single-planar capture format request.
    pub fn single(pix: PixFormat) -> Self {
        let mut f = Self::zeroed(BUF_TYPE_VIDEO_CAPTURE);
        f.fmt.pix = pix;
        f
    }

    /// A multi-planar capture format request.
    pub fn multi(pix_mp: PixFormatMplane) -> Self {
        let mut f = Self::zeroed(BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        f.fmt.pix_mp = pix_mp;
        f
    }

    /// The buffer type the format is for (single- or multi-planar capture).
    pub fn buf_type(&self) -> u32 {
        self.type_
    }

    /// The union read as a single-planar format.
    pub fn pix(&self) -> PixFormat {
        // SAFETY: every member is plain integers and the union is fully
        // initialised (zeroed on construction, then written by the driver or
        // a constructor), so any bit pattern read through it is valid.
        unsafe { self.fmt.pix }
    }

    /// The union read as a multi-planar format.
    pub fn pix_mp(&self) -> PixFormatMplane {
        // SAFETY: as in `pix`.
        unsafe { self.fmt.pix_mp }
    }
}

/// `struct v4l2_requestbuffers` (20 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestBuffers {
    pub count: u32,
    pub type_: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

/// `struct timeval` (16 bytes on a 64-bit kernel ABI).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timeval {
    pub sec: i64,
    pub usec: i64,
}

/// `struct v4l2_timecode` (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Timecode {
    pub type_: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

/// `struct v4l2_buffer` (88 bytes). `m` is the offset of a single-planar
/// buffer (low 32 bits) or, for a multi-planar one, the `struct v4l2_plane *`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Buffer {
    pub index: u32,
    pub type_: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: Timeval,
    pub timecode: Timecode,
    pub sequence: u32,
    pub memory: u32,
    pub m: u64,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

/// `struct v4l2_plane` (64 bytes). `m` holds `mem_offset` in its low 32 bits.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Plane {
    pub bytesused: u32,
    pub length: u32,
    pub m: u64,
    pub data_offset: u32,
    pub reserved: [u32; 11],
}

/// `struct v4l2_control` (8 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Control {
    pub id: u32,
    pub value: i32,
}

/// `struct v4l2_queryctrl` (68 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryCtrl {
    pub id: u32,
    pub type_: u32,
    pub name: [u8; 32],
    pub minimum: i32,
    pub maximum: i32,
    pub step: i32,
    pub default_value: i32,
    pub flags: u32,
    pub reserved: [u32; 2],
}

/// `V4L2_CTRL_FLAG_DISABLED` and `V4L2_CTRL_FLAG_READ_ONLY`.
pub const CTRL_FLAG_DISABLED: u32 = 0x0001;
pub const CTRL_FLAG_READ_ONLY: u32 = 0x0004;

/// `struct v4l2_captureparm` (40 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CaptureParm {
    pub capability: u32,
    pub capturemode: u32,
    pub timeperframe_num: u32,
    pub timeperframe_den: u32,
    pub extendedmode: u32,
    pub readbuffers: u32,
    pub reserved: [u32; 4],
}

/// `struct v4l2_streamparm` (204 bytes): the type, then a 200-byte union
/// whose capture member is [`CaptureParm`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StreamParm {
    pub type_: u32,
    pub capture: CaptureParm,
    pub raw: [u8; 160],
}

/// `_IOC(dir, 'V', nr, size)`.
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}
const READ: u64 = 2;
const WRITE: u64 = 1;

pub const VIDIOC_QUERYCAP: u64 = ioc(READ, 0, std::mem::size_of::<Capability>());
pub const VIDIOC_S_FMT: u64 = ioc(READ | WRITE, 5, std::mem::size_of::<Format>());
pub const VIDIOC_REQBUFS: u64 = ioc(READ | WRITE, 8, std::mem::size_of::<RequestBuffers>());
pub const VIDIOC_QUERYBUF: u64 = ioc(READ | WRITE, 9, std::mem::size_of::<Buffer>());
pub const VIDIOC_QBUF: u64 = ioc(READ | WRITE, 15, std::mem::size_of::<Buffer>());
pub const VIDIOC_DQBUF: u64 = ioc(READ | WRITE, 17, std::mem::size_of::<Buffer>());
pub const VIDIOC_STREAMON: u64 = ioc(WRITE, 18, std::mem::size_of::<i32>());
pub const VIDIOC_STREAMOFF: u64 = ioc(WRITE, 19, std::mem::size_of::<i32>());
pub const VIDIOC_S_PARM: u64 = ioc(READ | WRITE, 22, std::mem::size_of::<StreamParm>());
pub const VIDIOC_G_CTRL: u64 = ioc(READ | WRITE, 27, std::mem::size_of::<Control>());
pub const VIDIOC_S_CTRL: u64 = ioc(READ | WRITE, 28, std::mem::size_of::<Control>());
pub const VIDIOC_QUERYCTRL: u64 = ioc(READ | WRITE, 36, std::mem::size_of::<QueryCtrl>());

/// `ioctl(fd, request, arg)`, retried on `EINTR`.
///
/// # Safety
/// `arg` must point at the struct `request` encodes, valid for reads and
/// writes for the call.
unsafe fn ioctl<T>(fd: RawFd, request: u64, arg: *mut T) -> io::Result<()> {
    loop {
        // SAFETY: forwarded from the caller.
        let r = unsafe { libc::ioctl(fd, request as _, arg) };
        if r != -1 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Generates one safe function per request, each bound to the one struct the
/// request number encodes, so a request can never be paired with the wrong
/// argument type.
macro_rules! typed_ioctl {
    ($($(#[$doc:meta])* $name:ident: $req:ident => $t:ty;)*) => {$(
        $(#[$doc])*
        pub fn $name(fd: RawFd, arg: &mut $t) -> io::Result<()> {
            // SAFETY: `$req` encodes `size_of::<$t>()` (asserted in the
            // tests) and `arg` is a live, exclusively borrowed `$t`.
            unsafe { ioctl(fd, $req, arg as *mut $t) }
        }
    )*};
}

typed_ioctl! {
    /// `VIDIOC_QUERYCAP`.
    query_cap: VIDIOC_QUERYCAP => Capability;
    /// `VIDIOC_S_FMT`.
    set_format: VIDIOC_S_FMT => Format;
    /// `VIDIOC_S_PARM`.
    set_parm: VIDIOC_S_PARM => StreamParm;
    /// `VIDIOC_G_CTRL`.
    get_ctrl: VIDIOC_G_CTRL => Control;
    /// `VIDIOC_S_CTRL`.
    set_ctrl: VIDIOC_S_CTRL => Control;
    /// `VIDIOC_QUERYCTRL`.
    query_ctrl: VIDIOC_QUERYCTRL => QueryCtrl;
    /// `VIDIOC_REQBUFS`.
    request_buffers: VIDIOC_REQBUFS => RequestBuffers;
}

/// `STREAMON` / `STREAMOFF` for a buffer type.
pub fn stream(fd: RawFd, on: bool, buf_type: u32) -> io::Result<()> {
    let mut t = buf_type as i32;
    let req = if on {
        VIDIOC_STREAMON
    } else {
        VIDIOC_STREAMOFF
    };
    // SAFETY: both requests encode a 4-byte int argument.
    unsafe { ioctl(fd, req, &mut t as *mut i32) }
}

#[path = "v4l2_sys_device.rs"]
mod device;
pub use device::Device;

/// A NUL-padded byte string field as text.
pub fn cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Little-endian fourcc code of four ASCII bytes (`v4l2_fourcc`).
pub const fn fourcc_code(c: [u8; 4]) -> u32 {
    u32::from_le_bytes(c)
}

#[cfg(test)]
#[path = "v4l2_sys_tests.rs"]
mod tests;
