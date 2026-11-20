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
    p.free("a", 1_000 * MS);
    assert_eq!(p.done_target("a", 0), None, "the period is not known yet");
    p.frame("a", 1_100 * MS);
    assert_eq!(
        p.done_target("a", 0),
        Some(1_100 * MS),
        "a frame in inference holds the lane at its own time"
    );
    p.free("a", 1_100 * MS);
    // The next frame (1200) reaches the engine once 1400 is rendered: the
    // source is two frames deep.
    assert_eq!(p.depth("a"), SOURCE_DEPTH_FRAMES);
    assert_eq!(p.done_target("a", 0), Some(1_450 * MS));
    // A held batch of this camera releases before then: the lane stops there.
    p.hold(1_200 * MS, batch("a", Some(1_100)));
    p.hold(1_150 * MS, batch("b", Some(1_000)));
    assert_eq!(p.done_target("a", 0), Some(1_200 * MS));
    p.due(1_200 * MS);
    assert_eq!(p.done_target("a", 0), Some(1_450 * MS));
    // Two frames taken, the later one finished: the earlier holds the lane.
    p.frame("a", 1_200 * MS);
    p.frame("a", 1_300 * MS);
    p.free("a", 1_300 * MS);
    assert_eq!(p.done_target("a", 0), Some(1_200 * MS));
    p.free("a", 1_200 * MS);
    assert_eq!(p.done_target("a", 0), Some(1_650 * MS));
    p.lost("a");
    assert_eq!(
        p.done_target("a", 0),
        None,
        "a lost source leaves the barrier"
    );
}

/// With a modelled latency longer than the source's depth, the simulator
/// runs on while a frame is inferred, up to that frame's release time (the
/// result is published then, so no later), and between frames up to the next
/// frame's release time.
#[test]
fn the_simulator_runs_on_to_the_release_time_while_a_frame_is_inferred() {
    const L: u64 = 217 * MS;
    let mut p = Pacing::default();
    p.frame("a", 1_000 * MS);
    p.free("a", 1_000 * MS);
    p.frame("a", 1_100 * MS);
    assert_eq!(p.done_target("a", L), Some(1_317 * MS), "1100 + latency");
    // Frames taken and skipped while it is inferred change nothing.
    p.frame("a", 1_200 * MS);
    p.free("a", 1_200 * MS);
    assert_eq!(p.done_target("a", L), Some(1_317 * MS));
    // Its result is held to its release; once out, the lane waits for the
    // next frame (1300): its arrival (1200 + 300 + 50) is later than its
    // release (1300 + 217).
    p.hold(1_317 * MS, batch("a", Some(1_100)));
    p.free("a", 1_100 * MS);
    assert_eq!(p.done_target("a", L), Some(1_317 * MS));
    p.due(1_317 * MS);
    assert_eq!(p.done_target("a", L), Some(1_550 * MS));
    // A latency longer than the source's depth: the next frame's release.
    assert_eq!(p.done_target("a", 400 * MS), Some(1_700 * MS));
}

/// Drive a rendered lockstep run through the agent's own barrier
/// ([`ados_time::barrier::Barrier`]) with one camera of period `period_ns`
/// whose source is `source_depth` frames deep (frame k reaches the engine
/// only once frame k + depth is drawn), reporting `target`, World Engine
/// granting `grant_ns` at a time: the run holds at a boundary until the
/// barrier acknowledges it and draws a frame on the step past its sim time (a
/// frame at the boundary it holds at is not drawn yet). While the run holds,
/// wall time passes in [`LANE_CHECK_MS`] steps through [`Pacing::overdue`],
/// as the engine's check does. Returns the sim time the run reached, ns (it
/// stops at `until_ns` or where the barrier stalls for good), and the wall
/// time it held, ms.
fn barrier_run(
    p: &mut Pacing,
    period_ns: u64,
    source_depth: u64,
    grant_ns: u64,
    until_ns: u64,
    target: impl Fn(&Pacing) -> Option<u64>,
) -> (u64, u64) {
    let mut b = ados_time::barrier::Barrier::default();
    let from: std::net::SocketAddr = "127.0.0.1:14650".parse().unwrap();
    let now = Instant::now();
    let (mut sim, mut delivered, mut seq, mut wall_ms, mut held_ms) =
        (0u64, 0u64, 0u32, 0u64, 0u64);
    let mut id = None;
    let mut checks = 0;
    while sim < until_ns {
        while (delivered + source_depth) * period_ns < sim {
            p.frame("a", delivered * period_ns);
            p.free("a", (delivered) * period_ns);
            delivered += 1;
        }
        match target(p) {
            Some(done) => {
                let id = *id.get_or_insert_with(|| b.join().unwrap());
                b.done(id, done, 0, now);
            }
            None => {
                if let Some(i) = id.take() {
                    b.leave(i, now);
                }
            }
        }
        seq += 1;
        let tick = wire::Tick {
            flags: wire::FLAG_LOCKSTEP,
            seq,
            sim_ns: sim,
            grant_ns: sim + grant_ns,
            speed: 0.0,
        };
        if let Some((ack, _)) = b.tick(&tick, from, now) {
            sim = ack.done_ns;
            checks = 0;
            continue;
        }
        // Held: only the lane's check moves anything now.
        checks += 1;
        if checks > 40 {
            return (sim, held_ms);
        }
        wall_ms += LANE_CHECK_MS;
        held_ms += LANE_CHECK_MS;
        p.overdue(sim, wall_ms);
    }
    (sim, held_ms)
}

/// The barrier keeps advancing against the RTSP source's two-frame depth for
/// any grant, without ever holding on a missing frame: the run holds only at
/// a boundary at or past the lane's target, which is past the frame that
/// releases the next one.
#[test]
fn the_barrier_keeps_advancing_with_a_two_frame_deep_source_for_any_grant() {
    const UNTIL: u64 = 20_000 * MS;
    for (fps, grant_ms) in [(30, 20), (60, 20), (30, 1), (30, 50), (10, 20), (60, 100)] {
        for depth in [1, 2] {
            let period = 1_000 * MS / fps;
            let mut p = Pacing::default();
            let (reached, held) = barrier_run(&mut p, period, depth, grant_ms * MS, UNTIL, |p| {
                p.done_target("a", 0)
            });
            let at = format!("{fps} fps, grant {grant_ms} ms, {depth} deep");
            assert!(reached >= UNTIL, "{at}: stalled at {} ms", reached / MS);
            assert_eq!(held, 0, "{at}: the run held on a missing frame");
            assert_eq!(p.depth("a"), SOURCE_DEPTH_FRAMES, "{at}");
        }
    }
}

/// A deeper source than expected holds the run one probe per missing frame
/// of depth, then the lane has learned it and the run never holds again.
#[test]
fn a_deeper_source_is_learned_and_the_barrier_keeps_advancing() {
    const UNTIL: u64 = 20_000 * MS;
    for (fps, grant_ms) in [(30, 20), (60, 20), (30, 50)] {
        for depth in [3, 4] {
            let period = 1_000 * MS / fps;
            let mut p = Pacing::default();
            let (reached, held) = barrier_run(&mut p, period, depth, grant_ms * MS, UNTIL, |p| {
                p.done_target("a", 0)
            });
            let at = format!("{fps} fps, grant {grant_ms} ms, {depth} deep");
            assert!(reached >= UNTIL, "{at}: stalled at {} ms", reached / MS);
            assert_eq!(p.depth("a"), depth, "{at}");
            let probes = depth - SOURCE_DEPTH_FRAMES;
            assert!(
                held <= probes * (LANE_PROBE_MS + 2 * LANE_CHECK_MS),
                "{at}: held {held} ms"
            );
        }
    }
}

/// A source deeper than a lane learns: the lane leaves the barrier rather
/// than hold the run, and the run goes on.
#[test]
fn a_source_deeper_than_the_deepest_never_holds_the_run_for_good() {
    let period = 1_000 * MS / 30;
    let mut p = Pacing::default();
    let (reached, _) = barrier_run(
        &mut p,
        period,
        MAX_SOURCE_DEPTH_FRAMES + 1,
        20 * MS,
        5_000 * MS,
        |p| p.done_target("a", 0),
    );
    assert!(reached >= 5_000 * MS, "stalled at {} ms", reached / MS);
}

/// The target that counted one frame of source delay never lets the frame it
/// waits for be rendered from a two-frame-deep source: the run holds for good
/// (in a rendered lockstep run only the silence drop moved it, 1.25 s of wall
/// time a frame).
#[test]
fn one_frame_of_source_delay_deadlocks_a_two_frame_deep_source() {
    for (fps, grant_ms) in [(30, 20), (60, 20)] {
        let period = 1_000 * MS / fps;
        let mut p = Pacing::default();
        let (reached, _) = barrier_run(&mut p, period, 2, grant_ms * MS, 20_000 * MS, |p| {
            // Depth one whatever was learned: what the lane reported before.
            p.done_target("a", 0)
                .map(|d| d - (p.depth("a") - 1) * period)
        });
        assert!(
            reached < 1_000 * MS,
            "{fps} fps, grant {grant_ms} ms: expected a stall, reached {} ms",
            reached / MS
        );
    }
}

#[test]
fn a_lane_held_past_its_frame_learns_depth_then_leaves_the_barrier() {
    let mut p = Pacing::default();
    p.frame("a", 1_000 * MS);
    p.free("a", 1_000 * MS);
    p.frame("a", 1_100 * MS);
    p.free("a", 1_100 * MS);
    // Frame 1200 is expected once 1400 is rendered: not overdue before then.
    assert!(p.overdue(1_400 * MS, 0).is_empty());
    assert!(p.overdue(1_400 * MS, 5_000).is_empty(), "not yet past");
    assert!(p.overdue(1_420 * MS, 10_000).is_empty(), "the wait starts");
    assert!(p.overdue(1_420 * MS, 10_000 + LANE_PROBE_MS - 1).is_empty());
    // A frame in time clears it.
    p.frame("a", 1_200 * MS);
    p.free("a", 1_200 * MS);
    assert!(p.overdue(1_420 * MS, 10_000 + LANE_PROBE_MS).is_empty());
    // A busy lane is in inference, not waiting.
    p.frame("a", 1_300 * MS);
    assert!(p.overdue(1_700 * MS, 20_000).is_empty());
    assert!(p.overdue(1_700 * MS, 30_000).is_empty());
    p.free("a", 1_300 * MS);
    // Held past 1600 with no frame: one frame deeper per probe.
    assert!(p.overdue(1_620 * MS, 40_000).is_empty());
    assert_eq!(
        p.overdue(1_620 * MS, 40_000 + LANE_PROBE_MS),
        vec![Overdue::Deeper("a".into(), 3)]
    );
    assert_eq!(p.done_target("a", 0), Some(1_750 * MS));
    assert!(
        p.overdue(1_620 * MS, 50_000).is_empty(),
        "not past 1700 yet"
    );
    assert!(p.overdue(1_720 * MS, 50_000).is_empty());
    assert_eq!(
        p.overdue(1_720 * MS, 50_000 + LANE_PROBE_MS),
        vec![Overdue::Deeper("a".into(), 4)]
    );
    // At the deepest, the silence bound: then it leaves the barrier.
    assert!(p.overdue(1_820 * MS, 60_000).is_empty());
    assert!(p
        .overdue(1_820 * MS, 60_000 + LANE_SILENCE_MS - 1)
        .is_empty());
    assert_eq!(
        p.overdue(1_820 * MS, 60_000 + LANE_SILENCE_MS),
        vec![Overdue::Gone("a".into())]
    );
    assert_eq!(p.done_target("a", 0), None, "it left the barrier");
    // Back with its frames, it keeps what it learned.
    p.frame("a", 1_400 * MS);
    p.free("a", 1_400 * MS);
    p.frame("a", 1_500 * MS);
    p.free("a", 1_500 * MS);
    assert_eq!(p.done_target("a", 0), Some(2_050 * MS));
}

// Under World Engine's default participant timeout (2 s), probing included.
const _: () = assert!(
    (MAX_SOURCE_DEPTH_FRAMES - SOURCE_DEPTH_FRAMES) * (LANE_PROBE_MS + LANE_CHECK_MS) < 2_000
        && LANE_SILENCE_MS + LANE_CHECK_MS < 2_000
);

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

/// `DONE` frames until one at `want_ns` (the participant writes its latest
/// report, so earlier lower ones may or may not show).
async fn read_done_until(s: &mut tokio::net::UnixStream, want_ns: u64) {
    loop {
        let d = read_done(s).await;
        assert!(d <= want_ns, "reported {d}, past {want_ns}");
        if d == want_ns {
            return;
        }
    }
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
    assert_eq!(p.frame("cam_fl", &desc(Some(1_000))), Some(1_000 * MS));
    p.free("cam_fl", 1_000 * MS);
    assert_eq!(
        p.frame("cam_fl", &desc(None)),
        None,
        "a host-clock frame takes no lane"
    );
    // The second frame: while it is inferred the simulator may run to its
    // release (1100 + 100); its batch is held to then, before the next frame
    // can arrive (1100 + 300 + 50): the camera registers and reports 1200.
    feed.ingest(&tick(2, 1_100), at);
    assert!(p.frame("cam_fl", &desc(Some(1_100))).is_some());
    p.publish(batch("cam_fl", Some(1_100)));
    p.free("cam_fl", 1_100 * MS);
    let (mut s, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read_done(&mut s).await, 1_200 * MS);
    // The simulator reaches the release: the batch goes out and the camera
    // waits for its next frame.
    feed.ingest(&tick(3, 1_200), at);
    assert_eq!(recv(&mut rx).await.unwrap().ts_sensor_ms, Some(1_100));
    assert_eq!(read_done(&mut s).await, 1_450 * MS);
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
    assert!(p.frame("cam_fl", &desc(Some(1_000))).is_some());
    p.free("cam_fl", 1_000 * MS);
    assert!(p.frame("cam_fl", &desc(Some(1_100))).is_some());
    // It registered for the frame in flight; the source is lost meanwhile.
    p.lost("cam_fl");
    // The frame in flight finishes after its source was lost.
    p.free("cam_fl", 1_100 * MS);
    p.report("cam_fl");
    assert!(
        p.parts_lock().is_empty(),
        "a lost lane stays out of the barrier"
    );
    // At most the connection of the participant the loss dropped: none
    // registers again.
    let mut conns = 0;
    while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await
    {
        conns += 1;
    }
    assert!(conns <= 1, "{conns} connections");
}

#[tokio::test]
async fn a_lockstep_lane_held_past_its_frame_learns_depth_then_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join(ados_time::PARTICIPANTS_SOCKET);
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let feed = SimFeed::new();
    let (tx, _rx) = broadcast::channel(8);
    let p = SimPacing::new(Clock::Sim(feed.clone()), Some(100), Some(sock), tx);
    feed.ingest(&tick(1, 1_000), Instant::now());
    assert!(p.frame("cam_fl", &desc(Some(1_000))).is_some());
    p.free("cam_fl", 1_000 * MS);
    assert!(p.frame("cam_fl", &desc(Some(1_100))).is_some());
    p.free("cam_fl", 1_100 * MS);
    let (mut s, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    // The simulator holds just past each target and frame 1200 never comes:
    // the camera reports one frame later per probe, then leaves.
    for (seq, done_ms) in (2..).zip([1_450, 1_550, 1_650]) {
        read_done_until(&mut s, done_ms * MS).await;
        feed.ingest(&tick(seq, done_ms - 30), Instant::now());
    }
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
