//! A tracker batch the vehicle's vision engine published in a run (one
//! walker, track 8, a box 81 px tall, the camera 3.65 m
//! over the ground looking 15° down), through the plugin event export the
//! pilot subscribes to and into its feed: the walker is placed where the
//! camera saw them.

use std::time::Duration;

use ados_protocol::pilot::MoverKind;
use ados_protocol::vision_rpc::owned_model_id;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::harness::World;
use crate::events::{run, TOPIC_DETECTION};
use crate::geo;
use crate::obstacles::Mover;

/// The batch as recorded (`vision.detection` v4 JSON; the appearance
/// embedding cut to its first values).
const BATCH: &str = include_str!("fixtures/detection_batch.json");

/// When its frame was taken, simulation time, ms.
const TAKEN_MS: u64 = 122_567;

/// Where the FC reported the vehicle then (local NED) and its heading.
const AT: [f32; 3] = [-3.08, -1.94, -3.77];
const YAW: f32 = -2.356;

/// What the plugin host's event export does for the pilot's subscription:
/// it answers the subscribe line, then sends the event on `topic` only when
/// the pilot asked for that topic. What the pilot's client hands on.
async fn through_export(topic: &str, payload: Value) -> Option<(String, Value)> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("_events.sock");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let client = tokio::spawn(run(path, tx));
    let (stream, _) = listener.accept().await.unwrap();
    let (rd, mut wr) = stream.into_split();
    let mut req = String::new();
    BufReader::new(rd).read_line(&mut req).await.unwrap();
    let req: Value = serde_json::from_str(&req).unwrap();
    let subscribed = req["topics"]
        .as_array()
        .is_some_and(|t| t.iter().any(|t| t == topic));
    wr.write_all(b"{\"v\":1,\"ok\":true}\n").await.unwrap();
    if subscribed {
        let line = json!({"v": 1, "topic": topic, "publisher": "host", "ts_ms": 0,
            "payload": payload});
        wr.write_all(format!("{line}\n").as_bytes()).await.unwrap();
    }
    let got = tokio::time::timeout(Duration::from_millis(500), rx.recv())
        .await
        .ok()
        .flatten();
    client.abort();
    got
}

/// The movers the pilot holds after `payload` arrives on `topic`, the
/// vehicle hovering where the FC reported it and the batch arriving 200 ms
/// after its frame.
fn placed(topic: &str, payload: &Value) -> Vec<Mover> {
    let mut w = World::new();
    w.now = TAKEN_MS - 400;
    w.yaw = YAW;
    w.airborne_at(crate::modes::copter::GUIDED, AT);
    w.agl = Some(3.65);
    w.run(600);
    w.p.on_feed(topic, payload, w.now);
    w.p.obstacles.movers.list(w.now).copied().collect()
}

#[tokio::test]
async fn a_walker_the_camera_tracks_is_placed_through_the_export() {
    let batch: Value = serde_json::from_str(BATCH).unwrap();
    let (topic, payload) = through_export(TOPIC_DETECTION, batch)
        .await
        .expect("the pilot subscribes to the tracker's batches");

    let people = placed(&topic, &payload);
    assert_eq!(people.len(), 1, "{people:?}");
    assert_eq!(people[0].kind, MoverKind::Person);
    let off = geo::sub(people[0].ne, [AT[0], AT[1]]);
    // A 1.75 m person 81 px tall at a 674 px focal length stands about
    // 15 m off; the box's foot edge on the ground places them at 18.5 m.
    let range = geo::norm(off);
    assert!((14.0..22.0).contains(&range), "{range}");
    // Straight ahead: the box is centred.
    let bearing = geo::wrap_pi(geo::atan2(off[1], off[0]) - YAW);
    assert!(bearing.abs() < 0.05, "{bearing}");
}

/// The same batch as a plugin publishes it (its model under the plugin's
/// namespace), or naming no model, places nothing.
#[test]
fn a_plugins_batch_places_nothing() {
    let mut batch: Value = serde_json::from_str(BATCH).unwrap();
    batch["model_id"] = owned_model_id("com.example.det", "person_yolox_tiny_v1").into();
    assert!(placed(TOPIC_DETECTION, &batch).is_empty());
    batch.as_object_mut().unwrap().remove("model_id");
    assert!(placed(TOPIC_DETECTION, &batch).is_empty());
}
