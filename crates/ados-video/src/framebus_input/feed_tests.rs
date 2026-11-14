//! The framebus feed against a real ring file, descriptor socket and pipe.

use std::io::Read;
use std::time::Duration;

use ados_protocol::frame::encode_frame;
use ados_protocol::framebus::{
    write_slot, FrameDescriptor, FrameFormat, RingLayout, FRAMEBUS_DESCRIPTOR_VERSION, TIMEBASE_SIM,
};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixListener;

use super::{gst_raw_format, FeedSpec, FramebusFeed};

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
        hfov_deg: None,
        mount_pitch_deg: None,
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
        fps: 30,
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
    assert_eq!(FrameFormat::Gray8.ffmpeg_pix_fmt(), "gray");
    assert_eq!(FrameFormat::Rgb24.ffmpeg_pix_fmt(), "rgb24");
    assert_eq!(FrameFormat::BayerRggb8.ffmpeg_pix_fmt(), "bayer_rggb8");
    assert_eq!(gst_raw_format(FrameFormat::Nv12), "nv12");
    assert_eq!(gst_raw_format(FrameFormat::Yuv420p), "i420");
    assert_eq!(gst_raw_format(FrameFormat::BayerRggb16), "gray16-le");
}

#[tokio::test]
async fn renew_replaces_the_pipe_so_a_new_reader_starts_clean() {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let dir = tempfile::tempdir().unwrap();
    let spec = FeedSpec {
        camera_id: "cam_fl".to_string(),
        format: FrameFormat::Gray8,
        width: W,
        height: H,
        fps: 30,
        fifo: dir.path().join("framebus-cam_fl.fifo"),
        frames_socket: dir.path().join("vision-frames.sock"),
        shm_dir: dir.path().to_path_buf(),
    };
    let feed = FramebusFeed::spawn(spec.clone()).unwrap();
    // Hold the old pipe open (as a reader that died mid-frame would leave it
    // referenced), so its inode number cannot be reused by the new one.
    let old = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(&spec.fifo)
        .unwrap();
    let before = old.metadata().unwrap().ino();
    feed.renew().unwrap();
    let meta = std::fs::symlink_metadata(&spec.fifo).unwrap();
    assert!(std::os::unix::fs::FileTypeExt::is_fifo(&meta.file_type()));
    assert_ne!(
        meta.ino(),
        before,
        "a renewed feed must hand out a new pipe"
    );
}

/// An encoder blocked reading a pipe whose camera has gone silent cannot act
/// on SIGTERM; renewing the feed lets go of its pipe, so the reader gets
/// end-of-file and exits at once.
#[tokio::test]
async fn renew_releases_a_reader_blocked_on_a_silent_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let spec = FeedSpec {
        camera_id: "cam_fr".to_string(),
        format: FrameFormat::Gray8,
        width: W,
        height: H,
        fps: 30,
        fifo: dir.path().join("framebus-cam_fr.fifo"),
        frames_socket: dir.path().join("vision-frames.sock"),
        shm_dir: dir.path().to_path_buf(),
    };
    let feed = FramebusFeed::spawn(spec.clone()).unwrap();
    // A reader that ignores SIGTERM, blocked on the pipe with no frame coming.
    let script = format!("trap '' TERM; cat '{}' > /dev/null", spec.fifo.display());
    let mut reader = tokio::process::Command::new("sh")
        .args(["-c", &script])
        .spawn()
        .unwrap();
    // Let the feed attach to it as the pipe's writer.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(reader.try_wait().unwrap().is_none(), "blocked on the pipe");

    feed.renew().unwrap();
    let status = tokio::time::timeout(Duration::from_secs(2), reader.wait())
        .await
        .expect("the blocked reader exits once the feed lets go of its pipe")
        .unwrap();
    assert!(status.success());
}

/// A tempdir with a ring of `slots` frames for `cam_fl`, and a feed spec on it.
fn ring_fixture(slots: u32) -> (tempfile::TempDir, memmap2::MmapMut, RingLayout, FeedSpec) {
    let dir = tempfile::tempdir().unwrap();
    let layout = RingLayout::for_frame(slots, W, H, FrameFormat::Gray8);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.path().join("ados-vision-cam_fl"))
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
        fps: 30,
        fifo: dir.path().join("framebus-cam_fl.fifo"),
        frames_socket: dir.path().join("vision-frames.sock"),
        shm_dir: dir.path().to_path_buf(),
    };
    (dir, region, layout, spec)
}

/// Frame `id` of `cam_fl` captured at `capture_ms` on the source clock, in
/// ring slot `id % 8` under ring sequence `seq`.
fn captured(id: u64, capture_ms: u64, seq: u64) -> FrameDescriptor {
    let slot = (id % 8) as u32;
    FrameDescriptor {
        frame_id: id,
        ts_ns: 0,
        ts_sensor_ns: Some(capture_ms * 1_000_000),
        ..descriptor("cam_fl", seq, W, slot)
    }
}

/// Put frame `id` (every pixel `id`) in its slot under ring sequence `id`, and
/// announce `d` for it.
async fn publish(
    conn: &mut tokio::net::UnixStream,
    region: &mut memmap2::MmapMut,
    layout: &RingLayout,
    d: FrameDescriptor,
) {
    let id = d.frame_id;
    write_slot(
        region,
        layout,
        d.slot,
        id,
        &vec![id as u8; (W * H) as usize],
    )
    .unwrap();
    conn.write_all(&encode_frame(&d.to_msgpack().unwrap(), 1 << 20).unwrap())
        .await
        .unwrap();
}

/// Read `n` frames off the pipe on a blocking thread, with when each arrived.
fn read_frames(
    fifo: std::path::PathBuf,
    n: usize,
) -> tokio::task::JoinHandle<Vec<(u8, std::time::Instant)>> {
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::open(fifo).unwrap();
        (0..n)
            .map(|_| {
                let mut frame = vec![0u8; (W * H) as usize];
                f.read_exact(&mut frame).unwrap();
                (frame[0], std::time::Instant::now())
            })
            .collect()
    })
}

/// A source that stalls and then delivers several frames at once: every frame
/// of the burst reaches the pipe, spaced out at its capture cadence, instead of
/// the burst collapsing to its newest frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_is_spaced_out_not_collapsed() {
    let (_dir, mut region, layout, spec) = ring_fixture(8);
    let listener = UnixListener::bind(&spec.frames_socket).unwrap();
    let _feed = FramebusFeed::spawn(spec.clone()).unwrap();
    let reader = read_frames(spec.fifo.clone(), 6);
    let (mut conn, _) = listener.accept().await.unwrap();
    // Let the writer find the reader before the first frame.
    tokio::time::sleep(Duration::from_millis(600)).await;

    publish(&mut conn, &mut region, &layout, captured(1, 0, 1)).await;
    tokio::time::sleep(Duration::from_millis(33)).await;
    publish(&mut conn, &mut region, &layout, captured(2, 33, 2)).await;
    // A 140 ms stall, then four frames captured on cadence arrive together.
    tokio::time::sleep(Duration::from_millis(140)).await;
    for id in 3..=6u64 {
        publish(
            &mut conn,
            &mut region,
            &layout,
            captured(id, id * 33 - 33, id),
        )
        .await;
    }

    let frames = tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("every frame of the burst reaches the pipe")
        .unwrap();
    let ids: Vec<u8> = frames.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5, 6]);
    let gaps_ms: Vec<u128> = frames[2..]
        .windows(2)
        .map(|w| w[1].1.duration_since(w[0].1).as_millis())
        .collect();
    for gap in &gaps_ms {
        assert!(*gap >= 15, "the burst left bunched: {gaps_ms:?} ms");
    }
}

/// Frames that never reach the pipe are counted: a gap in the frame ids, and a
/// ring slot the engine reused before the feed read it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skipped_frames_and_seqlock_misses_are_counted() {
    let (_dir, mut region, layout, spec) = ring_fixture(8);
    let listener = UnixListener::bind(&spec.frames_socket).unwrap();
    let feed = FramebusFeed::spawn(spec.clone()).unwrap();
    let reader = read_frames(spec.fifo.clone(), 4);
    let (mut conn, _) = listener.accept().await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;

    // Frames 3 and 4 never arrive; frame 6's descriptor names a ring sequence
    // its slot no longer holds (the slot holds sequence 6).
    for (n, (id, seq)) in [(1, 1), (2, 2), (5, 5), (6, 99), (7, 7)]
        .into_iter()
        .enumerate()
    {
        let d = captured(id, 33 * n as u64, seq);
        publish(&mut conn, &mut region, &layout, d).await;
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
    let frames = tokio::time::timeout(Duration::from_secs(5), reader)
        .await
        .expect("four frames")
        .unwrap();
    let ids: Vec<u8> = frames.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![1, 2, 5, 7]);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while feed.stats().frames_written < 4 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let stats = feed.stats();
    assert_eq!(stats.camera_id, "cam_fl");
    assert_eq!(stats.frames_written, 4, "{stats:?}");
    assert_eq!(stats.seqlock_misses, 1, "{stats:?}");
    assert_eq!(stats.frames_skipped, 3, "{stats:?}");
    assert_eq!(stats.queue_overflows, 0, "{stats:?}");
}
