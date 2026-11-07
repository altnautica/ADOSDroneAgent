//! The pilot's `state` and `obstacles` into the nav hub, and the hub's view
//! demand back to the pilot.

use std::path::PathBuf;
use std::time::Duration;

use ados_protocol::pilot::{self, CameraModel, ViewDemand};
use serde_json::Value;
use tokio::net::UnixStream;

use crate::routes::nav::{NavHub, NavKind};

/// Re-send the view demand this often even when it did not change, so a
/// restarted pilot learns it within one period.
const DEMAND_REFRESH: Duration = Duration::from_secs(2);

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
                },
            );
        }
    }
    out
}

/// Fill the published `mission` from the router's mission state (`mission` of
/// the vehicle state), which the pilot does not compute itself.
pub type MissionSource = Box<dyn Fn() -> Option<Value> + Send + Sync>;

/// Run forever: subscribe to the pilot, feed the hub, push view demand.
pub async fn run(path: PathBuf, hub: NavHub, mission: MissionSource) {
    loop {
        if let Err(e) = session(&path, &hub, &mission).await {
            tracing::debug!(error = %e, "pilot_ingest_disconnected");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn session(path: &PathBuf, hub: &NavHub, mission: &MissionSource) -> std::io::Result<()> {
    let stream = UnixStream::connect(path).await?;
    let (mut rd, mut wr) = stream.into_split();
    let sub = pilot::encode_op("subscribe", &()).map_err(std::io::Error::other)?;
    pilot::write_msg(&mut wr, &sub)
        .await
        .map_err(std::io::Error::other)?;
    tracing::info!("pilot_ingest_connected");
    let mut demand = hub.view_demand();
    let mut refresh = tokio::time::interval(DEMAND_REFRESH);
    loop {
        tokio::select! {
            msg = pilot::read_msg(&mut rd) => {
                let Some(body) = msg.map_err(std::io::Error::other)? else { return Ok(()) };
                let Ok((op, v)) = pilot::peek_op(&body) else { continue };
                let Ok(mut json) = serde_json::to_value(&v) else { continue };
                if let Some(m) = json.as_object_mut() {
                    m.remove("op");
                }
                match op.as_str() {
                    "state" => {
                        if let (Some(m), Some(mut mission)) = (json.as_object_mut(), mission()) {
                            // The pilot kind carries the mission-state object only.
                            if let Some(o) = mission.as_object_mut() {
                                o.remove("items");
                                o.remove("rev");
                                o.remove("uploads_seen");
                            }
                            m.insert("mission".into(), mission);
                        }
                        hub.ingest_host(NavKind::Pilot, epoch_ms(), json);
                    }
                    "obstacles" => {
                        // The pilot stamps its own monotonic clock; surfaces
                        // compare `ts_ms` with wall time.
                        if let Some(m) = json.as_object_mut() {
                            m.insert("ts_ms".into(), Value::from(epoch_ms()));
                        }
                        hub.ingest_host(NavKind::Obstacles, epoch_ms(), json);
                    }
                    _ => {}
                }
            }
            changed = demand.changed() => {
                if changed.is_err() { return Ok(()); }
                let d = demand.borrow_and_update().clone();
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
