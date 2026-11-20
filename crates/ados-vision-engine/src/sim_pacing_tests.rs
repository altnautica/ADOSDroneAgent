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
fn a_lane_reports_half_a_period_past_its_delivered_next_frame_and_never_past_a_held_batch() {
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
    // The next frame (1200) reaches the engine once 1300 is rendered.
    assert_eq!(p.done_target("a"), Some(1_350 * MS));
    // A held batch of this camera releases before then: the lane stops there.
    p.hold(1_200 * MS, batch("a", Some(1_100)));
    p.hold(1_150 * MS, batch("b", Some(1_000)));
    assert_eq!(p.done_target("a"), Some(1_200 * MS));
    p.due(1_200 * MS);
    assert_eq!(p.done_target("a"), Some(1_350 * MS));
    // Two frames taken, one finished: still busy.
    p.frame("a", 1_200 * MS);
    p.frame("a", 1_300 * MS);
    p.free("a");
    assert_eq!(p.done_target("a"), None);
    p.free("a");
    assert_eq!(p.done_target("a"), Some(1_550 * MS));
    p.lost("a");
    assert_eq!(p.done_target("a"), None, "a lost source leaves the barrier");
}

/// Drive a rendered lockstep run through the agent's own barrier
/// ([`ados_time::barrier::Barrier`]) with one camera of period `period_ns`
/// reporting `target`, World Engine granting `grant_ns` at a time: the run
/// holds at a boundary until the barrier acknowledges it, draws a frame on
/// the step past its sim time (a frame at the boundary it holds at is not
/// drawn yet), and the source hands frame k on only once frame k+1 is drawn.
/// Returns the sim time the run reached, ns (it stops at `until_ns` or where
/// the barrier stalls).
fn barrier_run(
    period_ns: u64,
    grant_ns: u64,
    until_ns: u64,
    target: impl Fn(&Pacing) -> Option<u64>,
) -> u64 {
    let mut b = ados_time::barrier::Barrier::default();
    let from: std::net::SocketAddr = "127.0.0.1:14650".parse().unwrap();
    let now = Instant::now();
    let mut p = Pacing::default();
    let (mut sim, mut delivered, mut seq) = (0u64, 0u64, 0u32);
    let mut id = None;
    while sim < until_ns {
        while (delivered + 1) * period_ns < sim {
            p.frame("a", delivered * period_ns);
            p.free("a");
            delivered += 1;
        }
        if let Some(done) = target(&p) {
            let id = *id.get_or_insert_with(|| b.join().unwrap());
            b.done(id, done, 0, now);
        }
        seq += 1;
        let tick = wire::Tick {
            flags: wire::FLAG_LOCKSTEP,
            seq,
            sim_ns: sim,
            grant_ns: sim + grant_ns,
            speed: 0.0,
        };
        // Nothing else moves while the run holds: no ACK is a deadlock.
        let Some((ack, _)) = b.tick(&tick, from, now) else {
            return sim;
        };
        sim = ack.done_ns;
    }
    sim
}

/// The barrier keeps advancing against a one-frame-late source for any
/// grant: the run holds only at a boundary at or past the lane's target,
/// which is past the frame that releases the next one.
#[test]
fn the_barrier_keeps_advancing_with_a_one_frame_late_source_for_any_grant() {
    const UNTIL: u64 = 20_000 * MS;
    for (fps, grant_ms) in [(30, 20), (60, 20), (30, 1), (30, 50), (10, 20), (60, 100)] {
        let period = 1_000 * MS / fps;
        let reached = barrier_run(period, grant_ms * MS, UNTIL, |p| p.done_target("a"));
        assert!(
            reached >= UNTIL,
            "{fps} fps, grant {grant_ms} ms: stalled at {} ms",
            reached / MS
        );
    }
}

/// The target before the source's delay was counted (half a period past the
/// next frame's own time) deadlocks the same runs.
#[test]
fn half_a_period_past_the_next_frame_deadlocks_a_one_frame_late_source() {
    for (fps, grant_ms) in [(30, 20), (60, 20)] {
        let period = 1_000 * MS / fps;
        let reached = barrier_run(period, grant_ms * MS, 20_000 * MS, |p| {
            p.done_target("a").map(|d| d - period)
        });
        assert!(
            reached < 20_000 * MS,
            "{fps} fps, grant {grant_ms} ms: expected a stall, ran to the end"
        );
    }
}

#[test]
fn a_lane_whose_expected_frame_never_comes_leaves_the_barrier() {
    let mut p = Pacing::default();
    p.frame("a", 1_000 * MS);
    p.free("a");
    p.frame("a", 1_100 * MS);
    p.free("a");
    // Frame 1200 is expected once 1300 is rendered: not overdue before then.
    assert!(p.overdue(1_300 * MS, 0).is_empty());
    assert!(p.overdue(1_300 * MS, 5_000).is_empty(), "not yet past");
    assert!(
        p.overdue(1_320 * MS, 10_000).is_empty(),
        "the silence starts"
    );
    assert!(p
        .overdue(1_320 * MS, 10_000 + LANE_SILENCE_MS - 1)
        .is_empty());
    // A frame in time clears it.
    p.frame("a", 1_200 * MS);
    p.free("a");
    assert!(p.overdue(1_320 * MS, 10_000 + LANE_SILENCE_MS).is_empty());
    assert!(p.overdue(1_420 * MS, 20_000).is_empty());
    // A busy lane is in inference, not silent.
    p.frame("a", 1_300 * MS);
    assert!(p.overdue(1_600 * MS, 30_000).is_empty());
    assert!(p.overdue(1_600 * MS, 40_000).is_empty());
    p.free("a");
    assert!(p.overdue(1_600 * MS, 50_000).is_empty());
    assert_eq!(
        p.overdue(1_600 * MS, 50_000 + LANE_SILENCE_MS),
        vec!["a".to_owned()]
    );
    assert_eq!(p.done_target("a"), None, "it left the barrier");
}

// Under World Engine's default participant timeout (2 s).
const _: () = assert!(LANE_SILENCE_MS < 2_000);

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
    // is due (1100 + 200 + 50): the camera registers and reports 1200.
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
    assert_eq!(read_done(&mut s).await, 1_350 * MS);
    // A lost source leaves the barrier: the connection closes.
    p.lost("cam_fl");
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(Duration::from_secs(3), s.read(&mut b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn a_report_after_the_lane_was_lost_registers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(ados_time::PARTICIPANTS_SOCKET);
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let p = SimPacing::new(Clock::Sim(SimFeed::new()), Some(100), Some(sock), tx);
    assert!(p.frame("cam_fl", &desc(Some(1_000))));
    p.free("cam_fl");
    assert!(p.frame("cam_fl", &desc(Some(1_100))));
    p.lost("cam_fl");
    // The frame in flight finishes after its source was lost.
    p.free("cam_fl");
    p.report("cam_fl");
    assert!(
        p.parts_lock().is_empty(),
        "a lost lane stays out of the barrier"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "nothing connected"
    );
}

#[tokio::test]
async fn a_silent_lockstep_lane_is_dropped_by_the_watchdog() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(ados_time::PARTICIPANTS_SOCKET);
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let feed = SimFeed::new();
    let (tx, _rx) = broadcast::channel(8);
    let p = SimPacing::new(Clock::Sim(feed.clone()), Some(100), Some(sock), tx);
    let at = Instant::now();
    feed.ingest(&tick(1, 1_000), at);
    assert!(p.frame("cam_fl", &desc(Some(1_000))));
    p.free("cam_fl");
    assert!(p.frame("cam_fl", &desc(Some(1_100))));
    p.free("cam_fl");
    let (mut s, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read_done(&mut s).await, 1_350 * MS);
    // The simulator passed 1300 and frame 1200 never comes.
    feed.ingest(&tick(2, 1_320), Instant::now());
    let mut b = [0u8; 1];
    let n = tokio::time::timeout(
        Duration::from_millis(LANE_SILENCE_MS + 1_500),
        s.read(&mut b),
    )
    .await
    .expect("the lane left the barrier within the silence bound")
    .unwrap();
    assert_eq!(n, 0);
}
