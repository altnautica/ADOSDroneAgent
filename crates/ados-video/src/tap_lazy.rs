//! The lazy vision tap leg: the serving socket stays bound, and the decoding
//! ffmpeg runs only while a vision engine is attached.
//!
//! [`run_lazy_vision_tap`] waits for an engine to connect, spawns the decoder,
//! serves frames through [`crate::tap::serve_tap_session`], and stops the
//! decoder once the engine has been gone for the idle window. A decoder that
//! exits or stalls while an engine is attached is torn down; the engine's own
//! reconnect then brings up a fresh one after a fixed backoff, with no cap.
//! The orchestrator's health ladder only checks that this task is alive: no
//! engine attached is healthy-idle, not a fault.
//!
//! While an engine is attached the leg holds the
//! [`VISION_LEASE_HOLDER`] hero lease, so a thumbnail drone encodes at least
//! the hero frame rate for the tap. The lease is taken before the first
//! decoder starts (and the encoder switch is awaited, so the decoder does not
//! open a stream that is about to respawn), kept across a decoder restart, and
//! released when the engine has been gone for the idle window or the leg ends.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ados_protocol::framebus::FrameFormat;
use tokio::net::UnixListener;

use crate::process::ManagedProcess;
use crate::profile::ipc::APPLY_TIMEOUT;
use crate::profile::lease::{LeaseGuard, VISION_LEASE_HOLDER};
use crate::profile::EncoderControl;
use crate::tap::{serve_tap_session, TapSessionEnd};
use crate::wfb_tee::{drain_wfb_tee_stderr, ProgressTracker};

/// Fixed wait before the decoder is respawned after it exited, stalled or
/// failed to spawn.
pub const DECODER_RESPAWN_BACKOFF: Duration = crate::recovery::RETRY_INTERVAL;

/// Frame geometry the decoder emits.
#[derive(Debug, Clone, Copy)]
pub struct TapGeometry {
    pub format: FrameFormat,
    pub width: u32,
    pub height: u32,
}

/// Live facts about the leg, shared with the orchestrator.
#[derive(Debug, Default)]
pub struct TapLegState {
    decoding: AtomicBool,
    decoder_spawns: AtomicU32,
}

impl TapLegState {
    /// True while a decoder process is running.
    pub fn decoding(&self) -> bool {
        self.decoding.load(Ordering::Acquire)
    }

    /// Decoder processes spawned since the leg started.
    pub fn decoder_spawns(&self) -> u32 {
        self.decoder_spawns.load(Ordering::Acquire)
    }
}

/// Run the lazy leg until the listener fails. `spawn` starts one decoder with
/// stdout captured (normally [`crate::tap::spawn_vision_tap`]). `control` is
/// the encoder the leg holds the vision lease on.
pub async fn run_lazy_vision_tap<F>(
    listener: UnixListener,
    geometry: TapGeometry,
    idle: Duration,
    spawn: F,
    state: Arc<TapLegState>,
    control: Arc<EncoderControl>,
) where
    F: Fn() -> std::io::Result<ManagedProcess> + Send + 'static,
{
    // Dropped (and so released) on every exit, including an abort.
    let mut lease: Option<LeaseGuard> = None;
    loop {
        let accepted = if lease.is_some() {
            // A decoder ended under an attached engine: keep the lease while
            // the engine reconnects, but not past the idle window.
            match tokio::time::timeout(idle, listener.accept()).await {
                Ok(accepted) => accepted,
                Err(_) => {
                    lease = None;
                    tracing::info!("vision_tap_lease_released: engine did not return");
                    listener.accept().await
                }
            }
        } else {
            listener.accept().await
        };
        let engine = match accepted {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!(error = %e, "vision_tap_accept_failed");
                return;
            }
        };
        if lease.is_none() {
            lease = Some(hold_vision_lease(&control).await);
        }
        tracing::info!("vision_tap_engine_connected; starting decoder");
        let mut decoder = match spawn() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "vision_tap_start_failed");
                tokio::time::sleep(DECODER_RESPAWN_BACKOFF).await;
                continue;
            }
        };
        state.decoder_spawns.fetch_add(1, Ordering::AcqRel);
        if let Some(stderr) = decoder.take_stderr() {
            tokio::spawn(drain_wfb_tee_stderr(stderr, ProgressTracker::new()));
        }
        let Some(stdout) = decoder.take_stdout() else {
            tracing::error!("vision_tap_no_stdout");
            decoder.terminate(Duration::from_secs(2)).await;
            tokio::time::sleep(DECODER_RESPAWN_BACKOFF).await;
            continue;
        };
        state.decoding.store(true, Ordering::Release);
        let end = serve_tap_session(
            &listener,
            Some(engine),
            stdout,
            geometry.format,
            geometry.width,
            geometry.height,
            Some(idle),
        )
        .await;
        decoder.terminate(Duration::from_secs(5)).await;
        state.decoding.store(false, Ordering::Release);
        match end {
            TapSessionEnd::EngineIdle => {
                lease = None;
                tracing::info!("vision_tap_decoder_stopped: no engine attached");
            }
            TapSessionEnd::ListenerFailed => return,
            TapSessionEnd::SourceEnded | TapSessionEnd::Stalled => {
                tracing::warn!(reason = ?end, "vision_tap_decoder_ended; respawning on reconnect");
                tokio::time::sleep(DECODER_RESPAWN_BACKOFF).await;
            }
        }
    }
}

/// Take the vision lease and wait for the encoder to apply it.
async fn hold_vision_lease(control: &Arc<EncoderControl>) -> LeaseGuard {
    let (guard, generation) = control.hold_lease(VISION_LEASE_HOLDER);
    match control.wait_applied(generation, APPLY_TIMEOUT).await {
        Some(applied) => tracing::info!(
            profile = %applied.state.profile,
            fps = applied.state.fps,
            "vision_tap_lease_held"
        ),
        None => tracing::warn!("vision_tap_lease_unconfirmed; starting decoder anyway"),
    }
    guard
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CameraConfig;
    use crate::profile::lease::test_support::spawn_applier;
    use crate::profile::lease::LeaseView;
    use crate::profile::VideoProfile;
    use crate::tap::bind_vision_tap;
    use ados_protocol::tap::{decode_tap_header, TAP_HEADER_LEN};
    use tokio::io::AsyncReadExt;

    /// A fake decoder: prints one 2x2 rgb24 frame (12 bytes) every 20 ms.
    fn fake_decoder() -> std::io::Result<ManagedProcess> {
        ManagedProcess::spawn_capturing_stdout(
            "vision_tap_test",
            "sh",
            &[
                "-c".to_string(),
                "while true; do printf 'abcdefghijkl'; sleep 0.02; done".to_string(),
            ],
        )
    }

    /// A thumbnail encoder with a stand-in orchestrator acknowledging changes.
    fn thumbnail_control() -> Arc<EncoderControl> {
        let thumbnail = CameraConfig::default().thumbnail;
        let control = EncoderControl::new(VideoProfile::Thumbnail, thumbnail);
        // Detached: it keeps acknowledging until the test runtime shuts down.
        let _applier = spawn_applier(Arc::clone(&control));
        control
    }

    /// Bind the tap at `sink` and run the leg with a 300 ms idle window.
    fn start_leg(
        sink: &str,
        control: &Arc<EncoderControl>,
    ) -> (Arc<TapLegState>, tokio::task::JoinHandle<()>) {
        let listener = bind_vision_tap(sink).unwrap();
        let state = Arc::new(TapLegState::default());
        let geometry = TapGeometry {
            format: FrameFormat::Rgb24,
            width: 2,
            height: 2,
        };
        let leg = tokio::spawn(run_lazy_vision_tap(
            listener,
            geometry,
            Duration::from_millis(300),
            fake_decoder,
            state.clone(),
            Arc::clone(control),
        ));
        (state, leg)
    }

    /// Connect an engine and read its first frame header.
    async fn attach_engine(sink: &str) -> tokio::net::UnixStream {
        let mut engine = tokio::net::UnixStream::connect(sink).await.unwrap();
        let mut header = [0u8; TAP_HEADER_LEN];
        tokio::time::timeout(Duration::from_secs(5), engine.read_exact(&mut header))
            .await
            .unwrap()
            .unwrap();
        let (fmt, w, h, len) = decode_tap_header(&header).unwrap();
        assert_eq!((fmt, w, h, len), (FrameFormat::Rgb24, 2, 2, 12));
        engine
    }

    fn vision_lease() -> Vec<LeaseView> {
        vec![LeaseView {
            holder: VISION_LEASE_HOLDER.to_string(),
            expires_in_s: None,
        }]
    }

    #[tokio::test]
    async fn the_decoder_and_the_vision_lease_live_only_while_an_engine_is_attached() {
        let dir = tempfile::tempdir().unwrap();
        let sink = dir.path().join("vision-tap.sock");
        let sink_str = sink.to_string_lossy().to_string();
        let control = thumbnail_control();
        let (state, leg) = start_leg(&sink_str, &control);

        // Bound but no engine: nothing is decoding and nothing is leased.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(state.decoder_spawns(), 0);
        assert!(!state.decoding());
        assert!(control.leases().is_empty());
        assert_eq!(control.desired().profile, VideoProfile::Thumbnail);

        // An engine connects: the encoder goes hero before the decoder starts,
        // and frames arrive.
        let mut engine = attach_engine(&sink_str).await;
        let mut pixels = [0u8; 12];
        engine.read_exact(&mut pixels).await.unwrap();
        assert_eq!(&pixels, b"abcdefghijkl");
        assert_eq!(state.decoder_spawns(), 1);
        assert!(state.decoding());
        assert_eq!(control.leases(), vision_lease());
        assert_eq!(control.applied().state.profile, VideoProfile::Hero);
        assert_eq!(control.base_profile(), VideoProfile::Thumbnail);

        // The engine leaves: after the idle window the decoder is stopped and
        // the lease released, reverting the encoder to its base.
        drop(engine);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while (state.decoding() || !control.leases().is_empty())
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !state.decoding(),
            "decoder stopped once the engine was idle"
        );
        assert!(control.leases().is_empty(), "the vision lease was released");
        assert_eq!(control.desired().profile, VideoProfile::Thumbnail);
        assert!(
            !leg.is_finished(),
            "the leg stays bound for the next engine"
        );
        leg.abort();
    }

    #[tokio::test]
    async fn tearing_the_leg_down_releases_the_vision_lease() {
        let dir = tempfile::tempdir().unwrap();
        let sink = dir.path().join("vision-tap.sock");
        let sink_str = sink.to_string_lossy().to_string();
        let control = thumbnail_control();
        let (_state, leg) = start_leg(&sink_str, &control);

        let _engine = attach_engine(&sink_str).await;
        assert_eq!(control.leases(), vision_lease());
        assert_eq!(control.desired().profile, VideoProfile::Hero);

        // The orchestrator stops the tap by aborting the leg's task.
        leg.abort();
        let _ = leg.await;
        assert!(control.leases().is_empty());
        assert_eq!(control.desired().profile, VideoProfile::Thumbnail);
    }
}
