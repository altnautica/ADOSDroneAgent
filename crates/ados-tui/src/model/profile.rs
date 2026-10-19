//! The per-profile panel row: visor capture/clock/hub state, ground-station
//! radios/GNSS/LoRa, and the drone sensor-rate line.
//!
//! Each line is built only from route bodies the agent actually answered with
//! `present` not `false`; a missing route leaves its segment out rather than
//! showing a placeholder value.

use super::*;

/// The world-engine capture state route, reached through the plugin proxy.
pub const CAPTURE_STATE_PATH: &str =
    "/api/plugins/com.altnautica.world-engine/x/atlas/capture/state";

/// Colour class of one segment of a profile row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// A dim label or separator.
    Label,
    /// A value inside its normal range.
    Value,
    /// A value at 70–99 % of its limit, or a degraded state.
    Warn,
    /// A value at its limit, or a blocked state.
    Error,
}

/// One rendered line of the profile row as styled segments.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RowLine(pub Vec<(String, Level)>);

impl RowLine {
    pub(super) fn push(&mut self, text: impl Into<String>, level: Level) {
        self.0.push((text.into(), level));
    }

    /// The plain text of the line.
    #[cfg(test)]
    pub fn text(&self) -> String {
        self.0.iter().map(|(t, _)| t.as_str()).collect()
    }
}

/// The optional route bodies a profile polls. `None` = not polled, failed, or
/// answered `present: false`.
#[derive(Debug, Clone, Default)]
pub struct ProfileRoutes {
    pub sensors: Option<Value>,
    pub clock: Option<Value>,
    pub storage: Option<Value>,
    pub gnss: Option<Value>,
    pub lora: Option<Value>,
    pub radio: Option<Value>,
    pub capture: Option<Value>,
}

/// Which optional routes a profile polls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteSet {
    pub sensors: bool,
    pub clock: bool,
    pub storage: bool,
    pub gnss: bool,
    pub lora: bool,
    pub radio: bool,
    pub capture: bool,
}

impl RouteSet {
    /// The routes polled for a wire profile name.
    pub fn for_profile(profile: &str) -> Self {
        match profile {
            "visor" => Self {
                sensors: true,
                clock: true,
                storage: true,
                capture: true,
                ..Self::default()
            },
            "drone" => Self {
                sensors: true,
                ..Self::default()
            },
            "ground_station" => Self {
                gnss: true,
                lora: true,
                radio: true,
                ..Self::default()
            },
            _ => Self::default(),
        }
    }
}

/// Keep a route body unless it says `present: false`.
pub fn present(body: Value) -> Option<Value> {
    (body.get("present").and_then(Value::as_bool) != Some(false)).then_some(body)
}

/// A percentage with at most one decimal and no trailing `.0`.
pub(super) fn pct(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// `mm:ss` for a duration in seconds (minutes are not wrapped).
pub(super) fn mmss(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

/// Threshold colour for a used/limit ratio: < 70 % value, 70–99 % warn, at or
/// over the limit error.
pub(super) fn ratio_level(ratio: f64) -> Level {
    if ratio >= 1.0 {
        Level::Error
    } else if ratio >= 0.7 {
        Level::Warn
    } else {
        Level::Value
    }
}

fn f64_at(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

impl Dashboard {
    /// Merge the profile routes polled this cycle and rebuild the profile row.
    pub fn merge_profile(&mut self, routes: &ProfileRoutes) {
        let connected = |key: &str| {
            routes
                .sensors
                .as_ref()
                .and_then(|s| s.get(key))
                .and_then(|o| o.get("connected"))
                .and_then(Value::as_bool)
        };
        self.hub_connected = connected("hub");
        self.sensors_fc_connected = connected("fc");
        self.profile_row = match self.profile.as_str() {
            "visor" => visor_line(routes).into_iter().collect(),
            "drone" => routes
                .sensors
                .as_ref()
                .and_then(sensor_rates_line)
                .into_iter()
                .collect(),
            "ground_station" => super::profile_gs::gs_lines(routes),
            _ => Vec::new(),
        };
    }
}

fn sep(line: &mut RowLine) {
    if !line.0.is_empty() {
        line.push(" │ ", Level::Label);
    }
}

/// `visor-1-20261019T101500Z-3fa2` → `visor-1-…-3fa2`.
fn short_session(id: &str) -> String {
    let parts: Vec<&str> = id.split('-').collect();
    if parts.len() <= 3 {
        return id.to_string();
    }
    format!("{}-{}-…-{}", parts[0], parts[1], parts[parts.len() - 1])
}

/// Capture used/budget, or the capture volume's used share when no budget.
fn storage_pct(storage: &Value) -> Option<f64> {
    let capture = storage.get("capture");
    let used = capture.and_then(|c| f64_at(c, "used_bytes"));
    let budget = capture
        .and_then(|c| f64_at(c, "budget_bytes"))
        .filter(|b| *b > 0.0);
    if let (Some(used), Some(budget)) = (used, budget) {
        return Some(used / budget * 100.0);
    }
    let volume = storage
        .get("volumes")?
        .as_array()?
        .iter()
        .find(|v| v.get("is_capture").and_then(Value::as_bool) == Some(true))?;
    let total = f64_at(volume, "total_bytes").filter(|t| *t > 0.0)?;
    let free = f64_at(volume, "free_bytes")?;
    Some((total - free) / total * 100.0)
}

fn visor_line(routes: &ProfileRoutes) -> Option<RowLine> {
    let mut line = RowLine::default();
    if let Some(cap) = &routes.capture {
        if let Some(state) = cap.get("state").and_then(Value::as_str) {
            let word = match state {
                "recording" => "REC",
                "paused" => "PAUSED",
                "closing" => "CLOSING",
                _ => "IDLE",
            };
            line.push("capture ", Level::Label);
            let mut value = word.to_string();
            if state != "idle" {
                if let Some(d) = f64_at(cap, "duration_s") {
                    value = format!("{value} {}", mmss(d));
                }
            }
            line.push(value, Level::Value);
            if let Some(id) = cap
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                sep(&mut line);
                line.push(short_session(id), Level::Value);
            }
        }
    }
    if let Some(storage) = &routes.storage {
        if let Some(p) = storage_pct(storage) {
            let level = match storage.pointer("/capture/state").and_then(Value::as_str) {
                Some("full") => Level::Error,
                Some("near_full") => Level::Warn,
                _ => ratio_level(p / 100.0),
            };
            sep(&mut line);
            line.push("storage ", Level::Label);
            line.push(format!("{p:.0} %"), level);
        }
    }
    if let Some(sigma) = routes
        .clock
        .as_ref()
        .and_then(|c| f64_at(c, "sigma_ns"))
        .filter(|s| *s >= 0.0)
    {
        sep(&mut line);
        line.push("clock σ ", Level::Label);
        line.push(format!("{:.2} ms", sigma / 1e6), Level::Value);
    }
    let hub_imu = routes
        .sensors
        .as_ref()
        .and_then(|s| s.get("sensors"))
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter().find(|s| {
                s.get("kind").and_then(Value::as_str) == Some("imu")
                    && s.get("source").and_then(Value::as_str) == Some("hub")
            })
        })
        .and_then(|s| f64_at(s, "rate_hz"));
    if let Some(rate) = hub_imu {
        sep(&mut line);
        line.push("hub imu ", Level::Label);
        line.push(format!("{rate:.0} Hz"), Level::Value);
    }
    (!line.0.is_empty()).then_some(line)
}

/// `sensors imu 1000/1000 Hz · camera 20/20 Hz`: per kind, the sensor furthest
/// below its expected rate (a sensor without an expected rate gets no verdict).
fn sensor_rates_line(sensors: &Value) -> Option<RowLine> {
    let list = sensors.get("sensors")?.as_array()?;
    // (kind, rate, expected) keeping the worst ratio per kind, in first-seen order.
    let mut kinds: Vec<(String, f64, Option<f64>)> = Vec::new();
    for s in list {
        let (Some(kind), Some(rate)) =
            (s.get("kind").and_then(Value::as_str), f64_at(s, "rate_hz"))
        else {
            continue;
        };
        let expected = f64_at(s, "expected_hz").filter(|e| *e > 0.0);
        let ratio = |r: f64, e: Option<f64>| e.map_or(f64::INFINITY, |e| r / e);
        match kinds.iter_mut().find(|k| k.0 == kind) {
            Some(k) if ratio(rate, expected) < ratio(k.1, k.2) => (k.1, k.2) = (rate, expected),
            Some(_) => {}
            None => kinds.push((kind.to_string(), rate, expected)),
        }
    }
    if kinds.is_empty() {
        return None;
    }
    let mut line = RowLine::default();
    line.push("sensors", Level::Label);
    for (i, (kind, rate, expected)) in kinds.into_iter().enumerate() {
        line.push(if i == 0 { " " } else { " · " }, Level::Label);
        line.push(format!("{kind} "), Level::Label);
        match expected {
            Some(e) => {
                let level = match rate / e {
                    r if r < 0.5 => Level::Error,
                    r if r < 0.9 => Level::Warn,
                    _ => Level::Value,
                };
                line.push(format!("{rate:.0}/{e:.0} Hz"), level);
            }
            None => line.push(format!("{rate:.0} Hz"), Level::Value),
        }
    }
    Some(line)
}
