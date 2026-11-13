//! Source outage recovery: restart scope and time to ready.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::encoder::EncoderKind;
use crate::mediamtx::MediamtxManager;
use crate::process::ManagedProcess;
use crate::recovery::{INPUT_WATCH_INTERVAL, STAGE_STOP_GRACE};

/// A stand-in for the mediamtx control API: every `GET` answers 200 with the
/// `main` path's `ready` flag read from `ready`.
async fn fake_mediamtx_api(ready: Arc<AtomicBool>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
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
                let body = format!("{{\"ready\":{ready},\"bytesReceived\":0}}");
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

/// A free TCP port with nothing listening on it.
fn closed_port() -> u16 {
    let parked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    parked.local_addr().unwrap().port()
}

/// A process that ignores SIGTERM, as an ffmpeg blocked reading a silent
/// source does.
fn deaf_stage(label: &str) -> ManagedProcess {
    let args = vec!["-c".to_string(), "trap '' TERM; sleep 30".to_string()];
    ManagedProcess::spawn(label, "sh", &args).unwrap()
}

/// A streaming primary whose network source is `source` and whose mediamtx
/// API answers from `ready`; mediamtx itself is a stand-in process, so a
/// recovery that restarts mediamtx shows as a changed or missing PID.
async fn streaming_orch(
    dir: &std::path::Path,
    source: &str,
    ready: Arc<AtomicBool>,
) -> VideoOrchestrator {
    let camera = CameraConfig {
        source: source.to_string(),
        ..CameraConfig::default()
    };
    let mut o = VideoOrchestrator::new(
        AgentVideoConfig::default(),
        camera,
        dir,
        EncoderEnv::detect(),
    );
    let api = fake_mediamtx_api(ready).await;
    let mut mediamtx = MediamtxManager::new(dir).with_ports(api, closed_port(), 0, 0);
    let args = vec!["30".to_string()];
    mediamtx.adopt_process(ManagedProcess::spawn("mediamtx", "sleep", &args).unwrap());
    o.mediamtx = mediamtx;
    o.camera_state_path = Some(dir.join("camera-state.json"));
    o.video_streams_path = Some(dir.join("video-streams.json"));
    o.encoder_type = Some(EncoderKind::Ffmpeg);
    o.encoder_emits_wfb_rtp = true;
    o.encoder = Some(deaf_stage("encoder"));
    o.state = PipelineState::Running;
    o.first_packet_seen = true;
    o.inbound_flat = true;
    o
}

/// An 11 s outage of the primary's network camera: mediamtx drops the silent
/// publisher, the input watch sees it, and only the encoder is replaced. The
/// camera does not answer yet, so no encoder is spawned against it; the moment
/// it answers, the probe sees it and the encoder is respawned, with mediamtx
/// (and so the WHEP path) up the whole time.
#[tokio::test]
async fn a_source_outage_respawns_only_the_encoder_the_moment_the_source_answers() {
    let dir = tempfile::tempdir().unwrap();
    let source_port = closed_port();
    let ready = Arc::new(AtomicBool::new(false));
    let source = format!("rtsp://127.0.0.1:{source_port}/cam");
    let mut o = streaming_orch(dir.path(), &source, Arc::clone(&ready)).await;
    let mediamtx_pid = o.mediamtx.pid();
    assert!(o.input_watch_due(), "a flat counter on a streaming encoder");

    // The publisher is gone; one watch step handles it, inside a bounded time
    // even though the outgoing encoder ignores SIGTERM.
    let lost = tokio::time::Instant::now();
    tokio::time::timeout(STAGE_STOP_GRACE * 2, o.watch_input())
        .await
        .expect("the input-stage recovery is bounded");
    assert!(lost.elapsed() < STAGE_STOP_GRACE + INPUT_WATCH_INTERVAL * 2);
    assert!(o.mediamtx.is_running(), "mediamtx is kept");
    assert_eq!(o.mediamtx.pid(), mediamtx_pid, "mediamtx was not restarted");
    assert_eq!(o.state, PipelineState::Running);
    assert!(
        o.encoder.is_none(),
        "no encoder against an unreachable source"
    );
    assert!(o.source_wait.is_some(), "the source is being probed");

    // A tick while the source is still down leaves the held encoder alone.
    o.tick_running().await;
    assert!(o.encoder.is_none());
    assert_eq!(o.mediamtx.pid(), mediamtx_pid);

    // The camera comes back.
    let _camera = TcpListener::bind(("127.0.0.1", source_port)).await.unwrap();
    let back = tokio::time::Instant::now();
    tokio::time::timeout(
        ados_protocol::source_probe::PROBE_INTERVAL * 3,
        crate::orchestrator::input_stage::wait_for_source(o.source_wait.clone()),
    )
    .await
    .expect("the probe sees the source back within an interval");
    o.on_source_answered().await;
    let ready_after = back.elapsed();
    assert!(o.encoder.is_some(), "the encoder is respawned");
    assert!(o.source_wait.is_none());
    assert!(
        ready_after <= Duration::from_secs(1),
        "encoder respawned {ready_after:?} after the source answered"
    );
    assert_eq!(
        o.mediamtx.pid(),
        mediamtx_pid,
        "mediamtx was never restarted"
    );
    assert!(
        !o.first_packet_seen,
        "the fresh encoder gets the startup grace"
    );

    o.stop_stream().await;
}

/// A captured (frame-bus) or otherwise reachable source: the publisher leaving
/// `main` respawns the encoder at once and keeps mediamtx.
#[tokio::test]
async fn a_lost_publisher_with_a_reachable_source_respawns_the_encoder_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let camera = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let source = format!("rtsp://{}/cam", camera.local_addr().unwrap());
    let ready = Arc::new(AtomicBool::new(false));
    let mut o = streaming_orch(dir.path(), &source, ready).await;
    let mediamtx_pid = o.mediamtx.pid();
    let old_encoder = o.encoder.as_ref().and_then(ManagedProcess::pid);

    o.tick_running().await;

    let new_encoder = o.encoder.as_ref().and_then(ManagedProcess::pid);
    assert!(new_encoder.is_some(), "a fresh encoder is running");
    assert_ne!(new_encoder, old_encoder);
    assert_eq!(o.mediamtx.pid(), mediamtx_pid, "mediamtx was not restarted");
    assert_eq!(o.state, PipelineState::Running);
    o.stop_stream().await;
}

/// The teardown stops every stage at once under one short grace: three stages
/// that ignore SIGTERM cost one grace, not three of five seconds each.
#[tokio::test]
async fn a_teardown_of_deaf_stages_is_bounded_by_one_grace() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = test_orch();
    o.camera_state_path = Some(dir.path().join("camera-state.json"));
    o.encoder = Some(deaf_stage("encoder"));
    o.sei_tap = Some(deaf_stage("sei_tap"));
    o.secondary_encoders = vec![("cam2".to_string(), deaf_stage("encoder-cam2"))];
    o.encoder_emits_wfb_rtp = true;
    o.state = PipelineState::Running;

    let start = tokio::time::Instant::now();
    tokio::time::timeout(STAGE_STOP_GRACE * 3, o.stop_stream())
        .await
        .expect("the teardown is bounded by one grace");
    assert!(
        start.elapsed() < STAGE_STOP_GRACE * 2,
        "{:?}",
        start.elapsed()
    );
    assert!(o.encoder.is_none() && o.sei_tap.is_none());
    assert!(o.secondary_encoders.is_empty());
    assert_eq!(o.state, PipelineState::Stopped);
}
