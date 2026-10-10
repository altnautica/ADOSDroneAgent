//! The encoder command socket — the ONE cross-process entry point for changing
//! what the encoder produces.
//!
//! `ados-video` owns the encoder, but the controllers that steer it live in
//! other processes: `ados-control` serves `/api/video/profile` (the fleet's
//! hero assignment and an operator's hero lease) and `ados-radio`'s
//! adaptive-bitrate ladder applies a clamp. All of them dial this socket rather
//! than each inventing a transport, so the composition rules in
//! [`super::resolve`] and [`super::lease`] are enforced in exactly one place.
//!
//! Newline-delimited JSON, one request object and one reply object per
//! connection — the same shape as the supervisor's `/run/ados/video-cmd.sock`.
//!
//! ```text
//! {"op":"video.encoder.profile.set","profile":"hero"}       // moves the base
//! {"op":"video.encoder.lease.set","holder":"gcs-1","lease_s":30}
//! {"op":"video.encoder.lease.release","holder":"gcs-1"}     // unknown holder ok
//! {"op":"video.encoder.ceiling.set","bitrate_kbps":1200}   // null / absent clears
//! {"op":"video.encoder.get"}
//!
//! -> {"ok":true,"profile":"hero","base_profile":"thumbnail","ceiling_kbps":1200,
//!     "width":1280,"height":720,"fps":30,"bitrate_kbps":1200,
//!     "ceiling_deferred":false,"encoder_respawns":3,
//!     "leases":[{"holder":"gcs-1","expires_in_s":29.8},
//!               {"holder":"vision","expires_in_s":null}],
//!     "restarted":true,"applied":true}
//! -> {"ok":false,"error":"E_ARGS","detail":"…"}
//! ```
//!
//! `profile` is the effective profile: hero while any lease is live, else
//! `base_profile`. A lease's `holder` is 1-64 characters of `[A-Za-z0-9._:-]`
//! (`vision` is reserved for the internal vision-tap lease, which has no
//! expiry) and `lease_s` is an integer in `5..=120`. `leases` is read live at
//! reply time.
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

use super::lease::{check_lease_holder, lease_seconds_in_range};
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
pub async fn serve(
    control: Arc<EncoderControl>,
    path: &Path,
    shutdown: ados_protocol::shutdown::Shutdown,
) {
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
            ok_reply(&a.state, control, a.restarted, true)
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
        "video.encoder.lease.set" => {
            let holder = match holder_arg(&parsed) {
                Ok(holder) => holder,
                Err(reply) => return reply,
            };
            let Some(lease_s) = parsed
                .get("lease_s")
                .and_then(Value::as_u64)
                .filter(|n| lease_seconds_in_range(*n))
            else {
                return err("E_ARGS", "lease_s must be an integer from 5 to 120");
            };
            let generation = control.set_lease(holder, Duration::from_secs(lease_s));
            settle(control, generation).await
        }
        "video.encoder.lease.release" => {
            let holder = match holder_arg(&parsed) {
                Ok(holder) => holder,
                Err(reply) => return reply,
            };
            settle(control, control.release_lease(holder)).await
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

/// The validated `holder` of a lease verb, or the `E_ARGS` reply.
fn holder_arg(parsed: &Value) -> Result<&str, Value> {
    let holder = parsed.get("holder").and_then(Value::as_str).unwrap_or("");
    check_lease_holder(holder).map_err(|detail| err("E_ARGS", detail))?;
    Ok(holder)
}

/// Wait for the orchestrator to acknowledge `generation`, then answer with what
/// it actually applied. A timeout answers with the resolved target and
/// `applied: false` rather than claiming a switch that may not have happened.
async fn settle(control: &EncoderControl, generation: u64) -> Value {
    match control.wait_applied(generation, APPLY_TIMEOUT).await {
        Some(a) => ok_reply(&a.state, control, a.restarted, true),
        None => {
            let a = control.applied();
            ok_reply(&a.state, control, false, false)
        }
    }
}

fn ok_reply(
    state: &EncoderState,
    control: &EncoderControl,
    restarted: bool,
    applied: bool,
) -> Value {
    json!({
        "ok": true,
        "profile": state.profile.as_str(),
        // The base and the leases are read live, so `expires_in_s` is current
        // and a release shows even when it left the encoder untouched.
        "base_profile": control.base_profile().as_str(),
        "leases": control.leases(),
        "ceiling_kbps": state.ceiling_kbps,
        "width": state.width,
        "height": state.height,
        "fps": state.fps,
        "bitrate_kbps": state.bitrate_kbps,
        // The ceiling was accepted but is NOT on the encoder: `bitrate_kbps`
        // above is what the encoder is really running. A caller that reported
        // the ceiling as the live rate would be publishing a number the encoder
        // never received.
        "ceiling_deferred": state.ceiling_deferred,
        "encoder_respawns": state.encoder_respawns,
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

/// Create or renew `holder`'s hero lease for `lease_secs` seconds. The
/// canonical socket path.
pub async fn set_lease(holder: &str, lease_secs: u64) -> anyhow::Result<EncoderState> {
    set_lease_at(Path::new(VIDEO_ENCODER_SOCK), holder, lease_secs).await
}

/// [`set_lease`] against an explicit socket path.
pub async fn set_lease_at(
    sock: &Path,
    holder: &str,
    lease_secs: u64,
) -> anyhow::Result<EncoderState> {
    request(
        sock,
        &json!({"op": "video.encoder.lease.set", "holder": holder, "lease_s": lease_secs}),
    )
    .await
}

/// Release `holder`'s hero lease (a no-op for an unknown holder). The
/// canonical socket path.
pub async fn release_lease(holder: &str) -> anyhow::Result<EncoderState> {
    release_lease_at(Path::new(VIDEO_ENCODER_SOCK), holder).await
}

/// [`release_lease`] against an explicit socket path.
pub async fn release_lease_at(sock: &Path, holder: &str) -> anyhow::Result<EncoderState> {
    request(
        sock,
        &json!({"op": "video.encoder.lease.release", "holder": holder}),
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
#[path = "ipc_tests.rs"]
mod tests;
