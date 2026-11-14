//! Navigation sensor health as the cockpit spec words it
//! (`web-shared/src/cockpit-spec/sensor-health.json`, compiled in): the
//! flight controller's optical flow and downward range and the scanning lidar
//! from the pilot kind's `sensors`, and each camera from `GET /api/v1/sensors`
//! `health.cameras`. The agent classifies; this reads the wire JSON (an
//! unknown state word reads as absent, a camera's as lost) and words each
//! part as the shared `sensor-health.ts` does, so the terminal reads what
//! every other surface reads. The spec's colour names map onto the panel's
//! tones.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::Value;

use super::autonomy::Tone;
use super::spec_text::fill_placeholders;

/// The spec file, byte for byte.
pub const SENSOR_HEALTH_JSON: &str =
    include_str!("../../../../web-shared/src/cockpit-spec/sensor-health.json");

#[derive(Deserialize)]
struct Entry {
    text: String,
    color: String,
}

#[derive(Deserialize)]
struct Spec {
    states: BTreeMap<String, BTreeMap<String, Entry>>,
}

static STATES: LazyLock<BTreeMap<String, BTreeMap<String, Entry>>> = LazyLock::new(|| {
    serde_json::from_str::<Spec>(SENSOR_HEALTH_JSON)
        .expect("sensor-health.json is the sensor health spec")
        .states
});

const FLOW_STATES: [&str; 4] = ["ok", "low", "lost", "absent"];
const RANGE_STATES: [&str; 4] = ["ok", "out_of_range", "lost", "absent"];
const LIDAR_STATES: [&str; 4] = ["ok", "degraded", "lost", "absent"];
const CAMERA_STATES: [&str; 3] = ["live", "stale", "lost"];

/// The placeholder for a missing value, as the shared formatter writes it.
const NO_DATA: &str = "—";

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

fn word_of(v: &Value, key: &str, words: &[&'static str], fallback: &'static str) -> &'static str {
    let s = v.get(key).and_then(Value::as_str).unwrap_or_default();
    words.iter().copied().find(|w| *w == s).unwrap_or(fallback)
}

fn non_empty(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlowHealth {
    pub state: &'static str,
    pub quality: Option<f64>,
    pub rate_hz: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RangeHealth {
    pub state: &'static str,
    pub height_m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LidarHealth {
    pub state: &'static str,
    pub rate_hz: Option<f64>,
    /// The driver's health word (`good`, `warning`, `error`).
    pub health: Option<String>,
}

/// The pilot kind's `sensors`.
#[derive(Debug, Clone, PartialEq)]
pub struct SensorHealth {
    pub flow: FlowHealth,
    pub range: RangeHealth,
    pub lidar: LidarHealth,
}

/// One `health.cameras[]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraHealth {
    pub id: String,
    pub state: &'static str,
    pub fps: Option<f64>,
}

/// One part's spec text and tone.
#[derive(Debug, Clone, PartialEq)]
pub struct SensorWord {
    pub state: &'static str,
    pub text: String,
    /// The spec's colour name (a tokens.json `state.*` key).
    pub color: String,
    pub tone: Tone,
}

impl SensorHealth {
    /// The `sensors` block; `None` when the agent sends none (no FC link).
    pub fn from_value(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        let null = Value::Null;
        let part = |k: &str| v.get(k).unwrap_or(&null);
        let (flow, range, lidar) = (part("flow"), part("range"), part("lidar"));
        Some(Self {
            flow: FlowHealth {
                state: part_state(flow, &FLOW_STATES),
                quality: num(flow, "quality"),
                rate_hz: num(flow, "rate_hz"),
            },
            range: RangeHealth {
                state: part_state(range, &RANGE_STATES),
                height_m: num(range, "height_m"),
            },
            lidar: LidarHealth {
                state: word_of(lidar, "state", &LIDAR_STATES, "absent"),
                rate_hz: num(lidar, "rate_hz"),
                health: non_empty(lidar, "health")
                    .filter(|h| ["good", "warning", "error"].contains(&h.as_str())),
            },
        })
    }

    /// The flow word, then the range word, each only when known and not
    /// absent.
    pub fn row_words(&self) -> Vec<SensorWord> {
        let mut out = Vec::new();
        if !matches!(self.flow.state, "absent" | UNKNOWN) {
            out.push(self.flow_word());
        }
        if !matches!(self.range.state, "absent" | UNKNOWN) {
            out.push(self.range_word());
        }
        out
    }

    pub fn flow_word(&self) -> SensorWord {
        word(
            "flow",
            self.flow.state,
            &[("quality", fmt_int(self.flow.quality))],
        )
    }

    pub fn range_word(&self) -> SensorWord {
        word(
            "range",
            self.range.state,
            &[("height_m", fmt_1(self.range.height_m))],
        )
    }

    pub fn lidar_word(&self) -> SensorWord {
        word(
            "lidar",
            self.lidar.state,
            &[("rate_hz", fmt_1(self.lidar.rate_hz))],
        )
    }
}

impl CameraHealth {
    /// A camera entry; `None` without an id.
    pub fn from_value(v: &Value) -> Option<Self> {
        Some(Self {
            id: non_empty(v, "id")?,
            state: word_of(v, "state", &CAMERA_STATES, "lost"),
            fps: num(v, "fps"),
        })
    }

    /// `GET /api/v1/sensors` `health.cameras`; empty without one.
    pub fn list(sensors: &Value) -> Vec<Self> {
        sensors
            .get("health")
            .and_then(|h| h.get("cameras"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Self::from_value)
            .collect()
    }

    pub fn word(&self) -> SensorWord {
        word(
            "camera",
            self.state,
            &[("id", self.id.clone()), ("fps", fmt_int(self.fps))],
        )
    }
}

fn fmt_int(v: Option<f64>) -> String {
    v.map_or_else(|| NO_DATA.to_string(), |v| format!("{}", v.round() as i64))
}

fn fmt_1(v: Option<f64>) -> String {
    v.map_or_else(|| NO_DATA.to_string(), |v| format!("{v:.1}"))
}

/// A null flow or range part: the agent has no fresh word from the flight
/// controller, so the part reads unknown, never "No flow".
const UNKNOWN: &str = "unknown";

fn part_state(v: &Value, words: &[&'static str]) -> &'static str {
    if v.is_null() {
        UNKNOWN
    } else {
        word_of(v, "state", words, "absent")
    }
}

fn word(part: &str, state: &'static str, args: &[(&str, String)]) -> SensorWord {
    if state == UNKNOWN {
        return SensorWord {
            state,
            text: NO_DATA.to_string(),
            color: "stale".to_string(),
            tone: Tone::Neutral,
        };
    }
    let entry = &STATES[part][state];
    let text = fill_placeholders(&entry.text, |name| {
        args.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
    });
    let tone = match entry.color.as_str() {
        "ready" => Tone::Good,
        "avoiding" => Tone::Caution,
        "not_ready" => Tone::Fault,
        _ => Tone::Neutral,
    };
    SensorWord {
        state,
        text,
        color: entry.color.clone(),
        tone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared behaviour cases every surface runs.
    const CASES: &str =
        include_str!("../../../../web-shared/src/hud/fixtures/sensor-health-cases.json");

    fn plain(w: &SensorWord) -> Value {
        serde_json::json!({"state": w.state, "text": w.text, "color": w.color})
    }

    fn sensors_with(part: &str, v: &Value) -> SensorHealth {
        SensorHealth::from_value(&serde_json::json!({ part: v })).expect("an object")
    }

    #[test]
    fn the_shared_cases_word_alike() {
        let cases: Vec<Value> = serde_json::from_str(CASES).expect("cases are JSON");
        let mut ran = 0;
        for c in &cases {
            let (name, input, want) = (&c["name"], &c["input"], &c["expected"]);
            let got = match c["fn"].as_str().unwrap_or_default() {
                "flowWord" => plain(&sensors_with("flow", input).flow_word()),
                "rangeWord" => plain(&sensors_with("range", input).range_word()),
                "lidarWord" => plain(&sensors_with("lidar", input).lidar_word()),
                "cameraWord" => {
                    CameraHealth::from_value(input).map_or(Value::Null, |c| plain(&c.word()))
                }
                "sensorRowWords" => SensorHealth::from_value(input)
                    .map(|s| s.row_words().iter().map(|w| w.text.clone()).collect())
                    .unwrap_or_else(|| Value::Array(Vec::new())),
                // Tab and leg matching are the web surfaces' own.
                _ => continue,
            };
            assert_eq!(&got, want, "case {name}");
            ran += 1;
        }
        assert!(ran >= 20, "ran {ran} cases");
    }

    #[test]
    fn spec_colours_map_onto_tones() {
        let s = sensors_with(
            "lidar",
            &serde_json::json!({"state": "degraded", "rate_hz": 4.0}),
        );
        assert_eq!(s.lidar_word().tone, Tone::Caution);
        assert_eq!(s.flow_word().tone, Tone::Neutral);
        let lost = sensors_with("range", &serde_json::json!({"state": "lost"}));
        assert_eq!(lost.range_word().tone, Tone::Fault);
    }

    #[test]
    fn cameras_come_from_the_sensors_route_health() {
        let body = serde_json::json!({"health": {"cameras": [
            {"id": "front", "state": "live", "fps": 29.6},
            {"state": "live"},
            {"id": "belly", "state": "stale"}
        ]}});
        let cams = CameraHealth::list(&body);
        let words: Vec<String> = cams.iter().map(|c| c.word().text).collect();
        assert_eq!(words, ["front 30 fps", "belly stale"]);
        assert!(CameraHealth::list(&serde_json::json!({"present": true})).is_empty());
    }
}
