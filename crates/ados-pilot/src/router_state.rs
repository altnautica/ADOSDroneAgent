//! The router's vehicle-state socket (`state.v2`): the mission state the router
//! computes (`mission` of the snapshot) feeds the pilot's launch "when done",
//! `resume` and the `mission` of its published state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// The feed topic the mission state arrives under (see `Pilot::on_feed`).
pub const MISSION_TOPIC: &str = "mission";

/// The router's state socket.
pub fn socket_path() -> PathBuf {
    ados_protocol::paths::run_dir().join(ados_protocol::paths::STATE_SOCK)
}

/// The `mission` object of one snapshot, without the mirror's navigation
/// items, revision and upload counter: the mission state alone. `null` when the
/// snapshot carries none.
pub fn mission_of(mut snapshot: Value) -> Value {
    let mut mission = snapshot
        .get_mut(MISSION_TOPIC)
        .map(Value::take)
        .unwrap_or(Value::Null);
    if let Some(o) = mission.as_object_mut() {
        o.remove("items");
        o.remove("rev");
        o.remove("uploads_seen");
    }
    mission
}

/// Keep a subscription alive forever, forwarding each snapshot's mission
/// state as `(MISSION_TOPIC, mission)`. While the socket is down the mission
/// state is unknown, so `null` is forwarded once per lost session.
pub async fn run(path: PathBuf, tx: mpsc::Sender<(String, Value)>) {
    let mut backoff = Duration::from_millis(500);
    loop {
        match session(&path, &tx).await {
            Ok(true) => backoff = Duration::from_millis(500),
            Ok(false) => {}
            Err(e) => tracing::debug!(error = %e, "pilot_router_state_disconnected"),
        }
        if tx.send((MISSION_TOPIC.into(), Value::Null)).await.is_err() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// One connection. `Ok(true)` when at least one snapshot arrived.
async fn session(path: &Path, tx: &mpsc::Sender<(String, Value)>) -> std::io::Result<bool> {
    let stream = UnixStream::connect(path).await?;
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let mut got = false;
    while let Some(snapshot) =
        ados_protocol::state::read_state_value_into(&mut reader, &mut buf).await?
    {
        if !got {
            tracing::info!("pilot_router_state_connected");
            got = true;
        }
        if tx
            .send((MISSION_TOPIC.into(), mission_of(snapshot)))
            .await
            .is_err()
        {
            return Ok(got);
        }
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn mission_of_keeps_the_state_and_drops_the_mirror() {
        let snap = json!({
            "armed": true,
            "mission": {"state": "active", "seq": 2, "total": 4, "item_reached": 1, "id": null,
                        "source": "mission_state", "age_ms": 80, "landing_seq": false,
                        "rev": 3, "uploads_seen": 1, "items": [{"seq": 1, "cmd": 16}]},
        });
        let m = mission_of(snap);
        assert_eq!(m["state"], "active");
        assert!(m.get("items").is_none() && m.get("rev").is_none());
        assert!(m.get("uploads_seen").is_none());
        let typed: ados_protocol::pilot::Mission = serde_json::from_value(m).unwrap();
        assert_eq!(typed.seq, Some(2));
        assert_eq!(mission_of(json!({"armed": false})), Value::Null);
    }

    #[test]
    fn a_mission_before_mission_current_parses_with_null_numbers() {
        let m = mission_of(
            json!({"mission": {"state": "none", "seq": null, "total": null,
            "item_reached": null, "id": null, "source": "fallback", "age_ms": null,
            "landing_seq": false}}),
        );
        let typed: ados_protocol::pilot::Mission = serde_json::from_value(m).unwrap();
        assert_eq!(typed.state, "none");
        assert_eq!(typed.seq, None);
    }

    #[tokio::test]
    async fn snapshots_from_the_state_socket_are_forwarded_then_null_on_loss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let task = tokio::spawn(run(path, tx));
        let (mut conn, _) = listener.accept().await.unwrap();
        let snap = json!({"mission": {"state": "complete", "seq": 4, "total": 4,
            "item_reached": 3, "id": null, "source": "mission_state", "age_ms": 10,
            "landing_seq": false, "items": []}});
        let frame = ados_protocol::state::encode_v2(&snap).unwrap();
        conn.write_all(&frame).await.unwrap();
        let (topic, m) = rx.recv().await.unwrap();
        assert_eq!(topic, MISSION_TOPIC);
        assert_eq!(m["state"], "complete");
        drop(conn);
        let (_, lost) = rx.recv().await.unwrap();
        assert_eq!(lost, Value::Null);
        task.abort();
    }
}
