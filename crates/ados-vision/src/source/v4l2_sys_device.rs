//! An open, streaming capture node: MMAP buffers, dequeue and requeue.
//!
//! The driver owns a queued buffer and may write it at any time, so a
//! buffer's mapped bytes are reachable only through [`Dequeued`], which holds
//! the buffer out of the driver queue and borrows the device mutably (no
//! second dequeue, no stream-off) until it is dropped and requeued.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use super::{
    ioctl, query_cap, request_buffers, stream, Buffer, Capability, Plane, RequestBuffers, Timeval,
    BUF_TYPE_VIDEO_CAPTURE_MPLANE, MEMORY_MMAP, VIDIOC_DQBUF, VIDIOC_QBUF, VIDIOC_QUERYBUF,
};
use nix::libc;

/// One driver buffer mapped read-only into this process.
struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

// SAFETY: plain memory owned by this value; it is read only through a
// `Dequeued` guard, while the driver does not own the buffer.
unsafe impl Send for Mapping {}

impl Mapping {
    fn map(fd: RawFd, len: usize, offset: u32) -> io::Result<Self> {
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
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the region `map` created.
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// A buffer descriptor for `index` with its one-plane array attached when the
/// node is multi-planar. The array must outlive every ioctl on the descriptor.
fn descriptor(buf_type: u32, index: u32, planes: &mut [Plane; 1]) -> Buffer {
    let mut b = Buffer {
        index,
        type_: buf_type,
        memory: MEMORY_MMAP,
        ..Default::default()
    };
    if buf_type == BUF_TYPE_VIDEO_CAPTURE_MPLANE {
        b.m = planes.as_mut_ptr() as u64;
        b.length = 1;
    }
    b
}

/// `QUERYBUF` / `QBUF` / `DQBUF` on a descriptor built by [`descriptor`].
fn buffer_ioctl(fd: RawFd, request: u64, b: &mut Buffer) -> io::Result<()> {
    // SAFETY: the three buffer requests encode `size_of::<Buffer>()`; a
    // multi-planar descriptor's plane pointer refers to the caller's array,
    // live for this call.
    unsafe { ioctl(fd, request, b as *mut Buffer) }
}

/// Offset, length, used bytes and data offset of a buffer's (only) plane.
fn plane_of(buf_type: u32, b: &Buffer, planes: &[Plane; 1]) -> (u32, usize, usize, usize) {
    if buf_type == BUF_TYPE_VIDEO_CAPTURE_MPLANE {
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

/// An open capture node.
pub struct Device {
    // Fields drop in declaration order, after `Drop::drop` has stopped the
    // stream: the buffers unmap first, then the file closes last.
    maps: Vec<Mapping>,
    buf_type: u32,
    streaming: bool,
    /// A requeue that failed in a guard's drop, reported on the next dequeue.
    requeue_error: Option<io::Error>,
    file: File,
}

impl Device {
    /// Open `path` and query its capabilities.
    pub fn open(path: &str) -> io::Result<(Self, Capability)> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut cap = Capability {
            driver: [0; 16],
            card: [0; 32],
            bus_info: [0; 32],
            version: 0,
            capabilities: 0,
            device_caps: 0,
            reserved: [0; 3],
        };
        query_cap(file.as_raw_fd(), &mut cap)?;
        let dev = Self {
            maps: Vec::new(),
            buf_type: 0,
            streaming: false,
            requeue_error: None,
            file,
        };
        Ok((dev, cap))
    }

    /// The descriptor the typed format, parameter and control ioctls take.
    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Allocate, map and queue `count` MMAP buffers of `buf_type`, then start
    /// streaming.
    pub fn start(&mut self, buf_type: u32, count: u32) -> io::Result<()> {
        let fd = self.fd();
        let mut req = RequestBuffers {
            count,
            type_: buf_type,
            memory: MEMORY_MMAP,
            ..Default::default()
        };
        request_buffers(fd, &mut req)?;
        self.buf_type = buf_type;
        for index in 0..req.count {
            let mut planes = [Plane::default(); 1];
            let mut b = descriptor(buf_type, index, &mut planes);
            buffer_ioctl(fd, VIDIOC_QUERYBUF, &mut b)?;
            let (offset, len, _, _) = plane_of(buf_type, &b, &planes);
            self.maps.push(Mapping::map(fd, len, offset)?);
            buffer_ioctl(
                fd,
                VIDIOC_QBUF,
                &mut descriptor(buf_type, index, &mut planes),
            )?;
        }
        stream(fd, true, buf_type)?;
        self.streaming = true;
        Ok(())
    }

    /// Wait up to `timeout_ms` for a buffer to be ready.
    pub fn poll_readable(&self, timeout_ms: i32) -> io::Result<bool> {
        let mut p = libc::pollfd {
            fd: self.fd(),
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

    /// Take the next filled buffer out of the driver queue.
    pub fn dequeue(&mut self) -> io::Result<Dequeued<'_>> {
        if let Some(e) = self.requeue_error.take() {
            return Err(e);
        }
        let mut planes = [Plane::default(); 1];
        let mut b = descriptor(self.buf_type, 0, &mut planes);
        buffer_ioctl(self.fd(), VIDIOC_DQBUF, &mut b)?;
        let (_, _, used, data_offset) = plane_of(self.buf_type, &b, &planes);
        Ok(Dequeued {
            dev: self,
            index: b.index,
            used,
            data_offset,
            sequence: b.sequence,
            flags: b.flags,
            timestamp: b.timestamp,
        })
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.streaming {
            let _ = stream(self.fd(), false, self.buf_type);
        }
        // Then the fields drop: `maps` unmaps, `file` closes last.
    }
}

/// A buffer held out of the driver queue; requeued on drop.
pub struct Dequeued<'a> {
    dev: &'a mut Device,
    index: u32,
    used: usize,
    data_offset: usize,
    pub sequence: u32,
    pub flags: u32,
    pub timestamp: Timeval,
}

impl Dequeued<'_> {
    /// The frame bytes the driver filled (`data_offset..bytesused`), or `None`
    /// when the driver reported a range outside its buffer.
    pub fn data(&self) -> Option<&[u8]> {
        let m = self.dev.maps.get(self.index as usize)?;
        let end = self.used.min(m.len);
        if self.data_offset > end {
            return None;
        }
        // SAFETY: `ptr` maps `len` readable bytes for the device's lifetime,
        // and this guard keeps the buffer out of the driver queue, so the
        // driver does not write it while the slice is borrowed.
        let all = unsafe { std::slice::from_raw_parts(m.ptr as *const u8, m.len) };
        all.get(self.data_offset..end)
    }
}

impl Drop for Dequeued<'_> {
    fn drop(&mut self) {
        let mut planes = [Plane::default(); 1];
        let mut b = descriptor(self.dev.buf_type, self.index, &mut planes);
        if let Err(e) = buffer_ioctl(self.dev.fd(), VIDIOC_QBUF, &mut b) {
            self.dev.requeue_error = Some(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multi_planar_descriptor_carries_its_plane_array() {
        let mut planes = [Plane::default(); 1];
        let b = descriptor(BUF_TYPE_VIDEO_CAPTURE_MPLANE, 3, &mut planes);
        assert_eq!((b.index, b.length), (3, 1));
        assert_eq!(b.m, planes.as_ptr() as u64);
        let single = descriptor(super::super::BUF_TYPE_VIDEO_CAPTURE, 2, &mut planes);
        assert_eq!((single.m, single.length), (0, 0));
    }

    #[test]
    fn plane_geometry_is_read_from_the_right_place() {
        let planes = [Plane {
            bytesused: 900,
            length: 1024,
            m: 4096,
            data_offset: 64,
            ..Default::default()
        }];
        let b = Buffer {
            bytesused: 7,
            length: 8,
            m: 512,
            ..Default::default()
        };
        assert_eq!(
            plane_of(BUF_TYPE_VIDEO_CAPTURE_MPLANE, &b, &planes),
            (4096, 1024, 900, 64)
        );
        assert_eq!(
            plane_of(super::super::BUF_TYPE_VIDEO_CAPTURE, &b, &planes),
            (512, 8, 7, 0)
        );
    }
}
