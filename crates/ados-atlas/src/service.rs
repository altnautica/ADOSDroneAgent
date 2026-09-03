//! The capture loop: pull a frame, tag it with the latest pose, feed the capture
//! session, and publish the pose (every frame), the keyframe (when one is
//! selected), and the capture state (on change) onto the atlas bus.
//!
//! The expensive keyframe image encode runs only when the session WOULD select
//! the frame (the [`CaptureSession::would_select`] peek), so a non-keyframe
//! frame costs only a pose-stream update, not a JPEG.
//!
//! The loop also accepts operator control commands (start / stop / pause /
//! resume / status) off an mpsc channel the control socket feeds, so the GCS can
//! drive the session at runtime. The loop is the single owner of the session, so
//! every mutation — a frame ingest or a control command — happens here.

use std::sync::Arc;
use std::time::Duration;

use ados_offload::{FreshnessGate, GateState};
use ados_protocol::atlas::{CaptureState, ImageEncoding, KeyframeImage, TimeAlignment, VioHealth};
use tokio::sync::{mpsc, Notify};

use crate::control::AtlasControlCmd;
use crate::encode::encode_keyframe_jpeg;
use crate::frame_source::{AtlasFrameSource, CapturedFrame};
use crate::pose_source::{clock_offset_ns, mono_ns, PoseProvider, PoseSample};
use crate::publish::AtlasPublisher;
use crate::runtime::AtlasRuntimeConfig;
use crate::session::{CaptureSession, FrameInput, PoseInput};

/// A globally-unique session id every run's keyframes share. Embeds the drone's
/// device id so two drones streaming to ONE shared compute node never collide on
/// a bare timestamp: the compute node keys its on-disk dataset, its in-memory
/// session→device map, and the cloud `cmd_atlasJobs` row on this id, so a bare
/// `atlas-{ms}` from two drones would corrupt one reconstruction and overwrite
/// the other's job row. The device id is sanitized to id-safe chars (it becomes a
/// dataset dir name and an upsert key). When the device id is genuinely
/// unavailable a process + sub-millisecond-nanosecond nonce stands in so the id
/// is never a bare millisecond two drones could mint identically. Shared by the
/// daemon's initial auto-started session and each operator-initiated `start`.
pub fn new_session_id(device_id: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let ms = now.as_millis();
    let sanitized = sanitize_id(device_id);
    if sanitized.is_empty() {
        // No device id: a process id + the sub-millisecond nanos keep the id
        // unique across concurrent drones even without an attribution — never a
        // bare millisecond two drones could mint identically.
        let nonce = format!("{}{}", std::process::id(), now.subsec_nanos());
        format!("atlas-n{nonce}-{ms}")
    } else {
        format!("atlas-{sanitized}-{ms}")
    }
}

/// Reduce a device id to id-safe chars (ASCII alphanumerics, `-`, `_`), mapping
/// anything else to `-` and trimming leading/trailing separators, so the session
/// id stays safe as a dataset directory name, a URL path, and an upsert key,
/// then append a short hash of the ORIGINAL id.
///
/// The hash suffix is load-bearing. Mapping every non-id-safe character to `-`
/// is many-to-one: `dr one/7`, `dr-one-7` and `dr/one/7` all collapse to
/// `dr-one-7`, so two genuinely different drones streaming to one shared compute
/// node would mint the same session-id prefix and corrupt each other's dataset —
/// the exact collision the session id exists to prevent. The hash is of the
/// pre-sanitization id, so distinct inputs stay distinct.
fn sanitize_id(id: &str) -> String {
    let mapped: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('-');
    if trimmed.is_empty() {
        return String::new();
    }
    format!("{trimmed}-{:06x}", short_hash(id))
}

/// A 24-bit FNV-1a hash of `s`, for disambiguating a sanitized id. Not a
/// security primitive: it only has to separate ids a lossy character map merged.
fn short_hash(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h & 0x00ff_ffff
}

/// What we publish capture state on a change of (so we re-publish only when the
/// operator-visible status actually moved, not every frame).
#[derive(PartialEq, Clone, Copy)]
struct StateKey {
    state: CaptureState,
    keyframes: u64,
    health: VioHealth,
    /// The keyframe cap stops selection while state and count both freeze, so
    /// without this in the key the transition is never republished and the
    /// operator surface shows "capturing" against a frozen count forever.
    capped: bool,
    /// The anchor latch gates keyframe selection, so the operator must see it
    /// flip — before it, a capture is running and producing nothing.
    anchored: bool,
    dropped_keyframes: u64,
}

/// One iteration's selected work: an operator control command, or the next
/// frame. `biased` selection checks shutdown, then control, then a frame, so a
/// pending control command is never starved by a busy frame source.
enum LoopStep {
    Control(Option<AtlasControlCmd>),
    Frame(Option<CapturedFrame>),
}

/// Finalize + bag a live session so the compute node's reconstruct trigger
/// (which fires only on [`CaptureState::Bagged`]) sees the session end.
/// `finalize()` ALONE would strand the session in `Finalizing` and no
/// reconstruction would ever run. Valid from capturing or paused; a session that
/// is already `Bagged` (an explicit stop) or `Idle` is left untouched. Returns
/// whether the session transitioned, so the caller publishes the new `Bagged`
/// state exactly once — a repeated stop (or a stop-then-shutdown) does not
/// re-announce `Bagged` and so cannot re-trigger reconstruction.
pub(crate) fn stop_and_bag(session: &mut CaptureSession) -> bool {
    if matches!(
        session.state(),
        CaptureState::Capturing | CaptureState::Paused
    ) {
        session.finalize();
        session.mark_bagged();
        true
    } else {
        false
    }
}

/// A brief grace period on shutdown. The bus broadcast enqueues each frame onto
/// the subscriber's writer task, which flushes to the socket asynchronously.
/// Sleeping this long after the final publish lets that writer deliver the
/// terminal `Bagged` frame before this function returns and drops the publisher
/// (dropping it aborts the writer). The forwarder must receive the terminal
/// `Bagged` to trigger reconstruction on a shutdown-while-capturing; a small
/// bounded grace on an already-stopping service is a cheap correctness guarantee.
const SHUTDOWN_FLUSH_GRACE: Duration = Duration::from_millis(150);

/// Run the capture loop until `cancel` is notified. Starts a fresh session under
/// `session_id`, then drives frames from `frames`, posing each with `pose`, and
/// publishes onto `publisher`. Operator commands arrive on `control_rx`. On
/// shutdown a still-live session is finalized AND bagged so the final `Bagged`
/// state is published (the reconstruct trigger fires), then the loop returns.
#[allow(clippy::too_many_arguments)]
pub async fn run_capture_loop(
    mut frames: AtlasFrameSource,
    pose: Arc<dyn PoseProvider>,
    publisher: AtlasPublisher,
    mut session: CaptureSession,
    runtime: AtlasRuntimeConfig,
    session_id: String,
    mut control_rx: mpsc::Receiver<AtlasControlCmd>,
    cancel: Arc<Notify>,
) {
    session.start(session_id);
    let mut last_key = state_key(&session);
    publisher.publish_capture_state(&session.status()).await;

    // The pose freshness gate. Anchored on THIS host's monotonic clock, so a
    // producer that stops publishing while its socket stays up is caught: the
    // socket-drop path clears the cache, but a wedged producer never drops the
    // socket and only an age check sees it. Without this, a flight-controller
    // link that dies mid-flight left `pose.latest()` returning the last pose
    // forever, and EVERY subsequent frame was tagged with that frozen pose and
    // entered the reconstruction as truth.
    let mut pose_gate = FreshnessGate::new(runtime.pose_max_age_ms);

    // Whether the control channel is still open. A closed channel (its sender
    // dropped — e.g. the control socket failed to bind) makes `recv()` resolve
    // immediately forever, so once it closes we disable the branch rather than
    // spin: the loop keeps serving frames with no control input.
    let mut control_open = true;

    loop {
        let step = tokio::select! {
            biased;
            _ = cancel.notified() => break,
            cmd = control_rx.recv(), if control_open => LoopStep::Control(cmd),
            f = frames.next() => LoopStep::Frame(f),
        };

        match step {
            LoopStep::Control(Some(cmd)) => {
                handle_control(
                    &mut session,
                    &publisher,
                    &mut last_key,
                    cmd,
                    &runtime.device_id,
                )
                .await;
            }
            LoopStep::Control(None) => {
                // The control channel closed (its sender was dropped). Stop
                // selecting on it so the loop does not busy-spin; keep serving
                // frames.
                control_open = false;
            }
            LoopStep::Frame(None) => {
                // The source needs a moment (a real source reconnecting, or an
                // exhausted synthetic sequence). Back off, but wake on shutdown.
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                    _ = cancel.notified() => break,
                }
            }
            LoopStep::Frame(Some(f)) => {
                process_frame(
                    &mut session,
                    &publisher,
                    &runtime,
                    &pose,
                    &mut pose_gate,
                    f,
                    &mut last_key,
                )
                .await;
            }
        }
    }

    // Shutdown: if the session was still live, finalize AND bag so the compute
    // node's reconstruct trigger (Bagged-only) fires — finalize() alone would
    // strand it in Finalizing. An already-bagged (explicit stop) or idle session
    // is left as-is and NOT re-announced.
    if stop_and_bag(&mut session) {
        publisher.publish_capture_state(&session.status()).await;
        // Let the bus writer flush the terminal Bagged frame to subscribers before
        // the publisher is dropped (drop aborts the writer). See SHUTDOWN_FLUSH_GRACE.
        tokio::time::sleep(SHUTDOWN_FLUSH_GRACE).await;
    }
    tracing::info!("atlas capture loop stopped");
}

/// Apply one operator control command to the session and publish the resulting
/// capture state when it changed. `Status` is a pure read that never blocks the
/// loop and never publishes; `Start`/`Stop` always publish their new state, and
/// `Pause`/`Resume` publish only on a real transition.
async fn handle_control(
    session: &mut CaptureSession,
    publisher: &AtlasPublisher,
    last_key: &mut StateKey,
    cmd: AtlasControlCmd,
    device_id: &str,
) {
    let publish = match cmd {
        AtlasControlCmd::Status(reply) => {
            let _ = reply.send(session.status());
            false
        }
        AtlasControlCmd::Start => {
            session.start(new_session_id(device_id));
            true
        }
        AtlasControlCmd::Stop => stop_and_bag(session),
        AtlasControlCmd::Pause => {
            let before = session.state();
            session.pause();
            session.state() != before
        }
        AtlasControlCmd::Resume => {
            let before = session.state();
            session.resume();
            session.state() != before
        }
    };
    if publish {
        publisher.publish_capture_state(&session.status()).await;
        *last_key = state_key(session);
    }
}

/// Build the [`TimeAlignment`] for a frame paired with `ps`.
///
/// `ts_monotonic_ns` is this host's monotonic instant for the frame,
/// `clock_offset_ns` lets a consumer rederive the wall stamp from it, and the
/// residual is the REAL measured gap between the frame and the pose it was
/// paired with, on the one clock where that subtraction means something.
///
/// `trigger_seq` is deliberately `None`: pairing a frame to a pose by the
/// flight controller's `CAMERA_TRIGGER.seq` requires the FC to be publishing
/// that message and the camera to be hardware-triggered from it, and neither is
/// wired on this platform. Claiming a sequence we did not join on would assert
/// an exact pairing where an interpolated one happened, so the field stays
/// absent and the residual carries the honest cost.
fn time_alignment(frame_mono_ns: i64, ps: &PoseSample, sigma_ns: i64) -> TimeAlignment {
    let residual_ns = frame_mono_ns - ps.arrival_mono_ns;
    TimeAlignment {
        ts_monotonic_ns: frame_mono_ns,
        clock_offset_ns: clock_offset_ns(),
        clock_offset_sigma_ns: sigma_ns,
        trigger_seq: None,
        pose_pairing_residual_ms: residual_ns as f64 / 1_000_000.0,
    }
}

/// Ingest one captured frame: tag it with the latest pose, encode a keyframe only
/// when the session would select it, feed the session, and publish the pose (+
/// keyframe) plus the capture state on a change.
///
/// A frame with no pose, or with a pose past its age budget, is DROPPED. Both
/// mean the same thing operationally — there is no current pose — and tagging a
/// frame with a stale one puts it into the reconstruction at a place the
/// aircraft was not.
#[allow(clippy::too_many_arguments)]
async fn process_frame(
    session: &mut CaptureSession,
    publisher: &AtlasPublisher,
    runtime: &AtlasRuntimeConfig,
    pose: &Arc<dyn PoseProvider>,
    pose_gate: &mut FreshnessGate,
    f: CapturedFrame,
    last_key: &mut StateKey,
) {
    // One monotonic read per frame, used for both the freshness decision and
    // the frame's own exposure stamp, so the two cannot disagree.
    let frame_mono_ns = mono_ns();
    let now_mono_ms = frame_mono_ns / 1_000_000;

    let Some(ps) = pose.latest() else {
        // No pose at all (the flight controller link never came up, or the
        // reader cleared its cache on disconnect). A frame cannot be pose-tagged,
        // so it is dropped rather than tagged with a guess.
        pose_gate.set_link(false);
        degrade_to_lost(session, publisher, last_key).await;
        return;
    };

    // The pose exists; record its arrival on the gate's clock and ask whether it
    // is still inside the age budget.
    pose_gate.set_link(true);
    pose_gate.record(ps.arrival_mono_ns / 1_000_000);
    if pose_gate.state(now_mono_ms) != GateState::Fresh {
        // A pose past its budget is treated as ABSENT, never held forward and
        // never extrapolated: the socket may still be up while the producer has
        // stopped, which is exactly the stall a liveness check cannot see.
        tracing::debug!(
            camera = %f.camera_id,
            age_ms = now_mono_ms.saturating_sub(ps.arrival_mono_ns / 1_000_000),
            budget_ms = runtime.pose_max_age_ms,
            "atlas_frame_dropped_stale_pose"
        );
        degrade_to_lost(session, publisher, last_key).await;
        return;
    }

    session.set_vio_health(ps.health);
    session.set_pose_tier(ps.source);
    // The anchor latch is observable from the sample: the reader fills `anchor`
    // once the first 3D fix lands and every later sample carries it.
    session.set_anchored(ps.anchor.is_some());

    // Encode the JPEG only when this frame will actually become a keyframe, and
    // off the reactor: the per-pixel YUV->RGB + JPEG pass is tens of ms on a
    // companion-class CPU and must not block a worker thread.
    if session.would_select(&f.camera_id, &ps.pose, f.ts_ms) {
        let (w, h, fmt, bytes) = (f.width, f.height, f.format, f.bytes);
        let encoded =
            tokio::task::spawn_blocking(move || encode_keyframe_jpeg(w, h, fmt, &bytes)).await;
        let jpeg = match encoded {
            Ok(Ok(jpeg)) => jpeg,
            Ok(Err(e)) => {
                // A malformed frame cannot become a keyframe, but the pose stream
                // stays whole and the selector baseline is NOT advanced, so the
                // next good frame retries the keyframe at this point.
                tracing::warn!(camera = %f.camera_id, error = %e, "keyframe_encode_failed");
                publish_pose_only(session, publisher, &f.camera_id, &ps, f.ts_ms).await;
                return;
            }
            Err(join) => {
                tracing::error!(camera = %f.camera_id, error = %join, "keyframe_encode_task_panicked");
                publish_pose_only(session, publisher, &f.camera_id, &ps, f.ts_ms).await;
                return;
            }
        };
        // Compute the alignment before the sample's fields move into the frame
        // input; the alignment reads the pose's arrival instant, not its pose.
        let time = time_alignment(frame_mono_ns, &ps, runtime.pose_prior.clock_offset_sigma_ns);
        let fi = FrameInput {
            image: KeyframeImage {
                encoding: ImageEncoding::Jpeg,
                width: w,
                height: h,
                bytes: jpeg,
            },
            camera: runtime.intrinsics_for(&f.camera_id, w, h),
            pose: ps.pose,
            pose_cov: ps.cov,
            pose_source: ps.source,
            time,
            global_anchor: ps.anchor,
            imu_window: Vec::new(),
        };
        if let Some(out) = session.on_frame(&f.camera_id, fi, f.ts_ms) {
            if let Some(pd) = &out.pose {
                publisher.publish_pose(pd).await;
            }
            if let Some(kf) = out.keyframe {
                let dropped = publisher.publish_keyframe(&kf).await;
                session.note_keyframe_dropped(dropped);
            }
        }
    } else {
        // Not a keyframe: the pose-only path, which builds no image, resolves no
        // intrinsics and allocates nothing per frame.
        publish_pose_only(session, publisher, &f.camera_id, &ps, f.ts_ms).await;
    }

    // Re-publish capture state only when the operator-visible status moved.
    let key = state_key(session);
    if key != *last_key {
        publisher.publish_capture_state(&session.status()).await;
        *last_key = key;
    }
}

/// Feed the session the pose side of a frame and publish the descriptor when
/// the rig-wide cadence is due.
async fn publish_pose_only(
    session: &mut CaptureSession,
    publisher: &AtlasPublisher,
    camera_id: &str,
    ps: &PoseSample,
    ts_ms: i64,
) {
    let input = PoseInput {
        pose: ps.pose.clone(),
        global_anchor: ps.anchor,
    };
    if let Some(pd) = session.on_pose_only(camera_id, input, ts_ms) {
        publisher.publish_pose(&pd).await;
    }
}

/// Report the pose stream as LOST and republish the capture state if that moved
/// the operator-visible status.
///
/// The status must say `lost` rather than hold the last health: a capture that
/// is producing no keyframes because it has no pose looks identical, from the
/// outside, to one that is simply between selections.
async fn degrade_to_lost(
    session: &mut CaptureSession,
    publisher: &AtlasPublisher,
    last_key: &mut StateKey,
) {
    session.set_vio_health(VioHealth::Lost);
    let key = state_key(session);
    if key != *last_key {
        publisher.publish_capture_state(&session.status()).await;
        *last_key = key;
    }
}

fn state_key(session: &CaptureSession) -> StateKey {
    let st = session.status();
    StateKey {
        state: st.state,
        keyframes: st.keyframes,
        health: st.vio_health,
        capped: st.capped,
        anchored: st.anchored,
        dropped_keyframes: st.dropped_keyframes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CameraConfig, CaptureConfig, CaptureProfile, SelectionParams};
    use crate::frame_source::SyntheticFrameSource;
    use crate::pose_source::PoseProvider;
    use ados_protocol::atlas::{AtlasEvent, CameraRole, CaptureStatus, ATLAS_CAPTURE_STATE_TOPIC};
    use ados_protocol::frame::PLUGIN_MAX_FRAME;
    use ados_protocol::ipc::read_length_prefixed;
    use tokio::net::UnixStream;

    fn cam(id: &str) -> CameraConfig {
        CameraConfig {
            id: id.into(),
            role: CameraRole::Primary,
            enabled: true,
            reconstruct: true,
        }
    }

    fn one_camera_session() -> CaptureSession {
        CaptureSession::new(CaptureConfig {
            cameras: vec![cam("front")],
            profile: CaptureProfile::Freeform,
            selection: SelectionParams::default(),
        })
    }

    /// A pose provider that never has a pose (the tests drive state through the
    /// control channel, not through frames, so no frame is ever pose-tagged).
    struct NoPose;
    impl PoseProvider for NoPose {
        fn latest(&self) -> Option<crate::pose_source::PoseSample> {
            None
        }
    }

    // ── new_session_id: globally-unique, device-scoped ───────────────────────

    #[test]
    fn session_id_embeds_the_sanitized_device_id() {
        let id = new_session_id("drone-7");
        assert!(id.starts_with("atlas-drone-7-"), "got {id}");
        // The trailing segment is the millisecond stamp — a bare number.
        let ms = id.rsplit('-').next().unwrap();
        assert!(ms.chars().all(|c| c.is_ascii_digit()), "got {id}");
    }

    #[test]
    fn session_id_sanitizes_unsafe_device_id_chars() {
        // Slashes / spaces would break a dataset dir name / URL path — mapped to '-'.
        let id = new_session_id("dr one/7:x");
        assert!(id.starts_with("atlas-dr-one-7-x-"), "got {id}");
    }

    #[test]
    fn session_id_falls_back_to_a_nonce_never_a_bare_ms() {
        // An empty (or all-unsafe) device id must NOT yield a bare `atlas-{ms}` two
        // drones could mint identically — a process+nanos nonce stands in.
        let id = new_session_id("");
        assert!(id.starts_with("atlas-n"), "got {id}");
        assert!(!id.starts_with("atlas-n-"), "the nonce is non-empty: {id}");
        assert!(new_session_id("---").starts_with("atlas-n"));
    }

    #[test]
    fn two_devices_never_collide_on_a_session_id() {
        assert_ne!(new_session_id("drone-a"), new_session_id("drone-b"));
    }

    // ── stop_and_bag: the shutdown / stop bag transition ──────────────────────

    #[test]
    fn stop_and_bag_bags_a_capturing_session() {
        let mut s = one_camera_session();
        s.start("sess-1".into());
        assert!(stop_and_bag(&mut s));
        assert_eq!(s.state(), CaptureState::Bagged);
    }

    #[test]
    fn stop_and_bag_bags_a_paused_session() {
        let mut s = one_camera_session();
        s.start("sess-2".into());
        s.pause();
        assert_eq!(s.state(), CaptureState::Paused);
        assert!(stop_and_bag(&mut s));
        assert_eq!(s.state(), CaptureState::Bagged);
    }

    #[test]
    fn stop_and_bag_is_a_noop_once_bagged_or_idle() {
        // Already bagged: no transition, no re-announce.
        let mut s = one_camera_session();
        s.start("sess-3".into());
        assert!(stop_and_bag(&mut s));
        assert!(!stop_and_bag(&mut s));
        assert_eq!(s.state(), CaptureState::Bagged);
        // Never started: idle stays idle.
        let mut idle = one_camera_session();
        assert!(!stop_and_bag(&mut idle));
        assert_eq!(idle.state(), CaptureState::Idle);
    }

    // ── integration: the control channel + shutdown drive the published state ─

    /// Connect a subscriber to the atlas bus and read the next capture-state
    /// event, skipping pose/keyframe frames. Bounded so a test never hangs.
    async fn next_capture_state(stream: &mut UnixStream) -> CaptureStatus {
        let read = async {
            loop {
                let payload = read_length_prefixed(stream, PLUGIN_MAX_FRAME, false)
                    .await
                    .expect("read")
                    .expect("frame");
                let ev = AtlasEvent::decode(&payload).expect("event");
                if ev.topic == ATLAS_CAPTURE_STATE_TOPIC {
                    return CaptureStatus::from_msgpack(&ev.payload).expect("status");
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(3), read)
            .await
            .expect("a capture state within 3s")
    }

    /// Spin up a capture loop bound to a temp atlas bus, with a control channel
    /// and a subscriber already connected. Returns the control sender, the cancel
    /// notify, the subscriber stream, the loop's join handle, and the tempdir
    /// (kept alive for the socket).
    async fn spawn_loop() -> (
        mpsc::Sender<AtlasControlCmd>,
        Arc<Notify>,
        UnixStream,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("atlas.sock");
        let sock_str = sock.to_str().unwrap().to_string();
        let publisher = AtlasPublisher::bind(&sock_str).await.unwrap();

        // Connect the subscriber and give the bus accept loop a beat to register
        // it before the loop publishes its first (Capturing) state.
        let subscriber = UnixStream::connect(&sock_str).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (tx, rx) = mpsc::channel(16);
        let cancel = Arc::new(Notify::new());
        let loop_cancel = cancel.clone();
        let handle = tokio::spawn(async move {
            run_capture_loop(
                AtlasFrameSource::Synthetic(SyntheticFrameSource::new(Vec::new())),
                Arc::new(NoPose),
                publisher,
                one_camera_session(),
                AtlasRuntimeConfig::default(),
                "sess-boot".to_string(),
                rx,
                loop_cancel,
            )
            .await;
        });
        (tx, cancel, subscriber, handle, dir)
    }

    #[tokio::test]
    async fn loop_starts_capturing_and_a_stop_publishes_bagged() {
        let (tx, cancel, mut sub, handle, _dir) = spawn_loop().await;

        // The loop auto-starts: the first published state is Capturing.
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Capturing
        );

        // A stop command finalizes + bags, publishing the Bagged state that the
        // compute node's reconstruct trigger fires on.
        tx.send(AtlasControlCmd::Stop).await.unwrap();
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Bagged
        );

        cancel.notify_waiters();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn pause_then_resume_publishes_each_transition() {
        let (tx, cancel, mut sub, handle, _dir) = spawn_loop().await;
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Capturing
        );

        tx.send(AtlasControlCmd::Pause).await.unwrap();
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Paused
        );

        tx.send(AtlasControlCmd::Resume).await.unwrap();
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Capturing
        );

        cancel.notify_waiters();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_while_capturing_reaches_bagged() {
        let (_tx, cancel, mut sub, handle, _dir) = spawn_loop().await;
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Capturing
        );

        // A shutdown with a live session must publish the final Bagged state, not
        // leave it stranded — the fix this loop exists to prove.
        cancel.notify_waiters();
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Bagged
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_start_command_restarts_a_bagged_session() {
        let (tx, cancel, mut sub, handle, _dir) = spawn_loop().await;
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Capturing
        );

        tx.send(AtlasControlCmd::Stop).await.unwrap();
        assert_eq!(
            next_capture_state(&mut sub).await.state,
            CaptureState::Bagged
        );

        // Start after a stop resets to a fresh capturing session.
        tx.send(AtlasControlCmd::Start).await.unwrap();
        let st = next_capture_state(&mut sub).await;
        assert_eq!(st.state, CaptureState::Capturing);
        assert_eq!(st.keyframes, 0);

        cancel.notify_waiters();
        handle.await.unwrap();
    }
}
