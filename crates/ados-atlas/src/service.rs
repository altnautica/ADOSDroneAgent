//! The capture loop: pull a frame, tag it with the latest pose, feed the capture
//! session, and publish the pose (every frame), the keyframe (when one is
//! selected), and the capture state (on change) onto the atlas bus.
//!
//! The expensive keyframe image encode runs only when the session WOULD select
//! the frame (the [`CaptureSession::would_select`] peek), so a non-keyframe
//! frame costs only a pose-stream update, not a JPEG.

use std::sync::Arc;
use std::time::Duration;

use ados_protocol::atlas::{ImageEncoding, KeyframeImage, PoseDescriptor, VioHealth};
use tokio::sync::Notify;

use crate::encode::encode_keyframe_jpeg;
use crate::frame_source::AtlasFrameSource;
use crate::pose_source::PoseProvider;
use crate::publish::AtlasPublisher;
use crate::runtime::AtlasRuntimeConfig;
use crate::session::{CaptureSession, FrameInput};

/// What we publish capture state on a change of (so we re-publish only when the
/// operator-visible status actually moved, not every frame).
#[derive(PartialEq, Clone, Copy)]
struct StateKey {
    state: ados_protocol::atlas::CaptureState,
    keyframes: u64,
    health: VioHealth,
}

/// Run the capture loop until `cancel` is notified. Starts a fresh session under
/// `session_id`, then drives frames from `frames`, posing each with `pose`, and
/// publishes onto `publisher`. On shutdown it finalizes the session and publishes
/// the final state.
pub async fn run_capture_loop(
    mut frames: AtlasFrameSource,
    pose: Arc<dyn PoseProvider>,
    publisher: AtlasPublisher,
    mut session: CaptureSession,
    runtime: AtlasRuntimeConfig,
    session_id: String,
    cancel: Arc<Notify>,
) {
    session.start(session_id);
    let mut last_key = state_key(&session);
    publisher.publish_capture_state(&session.status()).await;

    loop {
        let frame = tokio::select! {
            biased;
            _ = cancel.notified() => break,
            f = frames.next() => f,
        };
        let Some(f) = frame else {
            // The source needs a moment (a real source reconnecting, or an
            // exhausted synthetic sequence). Back off, but wake on shutdown.
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                _ = cancel.notified() => break,
            }
            continue;
        };

        let Some(ps) = pose.latest() else {
            // No pose yet (the flight controller link is not up): a frame cannot
            // be pose-tagged, so it is dropped rather than tagged with a guess.
            continue;
        };
        session.set_vio_health(ps.health);

        let intrinsics = runtime.intrinsics_for(&f.camera_id, f.width, f.height);

        // Encode the JPEG only when this frame will actually become a keyframe,
        // and off the reactor: the per-pixel YUV->RGB + JPEG pass is tens of ms
        // on a companion-class CPU and must not block a worker thread.
        if session.would_select(&f.camera_id, &ps.pose, f.ts_ms) {
            let (w, h, fmt, bytes) = (f.width, f.height, f.format, f.bytes);
            let encoded =
                tokio::task::spawn_blocking(move || encode_keyframe_jpeg(w, h, fmt, &bytes)).await;
            let jpeg = match encoded {
                Ok(Ok(jpeg)) => jpeg,
                Ok(Err(e)) => {
                    // A malformed frame cannot become a keyframe, but the pose
                    // stream stays whole and the selector baseline is NOT advanced,
                    // so the next good frame retries the keyframe at this point.
                    tracing::warn!(camera = %f.camera_id, error = %e, "keyframe_encode_failed");
                    publisher
                        .publish_pose(&PoseDescriptor {
                            pose: ps.pose,
                            anchor: ps.anchor,
                            ts_ms: f.ts_ms,
                        })
                        .await;
                    continue;
                }
                Err(join) => {
                    tracing::error!(camera = %f.camera_id, error = %join, "keyframe_encode_task_panicked");
                    publisher
                        .publish_pose(&PoseDescriptor {
                            pose: ps.pose,
                            anchor: ps.anchor,
                            ts_ms: f.ts_ms,
                        })
                        .await;
                    continue;
                }
            };
            let fi = FrameInput {
                image: KeyframeImage {
                    encoding: ImageEncoding::Jpeg,
                    width: w,
                    height: h,
                    bytes: jpeg,
                },
                camera: intrinsics,
                pose: ps.pose,
                pose_source: ps.source,
                global_anchor: ps.anchor,
                imu_window: Vec::new(),
            };
            if let Some(out) = session.on_frame(&f.camera_id, fi, f.ts_ms) {
                publisher.publish_pose(&out.pose).await;
                if let Some(kf) = out.keyframe {
                    publisher.publish_keyframe(&kf).await;
                }
            }
        } else {
            // Not a keyframe: feed on_frame an empty image (unused) so the pose
            // stream still flows, with no encode.
            let fi = FrameInput {
                image: KeyframeImage {
                    encoding: ImageEncoding::Jpeg,
                    width: f.width,
                    height: f.height,
                    bytes: Vec::new(),
                },
                camera: intrinsics,
                pose: ps.pose,
                pose_source: ps.source,
                global_anchor: ps.anchor,
                imu_window: Vec::new(),
            };
            if let Some(out) = session.on_frame(&f.camera_id, fi, f.ts_ms) {
                publisher.publish_pose(&out.pose).await;
            }
        }

        // Re-publish capture state only when the operator-visible status moved.
        let key = state_key(&session);
        if key != last_key {
            publisher.publish_capture_state(&session.status()).await;
            last_key = key;
        }
    }

    session.finalize();
    publisher.publish_capture_state(&session.status()).await;
    tracing::info!("atlas capture loop stopped");
}

fn state_key(session: &CaptureSession) -> StateKey {
    let st = session.status();
    StateKey {
        state: st.state,
        keyframes: st.keyframes,
        health: st.vio_health,
    }
}
