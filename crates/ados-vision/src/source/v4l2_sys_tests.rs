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
    let mp = f.pix_mp();
    let plane0 = mp.plane_fmt[0];
    assert_eq!({ mp.width }, 1280);
    assert_eq!({ mp.height }, 800);
    assert_eq!({ plane0.sizeimage }, 1_536_000);
    assert_eq!({ plane0.bytesperline }, 1280);
    assert_eq!(mp.num_planes, 1);
}
