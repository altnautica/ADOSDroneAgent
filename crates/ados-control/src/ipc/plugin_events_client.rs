//! The plugin event-bus export reader (seam K, contract `plugin.events`).
//!
//! The plugin host owns the event bus in its own process and exports it on the
//! root-only `<plugin host dir>/_events.sock`. This client subscribes to the
//! navigation, mapping and follow topics ([`crate::routes::nav::EXPORT_TOPICS`])
//! and feeds every event into the [`NavHub`] the nav routes serve.
//!
//! Wire (newline JSON): one request line
//! `{"v":1,"op":"subscribe","topics":[...]}`, the host's `{"v":1,"ok":true}`
//! (or `{"v":1,"error":...}`), then one `{"v","topic","publisher","ts_ms",
//! "payload"}` line per event. Same posture as the swarm reader: an absent
//! socket is normal (no plugin host on this profile, or it has not started), so
//! the reader logs at debug, retries on the shared local-socket pace, and the
//! routes report the kinds as `null`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ados_protocol::ipc::read_newline_line;
use ados_protocol::retry::RetryPace;
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::oneshot;

use crate::routes::nav::pose_age::enrich_nav_state;
use crate::routes::nav::{route, NavHub, NavKind, Routed, EXPORT_TOPICS};

/// The `plugin.events` wire version this reader speaks.
const EXPORT_VERSION: u64 = 1;

/// Cap on one event line. The host caps an event payload at 64 KiB of
/// msgpack; its JSON form (float arrays, base64 tiles) stays well inside this.
const MAX_LINE: usize = 512 * 1024;

const RECONNECT_PACE: RetryPace = ados_protocol::retry::LOCAL_SOCKET;

/// The export socket path (`ADOS_PLUGIN_HOST_DIR`-aware).
pub fn default_events_socket() -> PathBuf {
    super::plugin_control_client::plugin_host_dir().join(ados_protocol::paths::PLUGIN_EVENTS_SOCK)
}

/// Stops the reader task on shutdown and joins it.
pub struct PluginEventsHandle {
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl PluginEventsHandle {
    /// Signal the reader to stop and wait for it.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Err(e) = self.join.await {
            if e.is_panic() {
                std::panic::resume_unwind(e.into_panic());
            }
        }
    }
}

/// Spawn the reader against `socket_path`, feeding `hub`. `sidecar_dir` is the
/// plugin state-sidecar directory the `nav` kind's pose age is read from.
pub fn spawn(socket_path: PathBuf, sidecar_dir: PathBuf, hub: NavHub) -> PluginEventsHandle {
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(read_loop(socket_path, sidecar_dir, hub, stop_rx));
    PluginEventsHandle {
        stop: Some(stop_tx),
        join,
    }
}

/// The subscribe request line, newline included.
pub(crate) fn request_line() -> Vec<u8> {
    let mut line = serde_json::to_vec(&json!({
        "v": EXPORT_VERSION,
        "op": "subscribe",
        "topics": EXPORT_TOPICS,
    }))
    .unwrap_or_default();
    line.push(b'\n');
    line
}

/// Apply one exported event line to the hub. A line that is not a well-formed
/// event of this wire version is skipped. vision-nav's `nav.state` gains the
/// pose age from its state sidecar in `sidecar_dir`.
pub(crate) fn apply_line(hub: &NavHub, sidecar_dir: &Path, line: &[u8]) {
    let Ok(event) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if event["v"].as_u64() != Some(EXPORT_VERSION) {
        return;
    }
    let (Some(topic), Some(publisher)) = (event["topic"].as_str(), event["publisher"].as_str())
    else {
        return;
    };
    let ts_ms = event["ts_ms"].as_i64().unwrap_or(0);
    let mut payload = event["payload"].clone();
    if route(topic, publisher) == Routed::Latest(NavKind::Nav) {
        payload = enrich_nav_state(payload, sidecar_dir, SystemTime::now());
    }
    hub.ingest(topic, publisher, ts_ms, payload);
}

async fn read_loop(
    socket_path: PathBuf,
    sidecar_dir: PathBuf,
    hub: NavHub,
    stop: oneshot::Receiver<()>,
) {
    tokio::pin!(stop);
    loop {
        tokio::select! {
            biased;
            _ = &mut stop => return,
            connected = UnixStream::connect(&socket_path) => {
                match connected {
                    Ok(stream) => {
                        tracing::debug!(path = %socket_path.display(), "plugin event export connected");
                        if run_stream(stream, &hub, &sidecar_dir, &mut stop).await {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            path = %socket_path.display(),
                            error = %e,
                            "plugin event export absent; will retry"
                        );
                    }
                }
                tokio::select! {
                    _ = &mut stop => return,
                    _ = tokio::time::sleep(RECONNECT_PACE.wait()) => {}
                }
            }
        }
    }
}

/// Subscribe on one connection and apply events until it ends. True when the
/// stop signal fired (consumed; the caller must not poll it again).
async fn run_stream(
    stream: UnixStream,
    hub: &NavHub,
    sidecar_dir: &Path,
    stop: &mut std::pin::Pin<&mut oneshot::Receiver<()>>,
) -> bool {
    let (read_half, mut write_half) = stream.into_split();
    if write_half.write_all(&request_line()).await.is_err() {
        return false;
    }
    let mut reader = BufReader::new(read_half);
    let ack = tokio::select! {
        biased;
        _ = &mut **stop => return true,
        r = read_newline_line(&mut reader, MAX_LINE) => r,
    };
    match ack
        .ok()
        .flatten()
        .and_then(|l| serde_json::from_slice::<Value>(&l).ok())
    {
        Some(v) if v["ok"] == true => {}
        Some(v) => {
            tracing::warn!(error = %v["error"], "plugin event export refused the subscription");
            return false;
        }
        None => return false,
    }
    loop {
        let line = tokio::select! {
            biased;
            _ = &mut **stop => return true,
            r = read_newline_line(&mut reader, MAX_LINE) => r,
        };
        match line {
            Ok(Some(bytes)) => apply_line(hub, sidecar_dir, &bytes),
            Ok(None) | Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::nav::{FOLLOW_ME_ID, WORLD_ENGINE_ID};
    use tokio::io::AsyncBufReadExt;

    #[test]
    fn the_request_names_every_export_topic() {
        let line = request_line();
        assert_eq!(line.last(), Some(&b'\n'));
        let v: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v["op"], "subscribe");
        assert_eq!(v["topics"].as_array().unwrap().len(), EXPORT_TOPICS.len());
    }

    #[test]
    fn a_line_of_another_version_or_shape_is_skipped() {
        let hub = NavHub::new();
        let dir = tempfile::tempdir().unwrap();
        apply_line(&hub, dir.path(), b"not json");
        apply_line(
            &hub,
            dir.path(),
            br#"{"v":2,"topic":"follow.state","publisher":"com.altnautica.follow-me","payload":{}}"#,
        );
        apply_line(&hub, dir.path(), br#"{"v":1,"payload":{}}"#);
        assert_eq!(hub.status_body()["follow"], Value::Null);
    }

    /// The `nav` kind carries vision-nav's pose age from its state sidecar,
    /// `null` without one; the other kinds stay verbatim.
    #[test]
    fn nav_state_gains_the_pose_age() {
        let hub = NavHub::new();
        let dir = tempfile::tempdir().unwrap();
        let line = br#"{"v":1,"topic":"nav.state","publisher":"com.altnautica.vision-nav","ts_ms":1,"payload":{"mode":"external_nav"}}"#;
        apply_line(&hub, dir.path(), line);
        let nav = &hub.status_body()["nav"]["data"];
        assert_eq!(nav["mode"], "external_nav");
        assert!(nav.as_object().unwrap().contains_key("pose_age_ms"));
        assert_eq!(nav["pose_age_ms"], Value::Null);

        let written_ms = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let sidecar = json!({"telemetry.navigation": {"ts_ms": written_ms,
            "payload": {"poseAgeMs": 118.0}}});
        std::fs::write(
            dir.path().join("com.altnautica.vision-nav-state.json"),
            sidecar.to_string(),
        )
        .unwrap();
        apply_line(&hub, dir.path(), line);
        // The reported age plus the few ms since the sidecar was written.
        let served = hub.status_body()["nav"]["data"]["pose_age_ms"]
            .as_f64()
            .unwrap();
        assert!((118.0..1_118.0).contains(&served), "{served}");

        apply_line(
            &hub,
            dir.path(),
            br#"{"v":1,"topic":"follow.state","publisher":"com.altnautica.follow-me","ts_ms":1,"payload":{"phase":"idle"}}"#,
        );
        assert_eq!(
            hub.status_body()["follow"]["data"],
            json!({"phase": "idle"})
        );
    }

    #[tokio::test]
    async fn subscribes_and_feeds_the_hub_from_a_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("_events.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut lines = BufReader::new(read_half).lines();
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(req["op"], "subscribe");
            write_half
                .write_all(b"{\"v\":1,\"ok\":true}\n")
                .await
                .unwrap();
            let event = json!({"v": 1, "topic": "plugin.world-engine.pose", "publisher": WORLD_ENGINE_ID,
                "ts_ms": 5, "payload": {"health": "good"}});
            write_half
                .write_all(format!("{event}\n").as_bytes())
                .await
                .unwrap();
            let follow = json!({"v": 1, "topic": "follow.state", "publisher": FOLLOW_ME_ID,
                "ts_ms": 6, "payload": {"phase": "following"}});
            write_half
                .write_all(format!("{follow}\n").as_bytes())
                .await
                .unwrap();
            // Hold the connection open until the reader is stopped.
            let _ = lines.next_line().await;
        });

        let hub = NavHub::new();
        let handle = spawn(path, dir.path().to_path_buf(), hub.clone());
        for _ in 0..200 {
            if !hub.status_body()["follow"].is_null() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let status = hub.status_body();
        assert_eq!(status["pose"]["data"]["health"], "good");
        assert_eq!(status["follow"]["data"]["phase"], "following");
        handle.shutdown().await;
        server.abort();
    }
}
