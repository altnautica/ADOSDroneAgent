//! `GET /api/v1/mission`: the flight controller's main mission as the router's
//! mission mirror holds it, every MISSION_ITEM_INT field in field order, with
//! the live mission state (`mission`) from the vehicle-state snapshot.
//!
//! Body: `{rev, present, count, home_slot, uploads_seen, mission, items:[{param1,
//! param2, param3, param4, x, y, z, seq, command, frame, current, autocontinue,
//! mission_type}]}`. `x`/`y` are latitude/longitude in 1e-7 degrees, `z` the
//! altitude in metres in the item's `frame`. Before the router has mirrored
//! anything, `present` is false and `items` empty.

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::state::AppState;

/// The mirror file the router writes under the runtime directory.
pub const MIRROR_FILE: &str = "mission-mirror.json";

/// The route body from the mirror file text and the state snapshot.
pub fn mission_body(mirror: Option<&str>, snapshot: Option<&Value>) -> Value {
    let mut body = mirror
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| {
            json!({"rev": 0, "present": false, "count": 0, "home_slot": null, "items": []})
        });
    let mission = snapshot.and_then(|s| s.get("mission")).filter(|m| m.is_object());
    let uploads = mission.and_then(|m| m.get("uploads_seen")).cloned().unwrap_or(json!(0));
    let wire = mission.map(|m| {
        let mut w = m.clone();
        if let Value::Object(o) = &mut w {
            o.remove("items");
            o.remove("rev");
            o.remove("uploads_seen");
        }
        w
    });
    if let Value::Object(o) = &mut body {
        o.insert("uploads_seen".into(), uploads);
        o.insert("mission".into(), wire.unwrap_or(Value::Null));
    }
    body
}

/// `GET /api/v1/mission`.
pub async fn get_mission(State(state): State<AppState>) -> Json<Value> {
    let path = state.run_dir.join(MIRROR_FILE);
    let text = tokio::task::spawn_blocking(move || std::fs::read_to_string(path).ok())
        .await
        .ok()
        .flatten();
    let snapshot = state.state.snapshot();
    Json(mission_body(text.as_deref(), snapshot.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_mirror_with_the_live_state() {
        let mirror = r#"{"rev":2,"present":true,"count":1,"home_slot":true,
            "items":[{"param1":0.0,"param2":0.0,"param3":0.0,"param4":0.0,"x":470000000,
            "y":85000000,"z":30.0,"seq":0,"command":16,"frame":3,"current":0,
            "autocontinue":1,"mission_type":0}]}"#;
        let snap = json!({"mission": {"state": "active", "seq": 1, "rev": 2, "uploads_seen": 3,
                                     "items": [{"seq": 1}]}});
        let b = mission_body(Some(mirror), Some(&snap));
        assert_eq!(b["rev"], 2);
        assert_eq!(b["items"][0]["command"], 16);
        assert_eq!(b["uploads_seen"], 3);
        assert_eq!(b["mission"]["state"], "active");
        assert!(b["mission"].get("items").is_none());
    }

    #[test]
    fn nothing_mirrored_yet() {
        let b = mission_body(None, None);
        assert_eq!(b["present"], false);
        assert_eq!(b["items"], json!([]));
        assert_eq!(b["mission"], Value::Null);
    }
}
