//! The framebus feed against a real ring file, descriptor socket and pipe.

use std::io::Read;
use std::time::Duration;

use ados_protocol::frame::encode_frame;
use ados_protocol::framebus::{
    write_slot, FrameDescriptor, FrameFormat, RingLayout, FRAMEBUS_DESCRIPTOR_VERSION, TIMEBASE_SIM,
};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;

use super::{ffmpeg_pix_fmt, gst_raw_format, FeedSpec, FramebusFeed};

const W: u32 = 4;
const H: u32 = 2;

fn descriptor(camera: &str, seq: u64, width: u32, slot: u32) -> FrameDescriptor {
    FrameDescriptor {
        v: FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: camera.to_string(),
        frame_id: seq,
        ts_ns: seq * 50_000_000,
        ts_sensor_ns: Some(seq),
        exposure_ns: None,
        trigger_seq: Some(seq),
        timebase: TIMEBASE_SIM,
        width,
        height: H,
        format: FrameFormat::Gray8,
        shm_name: "ados-vision-cam_fl".to_string(),
        slot,
        seq,
        byte_len: width * H,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_frames_reach_the_pipe_and_foreign_frames_do_not() {
    let dir = tempfile::tempdir().unwrap();
    let layout = RingLayout::for_frame(4, W, H, FrameFormat::Gray8);
    let ring_path = dir.path().join("ados-vision-cam_fl");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&ring_path)
        .unwrap();
    file.set_len(layout.total_len() as u64).unwrap();
    // SAFETY: a private tempfile mapping for the test's writer.
    let mut region = unsafe { memmap2::MmapMut::map_mut(&file) }.unwrap();
    layout.write_header(&mut region).unwrap();

    let spec = FeedSpec {
        camera_id: "cam_fl".to_string(),
        format: FrameFormat::Gray8,
        width: W,
        height: H,
        fifo: dir.path().join("framebus-cam_fl.fifo"),
        frames_socket: dir.path().join("vision-frames.sock"),
        shm_dir: dir.path().to_path_buf(),
    };
    let listener = UnixListener::bind(&spec.frames_socket).unwrap();
    let _feed = FramebusFeed::spawn(spec.clone()).unwrap();
    assert!(std::fs::symlink_metadata(&spec.fifo)
        .map(|m| std::os::unix::fs::FileTypeExt::is_fifo(&m.file_type()))
        .unwrap());

    let fifo = spec.fifo.clone();
    let reader = tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::open(fifo).unwrap();
        let mut frames = Vec::new();
        for _ in 0..3 {
            let mut frame = vec![0u8; (W * H) as usize];
            f.read_exact(&mut frame).unwrap();
            frames.push(frame);
        }
        frames
    });

    let (mut conn, _) = listener.accept().await.unwrap();
    for seq in 1..=200u64 {
        let slot = (seq % 4) as u32;
        write_slot(
            &mut region,
            &layout,
            slot,
            seq,
            &vec![seq as u8; (W * H) as usize],
        )
        .unwrap();
        // Another camera, and this camera at a size the encoder was not built
        // for: neither may reach the pipe.
        for d in [
            descriptor("cam_fr", seq, W, slot),
            descriptor("cam_fl", seq, W * 2, slot),
        ] {
            conn.write_all(&encode_frame(&d.to_msgpack().unwrap(), 1 << 20).unwrap())
                .await
                .unwrap();
        }
        let d = descriptor("cam_fl", seq, W, slot);
        conn.write_all(&encode_frame(&d.to_msgpack().unwrap(), 1 << 20).unwrap())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        if reader.is_finished() {
            break;
        }
    }
    let frames = tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("three frames within the deadline")
        .unwrap();
    let mut last = 0u8;
    for frame in frames {
        assert!(
            frame.iter().all(|b| *b == frame[0]),
            "a torn frame: {frame:?}"
        );
        assert!(frame[0] > last, "frames arrive in order");
        last = frame[0];
    }
}

#[test]
fn raw_format_tokens_cover_every_frame_format() {
    assert_eq!(ffmpeg_pix_fmt(FrameFormat::Gray8), "gray");
    assert_eq!(ffmpeg_pix_fmt(FrameFormat::Rgb24), "rgb24");
    assert_eq!(ffmpeg_pix_fmt(FrameFormat::BayerRggb8), "bayer_rggb8");
    assert_eq!(gst_raw_format(FrameFormat::Nv12), "nv12");
    assert_eq!(gst_raw_format(FrameFormat::Yuv420p), "i420");
    assert_eq!(gst_raw_format(FrameFormat::BayerRggb16), "gray16-le");
}
