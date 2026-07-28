//! The encoder command socket — the ONE cross-process entry point for changing
//! what the encoder produces.
//!
//! `ados-video` owns the encoder, but the two controllers that steer it live in
//! other processes: `ados-control` serves `POST /api/video/profile` (the
//! operator's hero choice) and `ados-radio`'s adaptive-bitrate ladder applies a
//! clamp. Both dial this socket rather than each inventing a transport, so the
//! composition rule in [`super::resolve`] is enforced in exactly one place.
//!
//! Newline-delimited JSON, one request object and one reply object per
//! connection — the same shape as the supervisor's `/run/ados/video-cmd.sock`.
//!
//! ```text
//! {"op":"video.encoder.profile.set","profile":"hero"}
//! {"op":"video.encoder.ceiling.set","bitrate_kbps":1200}   // null / absent clears
//! {"op":"video.encoder.get"}
//!
//! -> {"ok":true,"profile":"hero","ceiling_kbps":1200,"width":1280,"height":720,
//!     "fps":30,"bitrate_kbps":1200,"restarted":true,"applied":true}
//! -> {"ok":false,"error":"E_ARGS","detail":"…"}
//! ```
//!
//! `applied` is observed, never assumed: a set waits for the orchestrator to
//! actually respawn the encoder before answering, and reports `false` if the
//! acknowledgement did not arrive within [`APPLY_TIMEOUT`].
//!
//! The read path is deliberately NOT the socket: [`read_state`] reads the
//! sidecar `ados-video` stamps on every apply, so a 1 Hz poller (the adaptive
//! ladder's self-heal check) and a 10 Hz poller (the state-snapshot builder that
//! feeds the swarm beacon's hero bit) cost a tmpfs read, not a connection.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::{EncoderControl, EncoderState, VideoProfile};

/// The command socket `ados-video` serves and every other process dials.
pub const VIDEO_ENCODER_SOCK: &str = "/run/ados/video-encoder.sock";

/// The attention-state sidecar `ados-video` stamps on every apply.
pub const VIDEO_PROFILE_SIDECAR: &str = "/run/ados/video-profile.json";

/// How long a set waits for the orchestrator's acknowledgement. Comfortably
/// over an encoder respawn (a terminate with a 5 s grace plus a spawn) so a
/// healthy switch always answers `applied: true`.
pub const APPLY_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a client waits on the whole socket round-trip. Strictly longer than
/// [`APPLY_TIMEOUT`] so the server's own honest "not confirmed" answer reaches
/// the caller instead of being pre-empted by the client's clock.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

/// Serve the encoder command socket until `shutdown` fires.
///
/// Best-effort: a bind failure is logged and the task returns, because a rig
/// with no writable run dir must still stream video — it just cannot be
/// retargeted at runtime.
pub async fn serve(control: Arc<EncoderControl>, path: &Path, shutdown: crate::shutdown::Shutdown) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // A stale socket from a crashed run would make bind fail with EADDRINUSE.
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "encoder_cmd_bind_failed");
            return;
        }
    };
    tracing::info!(path = %path.display(), "encoder_cmd_listening");
    loop {
        tokio::select! {
            _ = shutdown.wait() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let control = Arc::clone(&control);
                        tokio::spawn(async move { handle_conn(stream, control).await });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "encoder_cmd_accept_failed");
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_file(path);
}

async fn handle_conn(stream: UnixStream, control: Arc<EncoderControl>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() || line.trim().is_empty() {
        return;
    }
    let reply = dispatch(line.as_bytes(), &control).await;
    let mut body = serde_json::to_vec(&reply).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    body.push(b'\n');
    let _ = reader.get_mut().write_all(&body).await;
    let _ = reader.get_mut().flush().await;
}

/// Parse and execute one request. Pure over the control handle, so the whole
/// verb surface is testable with no socket.
pub async fn dispatch(request: &[u8], control: &EncoderControl) -> Value {
    let parsed: Value = match serde_json::from_slice(request) {
        Ok(v) => v,
        Err(e) => return err("E_ARGS", &format!("malformed request: {e}")),
    };
    let op = parsed.get("op").and_then(Value::as_str).unwrap_or("");
    match op {
        "video.encoder.get" => {
            let a = control.applied();
            ok_reply(&a.state, a.restarted, true)
        }
        "video.encoder.profile.set" => {
            let Some(profile) = parsed
                .get("profile")
                .and_then(Value::as_str)
                .and_then(VideoProfile::parse)
            else {
                return err("E_ARGS", "profile must be \"hero\" or \"thumbnail\"");
            };
            settle(control, control.request_profile(profile)).await
        }
        "video.encoder.ceiling.set" => {
            let ceiling = match parsed.get("bitrate_kbps") {
                None | Some(Value::Null) => None,
                Some(v) => match v.as_u64().filter(|n| *n > 0 && *n <= u64::from(u32::MAX)) {
                    Some(n) => Some(n as u32),
                    None => {
                        return err("E_ARGS", "bitrate_kbps must be a positive integer or null")
                    }
                },
            };
            settle(control, control.request_ceiling(ceiling)).await
        }
        other => json!({"ok": false, "error": "E_UNKNOWN_OP", "op": other}),
    }
}

/// Wait for the orchestrator to acknowledge `generation`, then answer with what
/// it actually applied. A timeout answers with the resolved target and
/// `applied: false` rather than claiming a switch that may not have happened.
async fn settle(control: &EncoderControl, generation: u64) -> Value {
    match control.wait_applied(generation, APPLY_TIMEOUT).await {
        Some(a) => ok_reply(&a.state, a.restarted, true),
        None => {
            let a = control.applied();
            ok_reply(&a.state, false, false)
        }
    }
}

fn ok_reply(state: &EncoderState, restarted: bool, applied: bool) -> Value {
    json!({
        "ok": true,
        "profile": state.profile.as_str(),
        "ceiling_kbps": state.ceiling_kbps,
        "width": state.width,
        "height": state.height,
        "fps": state.fps,
        "bitrate_kbps": state.bitrate_kbps,
        "restarted": restarted,
        "applied": applied,
    })
}

fn err(code: &str, detail: &str) -> Value {
    json!({"ok": false, "error": code, "detail": detail})
}

// ---------------------------------------------------------------------------
// client
// ---------------------------------------------------------------------------

/// Select the drone's base attention profile. The canonical socket path.
pub async fn set_profile(profile: VideoProfile) -> anyhow::Result<EncoderState> {
    set_profile_at(Path::new(VIDEO_ENCODER_SOCK), profile).await
}

/// [`set_profile`] against an explicit socket path (tests, alternate run dirs).
pub async fn set_profile_at(sock: &Path, profile: VideoProfile) -> anyhow::Result<EncoderState> {
    request(
        sock,
        &json!({"op": "video.encoder.profile.set", "profile": profile.as_str()}),
    )
    .await
}

/// Apply the adaptive ladder's bitrate clamp (`None` clears it). This only ever
/// reduces the bitrate below the active profile's nominal value.
pub async fn set_bitrate_ceiling(kbps: Option<u32>) -> anyhow::Result<EncoderState> {
    set_bitrate_ceiling_at(Path::new(VIDEO_ENCODER_SOCK), kbps).await
}

/// [`set_bitrate_ceiling`] against an explicit socket path.
pub async fn set_bitrate_ceiling_at(
    sock: &Path,
    kbps: Option<u32>,
) -> anyhow::Result<EncoderState> {
    request(
        sock,
        &json!({"op": "video.encoder.ceiling.set", "bitrate_kbps": kbps}),
    )
    .await
}

/// Read the live state over the socket. Prefer [`read_state`] for polling — this
/// costs a connection and exists for callers that need a same-process-as-truth
/// read rather than the sidecar's up-to-one-apply lag.
pub async fn state() -> anyhow::Result<EncoderState> {
    state_at(Path::new(VIDEO_ENCODER_SOCK)).await
}

/// [`state`] against an explicit socket path.
pub async fn state_at(sock: &Path) -> anyhow::Result<EncoderState> {
    request(sock, &json!({"op": "video.encoder.get"})).await
}

async fn request(sock: &Path, body: &Value) -> anyhow::Result<EncoderState> {
    let reply = tokio::time::timeout(CLIENT_TIMEOUT, roundtrip(sock, body))
        .await
        .map_err(|_| anyhow::anyhow!("encoder command socket timed out"))??;
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        let code = reply
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("E_UNKNOWN");
        let detail = reply.get("detail").and_then(Value::as_str).unwrap_or("");
        anyhow::bail!("encoder command rejected: {code} {detail}");
    }
    Ok(serde_json::from_value(reply)?)
}

async fn roundtrip(sock: &Path, body: &Value) -> anyhow::Result<Value> {
    let mut stream = UnixStream::connect(sock).await?;
    let mut line = serde_json::to_vec(body)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).await?;
    Ok(serde_json::from_str(&resp)?)
}

// ---------------------------------------------------------------------------
// sidecar read
// ---------------------------------------------------------------------------

/// The published attention state, or `None` when `ados-video` has not stamped
/// it (the service is down, or this node runs no pipeline).
///
/// Non-fatal by construction and never touches the socket: this is the poll
/// path for the adaptive ladder's self-heal check and for the state-snapshot
/// builder that feeds the swarm beacon's hero bit.
pub fn read_state() -> Option<EncoderState> {
    read_state_from(Path::new(VIDEO_PROFILE_SIDECAR))
}

/// [`read_state`] against an explicit sidecar path.
pub fn read_state_from(path: &Path) -> Option<EncoderState> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CameraConfig;
    use crate::profile::{base_settings, resolve};

    fn control() -> Arc<EncoderControl> {
        let cfg = CameraConfig::default();
        EncoderControl::new(VideoProfile::Thumbnail, cfg.thumbnail)
    }

    /// Stand in for the orchestrator: acknowledge every desired change by
    /// resolving it against the default camera config. Subscribes on the
    /// caller's thread so a request issued immediately after this returns can
    /// never be missed.
    fn spawn_applier(control: Arc<EncoderControl>) -> tokio::task::JoinHandle<()> {
        let mut rx = control.subscribe_desired();
        tokio::spawn(async move {
            let cfg = CameraConfig::default();
            while rx.changed().await.is_ok() {
                let d = *rx.borrow_and_update();
                let s = resolve(d.profile, d.ceiling_kbps, &cfg);
                control.note_applied(
                    EncoderState::new(d.profile, d.ceiling_kbps, s),
                    d.generation,
                    true,
                );
            }
        })
    }

    #[tokio::test]
    async fn a_profile_set_answers_with_the_settings_the_applier_confirmed() {
        let ctl = control();
        let _applier = spawn_applier(Arc::clone(&ctl));
        let reply = dispatch(
            br#"{"op":"video.encoder.profile.set","profile":"hero"}"#,
            &ctl,
        )
        .await;
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["applied"], true);
        assert_eq!(reply["profile"], "hero");
        assert_eq!(reply["width"], 1280);
        assert_eq!(reply["height"], 720);
        assert_eq!(reply["fps"], 30);
        assert_eq!(reply["bitrate_kbps"], 4000);
    }

    #[tokio::test]
    async fn a_ceiling_set_clamps_the_active_profile_and_null_clears_it() {
        let ctl = control();
        let _applier = spawn_applier(Arc::clone(&ctl));
        dispatch(
            br#"{"op":"video.encoder.profile.set","profile":"hero"}"#,
            &ctl,
        )
        .await;
        let clamped = dispatch(
            br#"{"op":"video.encoder.ceiling.set","bitrate_kbps":1200}"#,
            &ctl,
        )
        .await;
        assert_eq!(clamped["bitrate_kbps"], 1200);
        assert_eq!(clamped["ceiling_kbps"], 1200);
        // Geometry is the profile's, untouched by the clamp.
        assert_eq!(clamped["width"], 1280);
        let cleared = dispatch(br#"{"op":"video.encoder.ceiling.set"}"#, &ctl).await;
        assert_eq!(cleared["bitrate_kbps"], 4000);
        assert!(cleared["ceiling_kbps"].is_null());
    }

    #[tokio::test]
    async fn an_unconfirmed_set_reports_applied_false_instead_of_claiming_success() {
        // No applier running: the orchestrator never acknowledges.
        let ctl = control();
        let generation = ctl.request_profile(VideoProfile::Hero);
        // Drive `settle` directly with a short wait so the test stays fast; the
        // production bound is APPLY_TIMEOUT.
        assert!(ctl
            .wait_applied(generation, Duration::from_millis(20))
            .await
            .is_none());
        let a = ctl.applied();
        let reply = ok_reply(&a.state, false, false);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["applied"], false);
        // It reports what is actually live (still the boot thumbnail), not the
        // hero settings it was asked for.
        assert_eq!(reply["profile"], "thumbnail");
    }

    #[tokio::test]
    async fn bad_requests_are_rejected_without_touching_the_encoder() {
        let ctl = control();
        let before = ctl.desired();
        for body in [
            br#"{"op":"video.encoder.profile.set","profile":"Hero"}"#.as_slice(),
            br#"{"op":"video.encoder.profile.set"}"#.as_slice(),
            br#"{"op":"video.encoder.ceiling.set","bitrate_kbps":-5}"#.as_slice(),
            br#"{"op":"video.encoder.ceiling.set","bitrate_kbps":0}"#.as_slice(),
            br#"not json"#.as_slice(),
        ] {
            let reply = dispatch(body, &ctl).await;
            assert_eq!(reply["ok"], false, "body {}", String::from_utf8_lossy(body));
            assert_eq!(reply["error"], "E_ARGS");
        }
        let unknown = dispatch(br#"{"op":"video.encoder.nope"}"#, &ctl).await;
        assert_eq!(unknown["error"], "E_UNKNOWN_OP");
        assert_eq!(ctl.desired(), before);
    }

    #[tokio::test]
    async fn the_socket_round_trip_reaches_the_real_server() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("video-encoder.sock");
        let ctl = control();
        let _applier = spawn_applier(Arc::clone(&ctl));
        let shutdown = crate::shutdown::Shutdown::new();
        let server = {
            let ctl = Arc::clone(&ctl);
            let sock = sock.clone();
            let shutdown = shutdown.clone();
            tokio::spawn(async move { serve(ctl, &sock, shutdown).await })
        };
        // Wait for the bind.
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let st = set_profile_at(&sock, VideoProfile::Hero).await.unwrap();
        assert_eq!(st.profile, VideoProfile::Hero);
        assert_eq!(
            st.settings(),
            base_settings(VideoProfile::Hero, &CameraConfig::default())
        );

        let clamped = set_bitrate_ceiling_at(&sock, Some(900)).await.unwrap();
        assert_eq!(clamped.bitrate_kbps, 900);
        assert_eq!(clamped.ceiling_kbps, Some(900));

        let read_back = state_at(&sock).await.unwrap();
        assert_eq!(read_back, clamped);

        shutdown.trigger();
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    }

    #[test]
    fn an_absent_or_corrupt_sidecar_reads_as_none_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_state_from(&dir.path().join("missing.json")).is_none());
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"{not json").unwrap();
        assert!(read_state_from(&bad).is_none());
    }
}
