//! The video pipeline's one retry cadence.
//!
//! Every recovery path in this crate retries on the run loop's tick: the
//! primary encoder respawn after its source drops, the cold start from the
//! error state, and the consumer ladders (cloud push, wfb tap, vision tap,
//! SEI tap, secondary-leg encoders). The tick period is [`RETRY_INTERVAL`] and
//! it is fixed: no backoff, no multiplier, no attempt ceiling, and no recovery
//! path adds a sleep of its own on top of it. A source that comes back is
//! therefore republished on `main` within one interval plus the encoder's own
//! spawn and connect time, and every consumer of `main` is respawned on the
//! first tick that sees the publisher, inside that same tick.
//!
//! Stall detection is a separate concern with its own wall-clock thresholds
//! (`health::INBOUND_FLOW_STALL`, `health::STARTUP_GRACE_MAX`,
//! `wfb_tee::WFB_TEE_PROGRESS_TIMEOUT`); sampling them more often does not
//! make them trip sooner.

use std::time::Duration;

/// The fixed retry interval shared by every recovery path, and the period of
/// the run loop's health tick that drives them.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(2);

/// The run loop's tick: the first tick completes at once, then one every
/// [`RETRY_INTERVAL`]. A tick body that overruns the interval (a cold start
/// waiting on mediamtx) skips the ticks it missed instead of firing them in a
/// burst; the late tick fires as soon as it is polled and the one after it is
/// at most one interval later, so an overrun never pushes the schedule out.
pub fn retry_ticker() -> tokio::time::Interval {
    let mut tick = tokio::time::interval(RETRY_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick
}

/// What a consumer ladder does with an unhealthy tap on this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapRecovery {
    /// `main` has no publisher: tear the tap down and look again next tick.
    Defer,
    /// `main` is published: respawn the tap now, inside this tick.
    Respawn,
}

/// The decision for an unhealthy tap. It depends only on whether the tap's
/// source is published right now, never on how many attempts came before, so a
/// tap that has failed a hundred times is respawned on the same tick as one
/// that failed once.
pub fn tap_recovery(source_ready: bool) -> TapRecovery {
    if source_ready {
        TapRecovery::Respawn
    } else {
        TapRecovery::Defer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::Instant;

    use crate::config::{AgentVideoConfig, CameraConfig, VisionTapConfig, WfbVideoConfig};
    use crate::encoder::EncoderEnv;
    use crate::health::PipelineState;
    use crate::mediamtx::MediamtxManager;
    use crate::orchestrator::VideoOrchestrator;

    #[test]
    fn the_retry_interval_sits_inside_the_fixed_window() {
        assert!(RETRY_INTERVAL >= Duration::from_secs(2));
        assert!(RETRY_INTERVAL <= Duration::from_secs(5));
    }

    /// Fifty consecutive failed attempts, one per tick: every gap is exactly
    /// the retry interval. Nothing grows with the failure count.
    #[tokio::test(start_paused = true)]
    async fn the_retry_schedule_is_fixed_across_consecutive_failures() {
        let mut tick = retry_ticker();
        tick.tick().await;
        let mut previous = Instant::now();
        for attempt in 1..=50u32 {
            tick.tick().await;
            let now = Instant::now();
            assert_eq!(
                now - previous,
                RETRY_INTERVAL,
                "attempt {attempt} waited a different interval"
            );
            previous = now;
        }
    }

    /// An attempt that overran the interval is followed at once by the next
    /// one, and the attempt after that is no more than one interval later.
    #[tokio::test(start_paused = true)]
    async fn an_overrunning_attempt_does_not_push_the_schedule_out() {
        let mut tick = retry_ticker();
        tick.tick().await;
        // The attempt took more than two intervals.
        tokio::time::advance(RETRY_INTERVAL * 2 + Duration::from_millis(500)).await;
        let late = Instant::now();
        tick.tick().await;
        assert_eq!(
            Instant::now(),
            late,
            "the missed tick fires as soon as it is polled"
        );
        tick.tick().await;
        let gap = Instant::now() - late;
        assert!(gap > Duration::ZERO && gap <= RETRY_INTERVAL, "gap {gap:?}");
        // And from there on the cadence is the fixed interval again.
        let resumed = Instant::now();
        tick.tick().await;
        assert_eq!(Instant::now() - resumed, RETRY_INTERVAL);
    }

    #[test]
    fn a_tap_respawns_on_the_first_tick_its_source_is_back() {
        // Many ticks with the source down: every one defers, none respawns.
        for _ in 0..100 {
            assert_eq!(tap_recovery(false), TapRecovery::Defer);
        }
        // The first tick that sees `main` published respawns, whatever the
        // history: there is no attempt count to wait out.
        assert_eq!(tap_recovery(true), TapRecovery::Respawn);
    }

    /// A stand-in for the mediamtx control API: every `GET` answers 200 with
    /// the path's `ready` flag read from `ready`.
    async fn fake_mediamtx_api(ready: Arc<AtomicBool>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let ready = ready.load(Ordering::Acquire);
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 512];
                    while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => req.extend_from_slice(&buf[..n]),
                        }
                    }
                    let body = format!("{{\"ready\":{ready},\"bytesReceived\":4096}}");
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        port
    }

    /// The consumer ladders run independently inside one tick: on the tick
    /// `main` is published again, the vision tap AND the SEI tap both come
    /// back, with no wait of their own and without one tap's respawn holding
    /// the other over to a later tick.
    #[tokio::test]
    async fn every_tap_respawns_inside_the_tick_the_source_returns() {
        let dir = tempfile::tempdir().unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let api_port = fake_mediamtx_api(Arc::clone(&ready)).await;

        let cfg = AgentVideoConfig {
            vision: VisionTapConfig {
                enabled: true,
                sink: dir
                    .path()
                    .join("vision-tap.sock")
                    .to_string_lossy()
                    .into_owned(),
                ..VisionTapConfig::default()
            },
            wfb: WfbVideoConfig { sei_latency: true },
            ..AgentVideoConfig::default()
        };
        let mut o = VideoOrchestrator::new(
            cfg,
            CameraConfig::default(),
            dir.path(),
            EncoderEnv::detect(),
        );
        let mediamtx = MediamtxManager::new(dir.path());
        o.mediamtx = mediamtx.with_ports(api_port, 18554, 18889, 18888);
        o.camera_state_path = Some(dir.path().join("camera-state.json"));
        o.video_streams_path = Some(dir.path().join("video-streams.json"));
        // A one-shot SEI session that exits at once stands in for the Python tap.
        o.python_executable = "true".to_string();
        // The radio RTP rides the encoder, so there is no separate wfb tap here.
        o.encoder_emits_wfb_rtp = true;
        o.state = PipelineState::Running;

        // Source down: every tap is deferred, nothing is spawned.
        o.tick_consumers().await;
        assert!(o.vision_tap.is_none());
        assert!(o.sei_tap.is_none());

        // Source back: one tick brings every tap up, inside the tick.
        ready.store(true, Ordering::Release);
        tokio::time::timeout(RETRY_INTERVAL, o.tick_consumers())
            .await
            .expect("a tap respawn must not wait on a timer of its own");
        assert!(o.vision_tap.is_some(), "vision tap respawned on this tick");
        assert!(o.sei_tap.is_some(), "SEI tap respawned on the same tick");

        o.stop_vision_tap().await;
        if let Some(mut tap) = o.sei_tap.take() {
            tap.terminate(Duration::from_secs(1)).await;
        }
    }
}
