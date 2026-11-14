//! Navigation sensor health from `/api/v1/sensors` `health`, worded and
//! coloured by the cockpit spec (`sensor-health.json` states, `tokens.json`
//! `state.*` colours, both compiled in), so the panel reads what every other
//! surface reads: `Flow 212`, `Range 3.4 m`, `Lidar 9.9 Hz`, `No lidar`,
//! `front 30 fps`. The agent classifies; an unknown state word reads as
//! absent (a camera's as lost). A `null` flow or range means the agent has no
//! fresh word from the flight controller and reads unknown (`—`).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use embedded_graphics::pixelcolor::Rgb888;
use serde::Deserialize;
use serde_json::Value;

const SENSOR_HEALTH_JSON: &str =
    include_str!("../../../../web-shared/src/cockpit-spec/sensor-health.json");
const TOKENS_JSON: &str = include_str!("../../../../web-shared/src/cockpit-spec/tokens.json");

#[derive(Deserialize)]
struct Entry {
    text: String,
    color: String,
}

#[derive(Deserialize)]
struct Spec {
    states: BTreeMap<String, BTreeMap<String, Entry>>,
}

#[derive(Deserialize)]
struct Tokens {
    state: BTreeMap<String, String>,
}

static STATES: LazyLock<BTreeMap<String, BTreeMap<String, Entry>>> = LazyLock::new(|| {
    serde_json::from_str::<Spec>(SENSOR_HEALTH_JSON)
        .expect("sensor-health.json is the sensor health spec")
        .states
});

static STATE_COLORS: LazyLock<BTreeMap<String, String>> = LazyLock::new(|| {
    serde_json::from_str::<Tokens>(TOKENS_JSON)
        .expect("tokens.json is the cockpit colour table")
        .state
});

/// The placeholder for a missing value, as the shared formatter writes it.
const NO_DATA: &str = "—";

/// One part's spec text and colour.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthWord {
    pub state: String,
    pub text: String,
    /// The spec's colour name (a tokens.json `state.*` key).
    pub color_name: String,
    pub color: Rgb888,
}

/// The flight controller's flow and range, the lidar and the cameras, worded.
#[derive(Debug, Clone, PartialEq)]
pub struct NavHealth {
    pub flow: HealthWord,
    pub range: HealthWord,
    pub lidar: HealthWord,
    /// The lidar driver's health word (`good`, `warning`, `error`).
    pub lidar_health: Option<String>,
    /// One word per `health.cameras` row that carries an id, in wire order.
    pub cameras: Vec<HealthWord>,
}

fn hex(s: &str) -> Rgb888 {
    let n = u32::from_str_radix(s.trim_start_matches('#').get(..6).unwrap_or("909090"), 16)
        .unwrap_or(0x90_90_90);
    Rgb888::new((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

fn fmt(v: Option<f64>, decimals: usize) -> String {
    match v {
        Some(v) if decimals == 0 => format!("{}", v.round() as i64),
        Some(v) => format!("{v:.decimals$}"),
        None => NO_DATA.to_string(),
    }
}

fn health_word(state: &str, entry: &Entry, text: String) -> HealthWord {
    HealthWord {
        state: state.to_string(),
        text,
        color_name: entry.color.clone(),
        color: hex(STATE_COLORS
            .get(&entry.color)
            .map_or("#909090", String::as_str)),
    }
}

/// The finite number at `v[key]`, formatted to `decimals`.
fn num(v: &Value, key: &str, decimals: usize) -> String {
    fmt(
        v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite()),
        decimals,
    )
}

/// `table`'s key for the wire `state`, or `fallback` when the table has none.
fn state_key<'a>(table: &BTreeMap<String, Entry>, v: &'a Value, fallback: &'a str) -> &'a str {
    let wire = v.get("state").and_then(Value::as_str).unwrap_or_default();
    if table.contains_key(wire) {
        wire
    } else {
        fallback
    }
}

/// `part`'s word for its wire `state` (unknown reads as `absent`), with the
/// one `{arg}` its texts take filled from `value`.
fn word(part: &str, v: &Value, arg: &str, decimals: usize) -> HealthWord {
    let table = &STATES[part];
    let state = state_key(table, v, "absent");
    let entry = &table[state];
    let text = entry
        .text
        .replace(&format!("{{{arg}}}"), &num(v, arg, decimals));
    health_word(state, entry, text)
}

/// A flow or range word; `null` (no fresh word from the flight controller)
/// reads unknown: the placeholder in the stale colour, never "No flow".
fn fc_word(part: &str, v: &Value, arg: &str, decimals: usize) -> HealthWord {
    if v.is_null() {
        let stale = Entry {
            text: NO_DATA.to_string(),
            color: "stale".to_string(),
        };
        return health_word("unknown", &stale, NO_DATA.to_string());
    }
    word(part, v, arg, decimals)
}

/// One `health.cameras` row's word (an unknown state reads as `lost`); `None`
/// for a row without an id.
fn camera_word(v: &Value) -> Option<HealthWord> {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let table = &STATES["camera"];
    let state = state_key(table, v, "lost");
    let entry = &table[state];
    let text = entry
        .text
        .replace("{id}", id)
        .replace("{fps}", &num(v, "fps", 0));
    Some(health_word(state, entry, text))
}

/// The route's `health` object; `None` when the body carries none.
pub fn nav_health(health: &Value) -> Option<NavHealth> {
    if !health.is_object() {
        return None;
    }
    let null = Value::Null;
    let part = |k: &str| health.get(k).unwrap_or(&null);
    Some(NavHealth {
        flow: fc_word("flow", part("flow"), "quality", 0),
        range: fc_word("range", part("range"), "height_m", 1),
        lidar: word("lidar", part("lidar"), "rate_hz", 1),
        lidar_health: part("lidar")
            .get("health")
            .and_then(Value::as_str)
            .filter(|h| ["good", "warning", "error"].contains(h))
            .map(str::to_string),
        cameras: part("cameras")
            .as_array()
            .map(|rows| rows.iter().filter_map(camera_word).collect())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared behaviour cases every surface runs.
    const CASES: &str =
        include_str!("../../../../web-shared/src/hud/fixtures/sensor-health-cases.json");

    #[test]
    fn the_shared_cases_word_alike() {
        let cases: Vec<Value> = serde_json::from_str(CASES).expect("cases are JSON");
        let mut ran = 0;
        for c in &cases {
            let as_json = |w: &HealthWord| serde_json::json!({"state": w.state, "text": w.text, "color": w.color_name});
            let got = match c["fn"].as_str().unwrap_or_default() {
                "cameraWord" => camera_word(&c["input"])
                    .as_ref()
                    .map_or(Value::Null, as_json),
                f @ ("flowWord" | "rangeWord" | "lidarWord") => {
                    let part = &f[..f.len() - 4];
                    let h =
                        nav_health(&serde_json::json!({ part: c["input"] })).expect("an object");
                    as_json(match part {
                        "flow" => &h.flow,
                        "range" => &h.range,
                        _ => &h.lidar,
                    })
                }
                _ => continue,
            };
            assert_eq!(got, c["expected"], "case {}", c["name"]);
            ran += 1;
        }
        assert!(ran >= 20, "ran {ran} cases");
    }

    #[test]
    fn cameras_take_the_spec_words() {
        let h = nav_health(&serde_json::json!({"cameras": [
            {"id": "front", "state": "live", "fps": 29.7},
            {"id": "belly", "state": "stale", "fps": 2},
            {"state": "live", "fps": 30},
            {"id": "rear", "state": "warming"},
        ]}))
        .expect("an object");
        let words: Vec<_> = h
            .cameras
            .iter()
            .map(|w| (w.text.as_str(), w.color_name.as_str()))
            .collect();
        assert_eq!(
            words,
            [
                ("front 30 fps", "ready"),
                ("belly stale", "avoiding"),
                ("rear lost", "not_ready"),
            ]
        );
    }

    #[test]
    fn null_flow_and_range_read_unknown() {
        let h = nav_health(&serde_json::json!({"flow": null, "range": null})).expect("an object");
        assert_eq!([h.flow.text.as_str(), h.range.text.as_str()], ["—", "—"]);
        assert_eq!(
            [h.flow.state.as_str(), h.range.state.as_str()],
            ["unknown", "unknown"]
        );
        assert_eq!(h.flow.color_name, "stale");
    }

    #[test]
    fn colours_are_the_token_colours() {
        let h = nav_health(&serde_json::json!({"lidar": {"state": "lost"}})).expect("an object");
        assert_eq!(h.lidar.color, Rgb888::new(0xEF, 0x44, 0x44));
        assert_eq!(h.flow.color, Rgb888::new(0x90, 0x90, 0x90));
        assert!(nav_health(&Value::Null).is_none());
    }
}
