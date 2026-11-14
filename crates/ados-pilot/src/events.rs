//! The plugin event export client: the World Engine scan, live occupancy and
//! pose topics feed the obstacle model; its lidar status feeds sensor health.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Topics the pilot reads.
pub const TOPICS: &[&str] = &[
    "plugin.world-engine.scan",
    "plugin.world-engine.lidar",
    "plugin.world-engine.map_status",
    "plugin.world-engine.occupancy",
    "plugin.world-engine.pose",
    // vision-nav's active EKF source set: the GNSS-denied speed cap.
    "nav.state",
];

/// The export socket (`ADOS_PLUGIN_HOST_DIR`-aware, like the control front).
pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("ADOS_PLUGIN_HOST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ados_protocol::paths::resolve("/run/ados/plugin-host"));
    dir.join(ados_protocol::paths::PLUGIN_EVENTS_SOCK)
}

/// Keep a subscription alive forever, forwarding `(topic, payload)`.
pub async fn run(path: PathBuf, tx: mpsc::Sender<(String, serde_json::Value)>) {
    let mut backoff = Duration::from_millis(500);
    loop {
        match session(&path, &tx).await {
            Ok(()) => backoff = Duration::from_millis(500),
            Err(e) => tracing::debug!(error = %e, "pilot_events_disconnected"),
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

async fn session(
    path: &Path,
    tx: &mpsc::Sender<(String, serde_json::Value)>,
) -> std::io::Result<()> {
    let stream = UnixStream::connect(path).await?;
    let (rd, mut wr) = stream.into_split();
    let mut req =
        serde_json::to_vec(&serde_json::json!({"v": 1, "op": "subscribe", "topics": TOPICS}))?;
    req.push(b'\n');
    wr.write_all(&req).await?;
    tracing::info!("pilot_events_subscribed");
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let (Some(topic), Some(payload)) =
            (v.get("topic").and_then(|t| t.as_str()), v.get("payload"))
        else {
            continue;
        };
        if tx.send((topic.to_owned(), payload.clone())).await.is_err() {
            return Ok(());
        }
    }
    Ok(())
}
