//! The vision engine's depth broadcast reader (`vision-depth.sock`, records
//! [`ados_protocol::vision_depth::DepthRecord`]).
//!
//! Each depth grid and landing-zone frame is turned into the `depth` and
//! `surfaces` nav kinds and stored in the [`NavHub`], one camera per record:
//!
//! - `depth`: `{v, ts_ms, scale_source, cameras: {<id>: {cols, rows,
//!   scale_source, ts_ms, near, measured}}}`, `near` and `measured` base64.
//!   The hub files each camera's grid on its own, so a client leasing one
//!   camera gets only that camera's grids, each with its own `ts_ms`; the
//!   all-camera value merges the cameras still producing (see
//!   [`crate::routes::nav::DEPTH_CAMERA_STALE_MS`]).
//! - `surfaces`: `{v, ts_ms, camera_id, zones: [{pts, score, slope_deg}]}`,
//!   also kept per camera in the hub.
//!
//! `ts_ms` is the frame's capture time on the wall clock. An absent socket is
//! normal (vision off, or no depth model): the reader retries quietly and the
//! kinds stay `null`.

use std::path::PathBuf;
use std::time::Duration;

use ados_protocol::framebus::mono_ns;
use ados_protocol::ipc::read_length_prefixed;
use ados_protocol::vision_depth::{DepthGrid, DepthRecord, SurfacesFrame};
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::oneshot;

use crate::routes::nav::{NavHub, NavKind};

/// Largest record taken (a grid is under a kilobyte, a zone list a few).
const MAX_RECORD: usize = 64 * 1024;

/// Pause between reconnect attempts.
const RETRY: Duration = Duration::from_secs(2);

/// The broadcast socket under the run directory.
pub fn default_depth_socket() -> PathBuf {
    ados_protocol::paths::run_dir().join("vision-depth.sock")
}

/// The wall-clock milliseconds of a host-monotonic instant.
fn wall_ms_of(mono: u64) -> i64 {
    let now_wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let age_ms = mono_ns().saturating_sub(mono) / 1_000_000;
    now_wall - age_ms as i64
}

/// One record as the nav kind, its `ts_ms` and data to store.
pub fn record_value(record: &DepthRecord) -> (NavKind, i64, Value) {
    match record {
        DepthRecord::Depth(grid) => {
            let ts_ms = wall_ms_of(grid.mono_ns);
            (NavKind::Depth, ts_ms, depth_value(grid, ts_ms))
        }
        DepthRecord::Surfaces(frame) => {
            let ts_ms = wall_ms_of(frame.mono_ns);
            (NavKind::Surfaces, ts_ms, surfaces_value(frame, ts_ms))
        }
    }
}

fn depth_value(g: &DepthGrid, ts_ms: i64) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut cameras = serde_json::Map::new();
    cameras.insert(
        g.camera_id.clone(),
        json!({
            "cols": g.cols,
            "rows": g.rows,
            "scale_source": g.scale_source.as_str(),
            "ts_ms": ts_ms,
            "near": b64.encode(&g.near),
            "measured": b64.encode(&g.measured),
        }),
    );
    json!({
        "v": 1,
        "ts_ms": ts_ms,
        "scale_source": g.scale_source.as_str(),
        "cameras": cameras,
    })
}

fn surfaces_value(frame: &SurfacesFrame, ts_ms: i64) -> Value {
    let round = |v: f32| (f64::from(v) * 1000.0).round() / 1000.0;
    json!({
        "v": 1,
        "ts_ms": ts_ms,
        "camera_id": frame.camera_id,
        "zones": frame.zones.iter().map(|z| json!({
            "pts": z.pts.iter().map(|p| [round(p[0]), round(p[1])]).collect::<Vec<_>>(),
            "score": round(z.score),
            "slope_deg": round(z.slope_deg),
        })).collect::<Vec<_>>(),
    })
}

/// Stops the reader task on shutdown and joins it.
pub struct VisionDepthHandle {
    stop: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl VisionDepthHandle {
    /// Signal the reader to stop and wait for it.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = self.join.await;
    }
}

/// Spawn the reader against `socket_path`, feeding `hub`.
pub fn spawn(socket_path: PathBuf, hub: NavHub) -> VisionDepthHandle {
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        loop {
            tokio::select! {
                r = session(&socket_path, &hub) => {
                    if let Err(e) = r {
                        tracing::debug!(error = %e, path = %socket_path.display(), "vision_depth_reader_unavailable");
                    }
                }
                _ = &mut stop_rx => return,
            }
            tokio::select! {
                _ = tokio::time::sleep(RETRY) => {}
                _ = &mut stop_rx => return,
            }
        }
    });
    VisionDepthHandle {
        stop: Some(stop_tx),
        join,
    }
}

async fn session(path: &std::path::Path, hub: &NavHub) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(path).await?;
    tracing::info!(path = %path.display(), "vision_depth_reader_connected");
    while let Some(body) = read_length_prefixed(&mut stream, MAX_RECORD, true).await? {
        match DepthRecord::from_msgpack(&body) {
            Ok(record) => {
                let (kind, ts_ms, data) = record_value(&record);
                hub.ingest_host(kind, ts_ms, data);
            }
            Err(e) => tracing::warn!(error = %e, "vision_depth_record_unreadable"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ados_protocol::vision_depth::{
        ScaleSource, SurfaceZone, GRID_CELLS, GRID_COLS, GRID_MASK_BYTES, GRID_ROWS,
    };

    /// A grid of `camera` captured `age_ms` ago.
    fn grid(camera: &str, scale: ScaleSource, age_ms: u64) -> DepthRecord {
        DepthRecord::Depth(DepthGrid {
            camera_id: camera.into(),
            mono_ns: mono_ns() - age_ms * 1_000_000,
            scale_source: scale,
            cols: GRID_COLS as u16,
            rows: GRID_ROWS as u16,
            near: vec![7; GRID_CELLS],
            measured: vec![1; GRID_MASK_BYTES],
        })
    }

    fn feed(hub: &NavHub, record: &DepthRecord) {
        let (kind, ts_ms, data) = record_value(record);
        hub.ingest_host(kind, ts_ms, data);
    }

    #[test]
    fn grids_of_several_cameras_merge_and_a_stale_one_drops_out() {
        let hub = NavHub::new();
        feed(&hub, &grid("main", ScaleSource::Lidar, 500));
        let all = hub.status_body();
        assert_eq!(all["depth"]["data"]["scale_source"], "lidar");
        feed(&hub, &grid("down", ScaleSource::None, 0));
        let body = hub.status_body();
        let v = &body["depth"]["data"];
        assert_eq!(v["cameras"].as_object().unwrap().len(), 2);
        // One camera unscaled: the top level says so.
        assert_eq!(v["scale_source"], "none");
        assert_eq!(v["cameras"]["main"]["scale_source"], "lidar");
        // Each camera keeps its own capture time.
        assert!(
            v["cameras"]["main"]["ts_ms"].as_i64().unwrap()
                < v["cameras"]["down"]["ts_ms"].as_i64().unwrap()
        );
        let near = base64::engine::general_purpose::STANDARD
            .decode(v["cameras"]["main"]["near"].as_str().unwrap())
            .unwrap();
        assert_eq!(near.len(), GRID_CELLS);
        // `down` three seconds newer than `main`'s last grid: only `down` stays.
        let hub = NavHub::new();
        feed(&hub, &grid("main", ScaleSource::Lidar, 3_000));
        feed(&hub, &grid("down", ScaleSource::None, 0));
        let all = hub.status_body();
        let cams = all["depth"]["data"]["cameras"].as_object().unwrap();
        assert_eq!(cams.len(), 1);
        assert!(cams.contains_key("down"));
    }

    #[test]
    fn a_surfaces_frame_becomes_the_surfaces_kind_with_the_fixture_shape() {
        let (kind, ts_ms, v) = record_value(&DepthRecord::Surfaces(SurfacesFrame {
            camera_id: "main".into(),
            mono_ns: mono_ns(),
            zones: vec![SurfaceZone {
                pts: vec![[0.31, 0.78], [0.55, 0.78], [0.6, 0.95]],
                score: 0.8213,
                slope_deg: 3.5,
            }],
        }));
        assert_eq!(kind, NavKind::Surfaces);
        assert_eq!(v["ts_ms"], ts_ms);
        assert_eq!(v["camera_id"], "main");
        assert_eq!(v["zones"][0]["score"], 0.821);
        assert_eq!(v["zones"][0]["pts"][1], json!([0.55, 0.78]));
        // The golden fixture carries the same keys.
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/surfaces-frame.json")).unwrap();
        let keys = |o: &Value| {
            let mut k: Vec<String> = o.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&v), keys(&fixture));
        assert_eq!(keys(&v["zones"][0]), keys(&fixture["zones"][0]));
    }

    #[test]
    fn a_depth_kind_has_the_fixture_shape() {
        let (_, ts_ms, v) = record_value(&grid("main", ScaleSource::Lidar, 0));
        assert_eq!(v["cameras"]["main"]["ts_ms"], ts_ms);
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/depth-frame.json")).unwrap();
        let keys = |o: &Value| {
            let mut k: Vec<String> = o.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&v), keys(&fixture));
        assert_eq!(
            keys(&v["cameras"]["main"]),
            keys(&fixture["cameras"]["main"])
        );
    }
}
