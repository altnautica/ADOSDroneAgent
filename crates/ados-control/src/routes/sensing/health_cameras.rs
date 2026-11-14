//! `health.cameras` in `GET /api/v1/sensors`: one row per camera,
//! `{id, role, leg, state, fps, age_ms}`.
//!
//! The cameras are the configured `cameras[]` entries (enabled ones, and the
//! primary always) in config order, then any other camera the vision engine
//! reports. Per camera, the frame rate and last-frame age come from, in order:
//!
//! 1. the vision engine's sidecar (`vision-status.json` `cameras.<id>`
//!    `{fps, last_frame_age_ms}`, the age measured when the sidecar was
//!    written, so the sidecar's own age is added; a sidecar older than
//!    [`VISION_STATUS_FRESH`] is ignored). The engine reads a primary it does
//!    not capture through the video tap as `main`; that reading belongs to the
//!    configured primary row;
//! 2. the sensor bus trigger stream the camera's `trigger` setting names
//!    (`sensors.json`, stream `cam<channel>` from the hub or `cam0` from the
//!    flight controller);
//! 3. the video pipeline saying the camera's leg publishes (live, no rate or
//!    age known): `camera-state.json` for the primary leg, the leg's `live`
//!    flag in `video-streams.json` for another.
//!
//! A camera with a source error from the vision engine, or no reading at all,
//! is `lost`; otherwise its age against the spec thresholds makes it `live`,
//! `stale` or `lost`. `leg` is the video stream leg it feeds: `main` for the
//! primary, its own id for another enabled entry, `null` for none.

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use ados_config::cameras::CameraTrigger;
use ados_protocol::vision_status::VISION_STATUS_FILE;
use ados_video::camera_presence::VISION_STATUS_FRESH;
use serde_json::{json, Map, Value};

use super::health::Thresholds;
use super::{fresh_map, SensingPaths, FRESH};

/// The registry freshness window of `camera-state.json`, seconds.
const CAMERA_STATE_FRESH_S: f64 = ados_protocol::paths::CAMERA_STATE_MAX_AGE_MS as f64 / 1000.0;

/// The freshness window of `video-streams.json`, seconds.
const VIDEO_STREAMS_FRESH_S: f64 = ados_protocol::paths::VIDEO_STREAMS_MAX_AGE_MS as f64 / 1000.0;

/// The primary leg's fixed id.
const MAIN_LEG: &str = "main";

/// One camera's frame rate and last-frame age.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Frames {
    fps: Option<f64>,
    age_ms: Option<u64>,
}

/// A configured camera.
struct Configured {
    id: String,
    role: Option<String>,
    leg: Option<String>,
    /// The vision engine owns this camera's device.
    capture: bool,
    /// The sensor bus trigger stream `(origin, src)` of this camera.
    trigger: Option<(&'static str, String)>,
}

/// The bus stream a camera's triggers arrive on, as the vision engine joins
/// them: hub output `cam<channel>`, or the flight controller's `cam0`.
fn trigger_stream(trigger: CameraTrigger, channel: u8) -> Option<(&'static str, String)> {
    match trigger {
        CameraTrigger::None => None,
        CameraTrigger::Hub => Some(("hub", format!("cam{channel}"))),
        CameraTrigger::Fc => Some(("fc", "cam0".to_string())),
    }
}

/// The enabled configured cameras (and the primary), with their legs.
fn configured(config_text: &str) -> Vec<Configured> {
    let valid = ados_config::cameras::camera_entries(config_text).valid;
    let primary = valid
        .iter()
        .position(|(_, _, c)| c.role.as_deref() == Some("primary"))
        .unwrap_or(0);
    valid
        .into_iter()
        .enumerate()
        .filter(|(i, (_, _, c))| *i == primary || c.enabled)
        .map(|(i, (_, _, c))| Configured {
            leg: Some(if i == primary {
                MAIN_LEG.to_string()
            } else {
                c.id.clone()
            }),
            capture: c.capture,
            trigger: trigger_stream(c.trigger, c.trigger_channel.unwrap_or(0)),
            id: c.id,
            role: c.role,
        })
        .collect()
}

/// The vision engine's per-camera frames and source errors; nothing from a
/// sidecar older than [`VISION_STATUS_FRESH`] (the engine stopped).
fn vision(paths: &SensingPaths, now: SystemTime) -> (BTreeMap<String, Frames>, Vec<String>) {
    let path = paths.sidecar(VISION_STATUS_FILE);
    let Some(doc) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return Default::default();
    };
    let written_ago = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| now.duration_since(m).ok())
        .unwrap_or_default();
    if written_ago > VISION_STATUS_FRESH {
        return Default::default();
    }
    let written_ago_ms = u64::try_from(written_ago.as_millis()).unwrap_or(u64::MAX);
    let mut frames = BTreeMap::new();
    if let Some(cams) = doc.get("cameras").and_then(Value::as_object) {
        for (id, c) in cams {
            frames.insert(
                id.clone(),
                Frames {
                    fps: c.get("fps").and_then(Value::as_f64),
                    age_ms: c
                        .get("last_frame_age_ms")
                        .and_then(Value::as_u64)
                        .map(|a| a.saturating_add(written_ago_ms)),
                },
            );
        }
    }
    let errors = doc
        .get("source_errors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("camera")?.as_str().map(str::to_owned))
        .collect();
    (frames, errors)
}

/// The sensor bus trigger streams by `(origin, src)`.
fn triggers(
    paths: &SensingPaths,
    now: SystemTime,
    now_mono_ns: u64,
) -> BTreeMap<(String, String), Frames> {
    let Some(streams) = fresh_map(&paths.sidecar("sensors.json"), Some(FRESH), now) else {
        return BTreeMap::new();
    };
    streams
        .get("streams")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s["kind"] == "trigger")
        .filter_map(|s| {
            let last_rx = s["last_rx_ns"].as_u64().filter(|r| *r > 0)?;
            Some((
                (
                    s["origin"].as_str()?.to_owned(),
                    s["src"].as_str()?.to_owned(),
                ),
                Frames {
                    fps: s["rate_hz"].as_f64(),
                    age_ms: now_mono_ns.checked_sub(last_rx).map(|d| d / 1_000_000),
                },
            ))
        })
        .collect()
}

fn unix_s(now: SystemTime) -> f64 {
    now.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Whether the video pipeline reports the primary leg publishing.
fn main_streaming(paths: &SensingPaths, now: SystemTime) -> bool {
    ados_video::camera_state::read_main_stream_live(
        &paths.sidecar("camera-state.json"),
        unix_s(now),
        CAMERA_STATE_FRESH_S,
    )
}

/// The legs the video orchestrator reports live in a fresh
/// `video-streams.json`.
fn live_legs(paths: &SensingPaths, now: SystemTime) -> BTreeSet<String> {
    let Some(doc) = std::fs::read_to_string(paths.sidecar("video-streams.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
    else {
        return BTreeSet::new();
    };
    let updated = doc
        .get("updated_at_unix")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if updated <= 0.0 || unix_s(now) - updated > VIDEO_STREAMS_FRESH_S {
        return BTreeSet::new();
    }
    doc.get("streams")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| s.get("live").and_then(Value::as_bool) == Some(true))
        .filter_map(|s| s.get("id")?.as_str().map(str::to_owned))
        .collect()
}

/// The verdict for a camera's frames.
fn state(frames: Option<Frames>, errored: bool, streaming: bool, t: &Thresholds) -> &'static str {
    if errored {
        return "lost";
    }
    match frames.and_then(|f| f.age_ms) {
        Some(age) if age > t.camera_lost_ms => "lost",
        Some(age) if age > t.camera_stale_ms => "stale",
        Some(_) => "live",
        None if streaming => "live",
        None => "lost",
    }
}

/// The `health.cameras` rows.
pub(crate) fn camera_rows(
    paths: &SensingPaths,
    config_text: &str,
    now: SystemTime,
    now_mono_ns: u64,
    t: &Thresholds,
) -> Value {
    let mut cams = configured(config_text);
    let (vision_frames, errors) = vision(paths, now);
    let bus = triggers(paths, now, now_mono_ns);
    // The engine's `main` is the tap of a primary it does not capture: the
    // configured primary row's reading, not a camera of its own.
    let tapped_primary = cams
        .iter()
        .find(|c| c.leg.as_deref() == Some(MAIN_LEG))
        .filter(|c| !c.capture && c.id != MAIN_LEG)
        .map(|c| c.id.clone());
    let engine_id = |id: &str| -> String {
        match &tapped_primary {
            Some(p) if p == id => MAIN_LEG.to_string(),
            _ => id.to_string(),
        }
    };
    let extra: BTreeSet<&String> = vision_frames.keys().chain(errors.iter()).collect();
    let legacy = cams.is_empty();
    for id in extra {
        if tapped_primary.is_some() && id == MAIN_LEG {
            continue;
        }
        if !cams.iter().any(|c| &c.id == id) {
            // With no `cameras[]` the single legacy leg is `main`.
            let leg = (legacy && id == MAIN_LEG).then(|| MAIN_LEG.to_string());
            cams.push(Configured {
                id: id.clone(),
                role: None,
                leg,
                capture: false,
                trigger: None,
            });
        }
    }
    let primary_streaming = main_streaming(paths, now);
    let live = live_legs(paths, now);
    let rows: Vec<Value> = cams
        .into_iter()
        .map(|c| {
            let eid = engine_id(&c.id);
            let frames = vision_frames
                .get(&eid)
                .or_else(|| {
                    let (origin, src) = c.trigger.as_ref()?;
                    bus.get(&((*origin).to_string(), src.clone()))
                })
                .copied();
            let errored = errors.contains(&eid);
            let streaming = match c.leg.as_deref() {
                Some(MAIN_LEG) => primary_streaming,
                Some(leg) => live.contains(leg),
                None => false,
            };
            let mut row = Map::new();
            row.insert("id".into(), json!(c.id));
            row.insert("role".into(), json!(c.role));
            row.insert("leg".into(), json!(c.leg));
            row.insert("state".into(), json!(state(frames, errored, streaming, t)));
            row.insert("fps".into(), json!(frames.and_then(|f| f.fps)));
            row.insert("age_ms".into(), json!(frames.and_then(|f| f.age_ms)));
            Value::Object(row)
        })
        .collect();
    Value::Array(rows)
}
