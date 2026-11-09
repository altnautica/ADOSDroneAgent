//! The pilot's `state` and `obstacles` into the nav hub, and the hub's view
//! demand back to the pilot. The pilot reads the router's mission state itself,
//! so its `state` already carries `mission`.
//!
//! The socket is read by its own task feeding a channel, so a frame half read
//! when the demand changes is never dropped (a length-prefixed read is not
//! cancel-safe inside a `select!`). The view demand goes to the pilot when it
//! changes, when a camera model changes (a gimbal move re-aims the obstacle
//! grid at once), and every [`DEMAND_REFRESH`].

use std::path::PathBuf;
use std::time::Duration;

use ados_protocol::pilot::{self, CameraModel, ViewDemand};
use serde_json::Value;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::routes::nav::{NavHub, NavKind};

/// Re-send the view demand this often even when it did not change, so a
/// restarted pilot learns it within one period.
const DEMAND_REFRESH: Duration = Duration::from_secs(2);

/// The fixed pause before the pilot socket is redialled.
const RETRY: Duration = Duration::from_secs(2);

/// Frames buffered between the socket reader and the session.
const READ_QUEUE: usize = 16;

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The `view_demand` request: the hub's cameras and layers plus each camera's
/// projection model.
pub fn demand_body(hub_demand: &crate::routes::nav::ViewDemand) -> ViewDemand {
    let models = crate::ar::camera_models();
    let mut out = ViewDemand::default();
    for (cam, layers) in &hub_demand.cameras {
        out.cameras
            .insert(cam.clone(), layers.iter().cloned().collect());
        if let Some(m) = models.get(cam) {
            out.models.insert(
                cam.clone(),
                CameraModel {
                    frame_w: m.frame_w,
                    frame_h: m.frame_h,
                    hfov_deg: m.hfov_deg as f32,
                    vfov_deg: m.vfov_deg as f32,
                    tilt_down_deg: m.tilt_down_deg as f32,
                    yaw_deg: m.yaw_deg as f32,
                    stabilised: m.stabilised,
                },
            );
        }
    }
    out
}

/// Run forever: subscribe to the pilot, feed the hub, push view demand.
pub async fn run(path: PathBuf, hub: NavHub) {
    loop {
        if let Err(e) = session(&path, &hub).await {
            tracing::debug!(error = %e, "pilot_ingest_disconnected");
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Aborts the socket reader when the session ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Read whole frames off the socket into `tx` until it closes or fails.
async fn read_frames(
    mut rd: tokio::net::unix::OwnedReadHalf,
    tx: mpsc::Sender<std::io::Result<Vec<u8>>>,
) {
    loop {
        let msg = match pilot::read_msg(&mut rd).await {
            Ok(Some(body)) => Ok(body),
            Ok(None) => return,
            Err(e) => Err(std::io::Error::other(e)),
        };
        let failed = msg.is_err();
        if tx.send(msg).await.is_err() || failed {
            return;
        }
    }
}

fn ingest(hub: &NavHub, body: &[u8]) {
    let Ok((op, v)) = pilot::peek_op(body) else {
        return;
    };
    let Ok(mut json) = serde_json::to_value(&v) else {
        return;
    };
    if let Some(m) = json.as_object_mut() {
        m.remove("op");
    }
    match op.as_str() {
        "state" => hub.ingest_host(NavKind::Pilot, epoch_ms(), json),
        "obstacles" => {
            // The pilot stamps its own monotonic clock; surfaces compare
            // `ts_ms` with wall time.
            if let Some(m) = json.as_object_mut() {
                m.insert("ts_ms".into(), Value::from(epoch_ms()));
            }
            hub.ingest_host(NavKind::Obstacles, epoch_ms(), json);
        }
        _ => {}
    }
}

async fn session(path: &PathBuf, hub: &NavHub) -> std::io::Result<()> {
    let stream = UnixStream::connect(path).await?;
    let (rd, mut wr) = stream.into_split();
    let sub = pilot::encode_op("subscribe", &()).map_err(std::io::Error::other)?;
    pilot::write_msg(&mut wr, &sub)
        .await
        .map_err(std::io::Error::other)?;
    tracing::info!("pilot_ingest_connected");
    let (tx, mut frames) = mpsc::channel(READ_QUEUE);
    let _reader = AbortOnDrop(tokio::spawn(read_frames(rd, tx)));
    let mut demand = hub.view_demand();
    let mut models = crate::ar::model_changes();
    let mut refresh = tokio::time::interval(DEMAND_REFRESH);
    loop {
        tokio::select! {
            msg = frames.recv() => {
                let Some(msg) = msg else { return Ok(()) };
                ingest(hub, &msg?);
            }
            changed = demand.changed() => {
                if changed.is_err() { return Ok(()); }
                let d = demand.borrow_and_update().clone();
                send_demand(&mut wr, &d).await?;
            }
            changed = models.changed() => {
                if changed.is_err() { return Ok(()); }
                models.borrow_and_update();
                let d = demand.borrow().clone();
                send_demand(&mut wr, &d).await?;
            }
            _ = refresh.tick() => {
                let d = demand.borrow().clone();
                send_demand(&mut wr, &d).await?;
            }
        }
    }
}

async fn send_demand(
    wr: &mut tokio::net::unix::OwnedWriteHalf,
    d: &crate::routes::nav::ViewDemand,
) -> std::io::Result<()> {
    let body = pilot::encode_op("view_demand", &demand_body(d)).map_err(std::io::Error::other)?;
    pilot::write_msg(wr, &body)
        .await
        .map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_demand_carries_cameras_and_layers() {
        let mut d = crate::routes::nav::ViewDemand::default();
        d.cameras.insert(
            "main".into(),
            ["obstacles".to_owned(), "ar_route".to_owned()]
                .into_iter()
                .collect(),
        );
        let body = demand_body(&d);
        assert_eq!(
            body.cameras["main"],
            vec!["ar_route".to_owned(), "obstacles".to_owned()]
        );
        // No camera model is known before the AR builder ran.
        assert!(body.models.is_empty() || body.models.contains_key("main"));
    }

    /// A fake pilot socket with one session attached to `hub`; returns the
    /// accepted server end after reading the session's `subscribe`.
    async fn attach(hub: &NavHub) -> (tempfile::TempDir, UnixStream, tokio::task::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pilot.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let task = tokio::spawn({
            let hub = hub.clone();
            async move {
                let _ = session(&path, &hub).await;
            }
        });
        let (mut server, _) = listener.accept().await.unwrap();
        let sub = pilot::read_msg(&mut server).await.unwrap().unwrap();
        assert_eq!(pilot::peek_op(&sub).unwrap().0, "subscribe");
        (dir, server, task)
    }

    /// The next message from the session within `wait`, as JSON with its op.
    async fn next_msg(server: &mut UnixStream, wait: Duration) -> Option<(String, Value)> {
        let body = tokio::time::timeout(wait, pilot::read_msg(server))
            .await
            .ok()?
            .unwrap()?;
        let (op, v) = pilot::peek_op(&body).unwrap();
        Some((op, serde_json::to_value(&v).unwrap()))
    }

    #[tokio::test]
    async fn a_frame_split_around_a_demand_change_still_arrives() {
        use tokio::io::AsyncWriteExt;
        let hub = NavHub::new();
        let (_dir, mut server, task) = attach(&hub).await;
        let body = pilot::encode_op("state", &serde_json::json!({"owner": "pilot"})).unwrap();
        let mut framed = (body.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&body);
        let (head, tail) = framed.split_at(framed.len() / 2);
        server.write_all(head).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // The demand changes while the frame is half read.
        hub.status_body_for(Some("main:obstacles"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        server.write_all(tail).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while hub.latest(NavKind::Pilot).is_none() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let (pilot_state, _) = hub
            .latest(NavKind::Pilot)
            .expect("the split frame was lost");
        assert_eq!(pilot_state["owner"], "pilot");
        assert!(!task.is_finished(), "the session dropped the socket");
        task.abort();
    }

    #[tokio::test]
    async fn a_camera_model_change_reaches_the_pilot_at_once() {
        let hub = NavHub::new();
        hub.status_body_for(Some("probe-cam:obstacles"));
        let (_dir, mut server, task) = attach(&hub).await;
        // The demand sent on connect, then quiet until the 2 s refresh.
        while next_msg(&mut server, Duration::from_millis(300))
            .await
            .is_some()
        {}
        let mut models = crate::ar::camera_models();
        models.insert(
            "probe-cam".into(),
            crate::ar::CameraModel {
                frame_w: 1280,
                frame_h: 720,
                hfov_deg: 80.0,
                vfov_deg: 50.0,
                tilt_down_deg: 30.0,
                yaw_deg: 0.0,
                stabilised: true,
            },
        );
        crate::ar::publish_models(models);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        let mut seen = None;
        while let Some((op, v)) = next_msg(
            &mut server,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
        {
            if op == "view_demand" && !v["models"]["probe-cam"].is_null() {
                seen = Some(v);
                break;
            }
        }
        let v = seen.expect("the new camera model did not reach the pilot within 500 ms");
        assert_eq!(v["models"]["probe-cam"]["tilt_down_deg"], 30.0);
        assert_eq!(v["models"]["probe-cam"]["stabilised"], true);
        task.abort();
    }

    #[test]
    fn golden_pilot_and_obstacles_frames_match_the_protocol_types() {
        for (name, text) in [
            (
                "pilot",
                include_str!("../../../tests/fixtures/pilot-frame.json"),
            ),
            (
                "obstacles",
                include_str!("../../../tests/fixtures/obstacles-frame.json"),
            ),
        ] {
            let v: Value = serde_json::from_str(text).unwrap();
            let back = if name == "pilot" {
                serde_json::to_value(
                    serde_json::from_value::<ados_protocol::pilot::PilotState>(v.clone()).unwrap(),
                )
            } else {
                serde_json::to_value(
                    serde_json::from_value::<ados_protocol::pilot::ObstaclesFrame>(v.clone())
                        .unwrap(),
                )
            }
            .unwrap();
            assert_eq!(back, v, "{name} fixture does not round-trip");
        }
    }
}
