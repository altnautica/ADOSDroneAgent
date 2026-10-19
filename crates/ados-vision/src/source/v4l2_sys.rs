//! The V4L2 kernel interface this crate uses: the structs and ioctls of
//! `<linux/videodev2.h>` for querying a capture node, setting its format,
//! MMAP streaming (single- and multi-planar) and integer controls.
//!
//! Layouts are the 64-bit kernel ABI (`unsigned long` and pointers are 8
//! bytes), declared field for field from `videodev2.h`; each struct size is
//! asserted in the tests against the size the ioctl number encodes. Every
//! `unsafe` block of the V4L2 path lives in this module.

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
    pub pix: PixFormat,
    pub pix_mp: PixFormatMplane,
    pub raw: [u8; 200],
    _align: [u64; 25],
}

/// `struct v4l2_format` (208 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Format {
    pub type_: u32,
    pub fmt: FormatUnion,
}

impl Format {
    pub fn zeroed(type_: u32) -> Self {
        Format {
            type_,
            fmt: FormatUnion { raw: [0; 200] },
        }
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

/// A typed `ioctl` on a struct this module declares.
pub fn xioctl<T>(fd: RawFd, request: u64, arg: &mut T) -> io::Result<()> {
    // SAFETY: `arg` is a live, exclusively borrowed `T`; every request
    // constant above encodes the size of the struct it is used with.
    unsafe { ioctl(fd, request, arg as *mut T) }
}

/// `STREAMON` / `STREAMOFF` for a buffer type.
pub fn stream(fd: RawFd, on: bool, buf_type: u32) -> io::Result<()> {
    let mut t = buf_type as i32;
    xioctl(fd, if on { VIDIOC_STREAMON } else { VIDIOC_STREAMOFF }, &mut t)
}

/// One driver buffer mapped read-only into this process.
pub struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: the mapping is plain memory owned by this value; it is only read
// through `as_slice` while the driver does not own the buffer.
unsafe impl Send for Mapping {}

impl Mapping {
    /// Map `len` bytes of the device at `offset`.
    pub fn map(fd: RawFd, len: usize, offset: u32) -> io::Result<Self> {
        // SAFETY: a fresh shared read-only mapping of a device buffer the
        // driver reported with QUERYBUF; unmapped in Drop.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                libc::off_t::from(offset),
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }

    /// The mapped bytes. Valid while the buffer is dequeued.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` readable bytes for the lifetime of self.
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the region `map` created.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// Wait up to `timeout_ms` for the node to have a buffer ready.
pub fn poll_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one valid pollfd.
        let r = unsafe { libc::poll(&mut p, 1, timeout_ms) };
        if r >= 0 {
            return Ok(r > 0);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

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
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn struct_sizes_match_the_kernel_abi() {
        assert_eq!(size_of::<Capability>(), 104);
        assert_eq!(size_of::<PixFormat>(), 48);
        assert_eq!(size_of::<PlanePixFormat>(), 20);
        assert_eq!(size_of::<PixFormatMplane>(), 192);
        assert_eq!(size_of::<FormatUnion>(), 200);
        assert_eq!(align_of::<FormatUnion>(), 8);
        assert_eq!(size_of::<Format>(), 208);
        assert_eq!(offset_of!(Format, fmt), 8);
        assert_eq!(size_of::<RequestBuffers>(), 20);
        assert_eq!(size_of::<Buffer>(), 88);
        assert_eq!(offset_of!(Buffer, timestamp), 24);
        assert_eq!(offset_of!(Buffer, sequence), 56);
        assert_eq!(offset_of!(Buffer, m), 64);
        assert_eq!(offset_of!(Buffer, length), 72);
        assert_eq!(size_of::<Plane>(), 64);
        assert_eq!(offset_of!(Plane, data_offset), 16);
        assert_eq!(size_of::<Control>(), 8);
        assert_eq!(size_of::<CaptureParm>(), 40);
        assert_eq!(size_of::<StreamParm>(), 204);
        assert_eq!(offset_of!(PixFormatMplane, num_planes), 180);
    }

    #[test]
    fn ioctl_numbers_match_videodev2() {
        // Values as the 64-bit kernel headers define them.
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_STREAMOFF, 0x4004_5613);
        assert_eq!(VIDIOC_S_PARM, 0xc0cc_5616);
        assert_eq!(VIDIOC_G_CTRL, 0xc008_561b);
        assert_eq!(VIDIOC_S_CTRL, 0xc008_561c);
    }

    #[test]
    fn capabilities_prefer_the_node_set() {
        let mut c = Capability {
            driver: [0; 16],
            card: [0; 32],
            bus_info: [0; 32],
            version: 0,
            capabilities: CAP_VIDEO_CAPTURE | CAP_STREAMING,
            device_caps: 0,
            reserved: [0; 3],
        };
        assert_eq!(c.node_caps(), CAP_VIDEO_CAPTURE | CAP_STREAMING);
        c.capabilities |= CAP_DEVICE_CAPS;
        c.device_caps = CAP_VIDEO_CAPTURE_MPLANE | CAP_STREAMING;
        assert_eq!(c.node_caps(), CAP_VIDEO_CAPTURE_MPLANE | CAP_STREAMING);
        assert_eq!(cstr(b"rkisp\0\0\0"), "rkisp");
        assert_eq!(fourcc_code(*b"NV12"), 0x3231_564e);
    }

    #[test]
    fn the_mplane_format_reads_back_through_the_union() {
        let mut f = Format::zeroed(BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        // The driver writes the plane layout into the union.
        // SAFETY: writing the raw bytes of a POD union in a test.
        unsafe {
            let raw = &mut f.fmt.raw;
            raw[0..4].copy_from_slice(&1280u32.to_le_bytes());
            raw[4..8].copy_from_slice(&800u32.to_le_bytes());
            raw[20..24].copy_from_slice(&1_536_000u32.to_le_bytes());
            raw[24..28].copy_from_slice(&1280u32.to_le_bytes());
            raw[180] = 1;
        }
        // SAFETY: the union is fully initialised POD.
        let mp = unsafe { f.fmt.pix_mp };
        let plane0 = mp.plane_fmt[0];
        assert_eq!({ mp.width }, 1280);
        assert_eq!({ mp.height }, 800);
        assert_eq!({ plane0.sizeimage }, 1_536_000);
        assert_eq!({ plane0.bytesperline }, 1280);
        assert_eq!(mp.num_planes, 1);
    }
}
