use super::*;

const SEC: i64 = 1_234;
const USEC: i64 = 567_890;
const RAW: u64 = 1_234_567_890_000;

#[test]
fn a_monotonic_end_of_frame_stamp_moves_back_half_an_exposure() {
    let t = buffer_timing(TIMESTAMP_MONOTONIC, SEC, USEC, Some(4_000_000), 9);
    assert_eq!(t.ts_sensor_ns, Some(RAW));
    assert_eq!(t.ts_ns, RAW - 2_000_000);
    assert_eq!(t.exposure_ns, Some(4_000_000));
    assert_eq!(t.timebase, TIMEBASE_HOST);
    assert_eq!(t.trigger_seq, None);
}

#[test]
fn a_monotonic_start_of_exposure_stamp_moves_forward_half_an_exposure() {
    let flags = TIMESTAMP_MONOTONIC | TSTAMP_SRC_SOE;
    let t = buffer_timing(flags, SEC, USEC, Some(4_000_000), 9);
    assert_eq!(t.ts_ns, RAW + 2_000_000);
    // Unknown exposure: the stamp itself.
    assert_eq!(buffer_timing(flags, SEC, USEC, None, 9).ts_ns, RAW);
}

#[test]
fn a_driver_clock_stamp_keeps_the_host_arrival() {
    for flags in [0x0000_0000, 0x0000_4000] {
        let t = buffer_timing(flags, SEC, USEC, None, 777);
        assert_eq!(t.ts_ns, 777);
        assert_eq!(t.ts_sensor_ns, Some(RAW));
        assert_eq!(t.timebase, TIMEBASE_SENSOR);
    }
    // A negative stamp is no stamp.
    let t = buffer_timing(TIMESTAMP_MONOTONIC, -1, 0, None, 5);
    assert_eq!((t.ts_ns, t.ts_sensor_ns), (5, None));
}

#[test]
fn exposure_absolute_is_in_100_us_units() {
    assert_eq!(exposure_ns_from_absolute(40), Some(4_000_000));
    assert_eq!(exposure_ns_from_absolute(0), None);
    assert_eq!(exposure_ns_from_absolute(-3), None);
    assert_eq!(exposure_ns_from_absolute(i64::MAX), None);
}

#[test]
fn padded_rows_are_packed() {
    // 3 rows of 2 bytes, each padded to a 4-byte stride.
    let src = [1, 2, 0, 0, 3, 4, 0, 0, 5, 6, 0, 0];
    let mut out = [0u8; 6];
    assert!(copy_rows(&src, 4, 2, &mut out));
    assert_eq!(out, [1, 2, 3, 4, 5, 6]);
    // Unpadded is one copy.
    let mut out = [0u8; 4];
    assert!(copy_rows(&[9, 8, 7, 6, 5], 2, 2, &mut out));
    assert_eq!(out, [9, 8, 7, 6]);
    // Too short for the frame.
    assert!(!copy_rows(&src[..7], 4, 2, &mut [0u8; 6]));
    assert!(!copy_rows(&[1], 1, 2, &mut [0u8; 2]));
}

#[test]
fn fourcc_codes_match_videodev2() {
    assert_eq!(&fourcc(FrameFormat::Gray8), b"GREY");
    assert_eq!(&fourcc(FrameFormat::Nv12), b"NV12");
    assert_eq!(&fourcc(FrameFormat::Rgb24), b"RGB3");
    assert_eq!(&fourcc(FrameFormat::BayerRggb8), b"RGGB");
}

#[cfg(target_os = "linux")]
mod layouts {
    use super::super::capture::{accepted_layout, buffer_type, format_request, row_bytes};
    use super::super::v4l2_sys as sys;
    use super::*;

    fn settings(format: FrameFormat) -> V4l2Settings {
        V4l2Settings {
            device: "/dev/video0".into(),
            width: 1280,
            height: 800,
            fps: 20,
            format,
            exposure_us: None,
            gain: None,
        }
    }

    #[test]
    fn the_capture_api_follows_the_node_capabilities() {
        let s = sys::CAP_STREAMING;
        assert_eq!(
            buffer_type(s | sys::CAP_VIDEO_CAPTURE),
            Some(sys::BUF_TYPE_VIDEO_CAPTURE)
        );
        assert_eq!(
            buffer_type(s | sys::CAP_VIDEO_CAPTURE_MPLANE),
            Some(sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE)
        );
        assert_eq!(buffer_type(sys::CAP_VIDEO_CAPTURE), None);
        assert_eq!(buffer_type(s), None);
    }

    #[test]
    fn a_single_planar_request_reads_back_its_layout() {
        let mut f = format_request(sys::BUF_TYPE_VIDEO_CAPTURE, &settings(FrameFormat::Gray8));
        // The driver pads rows to 1344 bytes (a union field write is safe).
        f.fmt.pix.bytesperline = 1344;
        let l = accepted_layout(&f).unwrap();
        assert_eq!((l.width, l.height, l.stride), (1280, 800, 1344));
        assert_eq!(l.pixelformat, sys::fourcc_code(*b"GREY"));
    }

    #[test]
    fn a_multi_planar_request_uses_plane_zero_and_refuses_split_planes() {
        let mut f = format_request(
            sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE,
            &settings(FrameFormat::Nv12),
        );
        // SAFETY: test access to the POD union the request filled.
        unsafe {
            assert_eq!({ f.fmt.pix_mp.num_planes }, 1);
            f.fmt.pix_mp.plane_fmt[0].bytesperline = 1280;
        }
        let l = accepted_layout(&f).unwrap();
        assert_eq!(l.buf_type, sys::BUF_TYPE_VIDEO_CAPTURE_MPLANE);
        assert_eq!((l.width, l.height, l.stride), (1280, 800, 1280));
        assert_eq!(l.pixelformat, sys::fourcc_code(*b"NV12"));
        // NV12M: the chroma plane is separate.
        f.fmt.pix_mp.num_planes = 2;
        assert!(accepted_layout(&f).is_err());
    }

    #[test]
    fn first_plane_rows_are_sized_by_format() {
        assert_eq!(row_bytes(FrameFormat::Rgb24, 10), 30);
        assert_eq!(row_bytes(FrameFormat::Nv12, 10), 10);
        assert_eq!(row_bytes(FrameFormat::Gray16, 10), 20);
    }
}
