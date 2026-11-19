use std::time::{Duration, Instant};

use ados_protocol::framebus::{FrameFormat, TIMEBASE_HOST, VISION_DETECTION_VERSION};
use ados_time::{wire, SimFeed};
use tokio::io::AsyncReadExt;

use super::*;

const MS: u64 = 1_000_000;

fn desc(sim_ms: Option<u64>) -> FrameDescriptor {
    FrameDescriptor {
        v: ados_protocol::framebus::FRAMEBUS_DESCRIPTOR_VERSION,
        camera_id: "cam_fl".into(),
        frame_id: 1,
        ts_ns: 5_000 * MS,
        ts_sensor_ns: sim_ms.map(|ms| ms * MS),
        exposure_ns: None,
        trigger_seq: None,
        timebase: if sim_ms.is_some() {
            TIMEBASE_SIM
        } else {
            TIMEBASE_HOST
        },
        width: 8,
        height: 8,
        format: FrameFormat::Rgb24,
        shm_name: "ados-vision-cam_fl".into(),
        slot: 0,
        seq: 1,
        byte_len: 192,
        hfov_deg: None,
        mount_pitch_deg: None,
    }
}

fn batch(camera: &str, sim_ms: Option<i64>) -> DetectionBatch {
    DetectionBatch {
        v: VISION_DETECTION_VERSION,
        model_id: "det".into(),
        camera_id: camera.into(),
        frame_id: 1,
        ts_ms: 5_000,
        ts_sensor_ms: sim_ms,
        hfov_deg: None,
        mount_pitch_deg: None,
        frame_width: 8,
        frame_height: 8,
        crop_window: None,
        detections: Vec::new(),
    }
}

fn tick(seq: u32, sim_ms: u64) -> wire::Tick {
    wire::Tick {
        flags: wire::FLAG_LOCKSTEP,
        seq,
        sim_ns: sim_ms * MS,
        grant_ns: (sim_ms + 20) * MS,
        speed: 0.0,
    }
}

async fn recv(rx: &mut broadcast::Receiver<DetectionBatch>) -> Option<DetectionBatch> {
    tokio::time::timeout(Duration::from_millis(200), rx.recv())
        .await
        .ok()
        .and_then(Result::ok)
}

#[test]
fn the_frame_clock_is_the_sei_sim_time_of_a_simulated_frame() {
    assert_eq!(frame_clock_ns(&desc(Some(1_250))), Some(1_250 * MS));
    assert_eq!(frame_clock_ns(&desc(None)), None, "a host-clock frame");
}

#[test]
fn the_modelled_latency_and_the_clock_come_from_the_config() {
    let s = SimSetup::from_config(
        "simulation:\n  enabled: true\n  clock: sim\n  clock_mode: lockstep\nvision:\n  sim_latency_ms: 140\n",
    );
    assert!(s.enabled() && s.gate.sim_clock());
    assert_eq!(s.latency_ms, Some(140));
    let none = SimSetup::from_config("simulation:\n  enabled: true\n");
    assert!(none.enabled() && !none.gate.sim_clock());
    assert_eq!(none.latency_ms, None);
}

#[test]
fn held_batches_come_due_in_release_order() {
    let mut p = Pacing::default();
    assert_eq!(
        Pacing::release_ns(&batch("a", Some(1_000)), 150 * MS),
        Some(1_150 * MS)
    );
    assert_eq!(Pacing::release_ns(&batch("a", None), 150 * MS), None);
    p.hold(1_300 * MS, batch("a", Some(1_100)));
    p.hold(1_150 * MS, batch("b", Some(1_000)));
    assert_eq!(p.next_release(), Some(1_150 * MS));
    assert!(p.due(1_149 * MS).is_empty());
    let due = p.due(1_150 * MS);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].camera_id, "b");
    assert_eq!(p.due(2_000 * MS).len(), 1);
    assert_eq!(p.next_release(), None);
}

#[test]
fn a_lane_reports_half_a_period_past_its_next_frame_and_never_past_a_held_batch() {
    let mut p = Pacing::default();
    p.frame("a", 1_000 * MS);
    p.free("a");
    assert_eq!(p.done_target("a"), None, "the period is not known yet");
    p.frame("a", 1_100 * MS);
    assert_eq!(
        p.done_target("a"),
        None,
        "a frame in inference holds the lane"
    );
    p.free("a");
    assert_eq!(p.done_target("a"), Some(1_250 * MS));
    // A held batch of this camera releases before then: the lane stops there.
    p.hold(1_200 * MS, batch("a", Some(1_100)));
    p.hold(1_150 * MS, batch("b", Some(1_000)));
    assert_eq!(p.done_target("a"), Some(1_200 * MS));
    p.due(1_200 * MS);
    assert_eq!(p.done_target("a"), Some(1_250 * MS));
    // Two frames taken, one finished: still busy.
    p.frame("a", 1_200 * MS);
    p.frame("a", 1_300 * MS);
    p.free("a");
    assert_eq!(p.done_target("a"), None);
    p.free("a");
    assert_eq!(p.done_target("a"), Some(1_450 * MS));
    p.lost("a");
    assert_eq!(p.done_target("a"), None, "a lost source leaves the barrier");
}

#[tokio::test]
async fn a_result_is_released_at_its_frame_sim_time_plus_the_modelled_latency() {
    let feed = SimFeed::new();
    let (tx, mut rx) = broadcast::channel(8);
    let p = SimPacing::new(Clock::Sim(feed.clone()), Some(150), None, tx);
    let at = Instant::now();
    feed.ingest(&tick(1, 1_000), at);
    assert_eq!(
        p.publish(batch("a", Some(1_000))),
        1,
        "it will reach the subscriber"
    );
    assert!(recv(&mut rx).await.is_none(), "inference costs no sim time");
    feed.ingest(&tick(2, 1_100), at);
    assert!(recv(&mut rx).await.is_none());
    feed.ingest(&tick(3, 1_160), at);
    assert_eq!(recv(&mut rx).await.unwrap().ts_sensor_ms, Some(1_000));
    // Already late, or of a host-clock frame: out at once.
    p.publish(batch("a", Some(900)));
    assert_eq!(rx.try_recv().unwrap().ts_sensor_ms, Some(900));
    p.publish(batch("a", None));
    assert_eq!(rx.try_recv().unwrap().ts_sensor_ms, None);
}

#[tokio::test]
async fn the_measured_latency_stands_in_when_none_is_configured() {
    let (tx, _rx) = broadcast::channel(8);
    let p = SimPacing::new(Clock::Sim(SimFeed::new()), None, None, tx.clone());
    assert_eq!(p.latency_ns(), 0);
    p.observe_infer_ms(80.0);
    assert_eq!(p.latency_ns(), 80 * MS);
    p.observe_infer_ms(180.0);
    assert_eq!(p.latency_ns(), 100 * MS, "an EWMA, 0.2 on the newest");
    p.observe_infer_ms(f32::NAN);
    assert_eq!(p.latency_ns(), 100 * MS);
    let fixed = SimPacing::new(Clock::Sim(SimFeed::new()), Some(140), None, tx);
    fixed.observe_infer_ms(80.0);
    assert_eq!(fixed.latency_ns(), 140 * MS);
}

/// One `DONE` frame's `done_ns`.
async fn read_done(s: &mut tokio::net::UnixStream) -> u64 {
    let mut b = [0u8; wire::DONE_LEN];
    tokio::time::timeout(Duration::from_secs(3), s.read_exact(&mut b))
        .await
        .unwrap()
        .unwrap();
    wire::Done::decode(&b).unwrap().done_ns
}

#[tokio::test]
async fn in_lockstep_a_camera_holds_the_barrier_per_frame_and_per_held_result() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(ados_time::PARTICIPANTS_SOCKET);
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let feed = SimFeed::new();
    let (tx, mut rx) = broadcast::channel(8);
    let p = SimPacing::new(Clock::Sim(feed.clone()), Some(100), Some(sock), tx);
    let at = Instant::now();
    feed.ingest(&tick(1, 1_000), at);
    assert!(p.frame("cam_fl", &desc(Some(1_000))));
    p.free("cam_fl");
    assert!(
        !p.frame("cam_fl", &desc(None)),
        "a host-clock frame takes no lane"
    );
    // The second frame: its batch is held to 1200 ms, before the next frame
    // is due (1100 + 100 + 50): the camera registers and reports 1200.
    feed.ingest(&tick(2, 1_100), at);
    assert!(p.frame("cam_fl", &desc(Some(1_100))));
    p.publish(batch("cam_fl", Some(1_100)));
    p.free("cam_fl");
    let (mut s, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read_done(&mut s).await, 1_200 * MS);
    // The simulator reaches the release: the batch goes out and the camera
    // waits for its next frame.
    feed.ingest(&tick(3, 1_200), at);
    assert_eq!(recv(&mut rx).await.unwrap().ts_sensor_ms, Some(1_100));
    assert_eq!(read_done(&mut s).await, 1_250 * MS);
    // A lost source leaves the barrier: the connection closes.
    p.lost("cam_fl");
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(3), s.read(&mut b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0);
}
