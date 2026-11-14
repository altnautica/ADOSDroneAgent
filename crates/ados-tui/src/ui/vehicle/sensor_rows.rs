//! The Autonomy block's sensor rows, worded by the cockpit spec's
//! `sensor-health.json`: the flight controller's optical flow and downward
//! range (from the pilot's `sensors`), the lidar (its scan rate and driver
//! health from `sensors.lidar`, the nearest return from the scan or the nav
//! kind, and the rate vision-nav sends obstacle distances at, labelled as
//! such), and each camera's state from `/api/v1/sensors` `health.cameras`.

use ratatui::text::{Line, Span};

use super::autonomy::{mark_stale, tone_span, LABEL};
use crate::app::Core;
use crate::model::autonomy::Autonomy;
use crate::model::sensor_health::SensorWord;

/// A `label` row of words, each in its tone, joined by ` · `, then `tail`.
fn word_line(core: &Core, label: &str, words: Vec<SensorWord>, tail: Vec<String>) -> Line<'static> {
    let t = &core.theme;
    let mut spans = vec![Span::styled(format!("{label:<LABEL$} "), t.dim())];
    for w in words {
        if spans.len() > 1 {
            spans.push(Span::styled(" · ", t.dim()));
        }
        spans.push(tone_span(core, w.tone, w.text));
    }
    for part in tail {
        let text = if spans.len() > 1 {
            format!(" · {part}")
        } else {
            part
        };
        spans.push(Span::styled(text, t.bright()));
    }
    Line::from(spans)
}

pub(super) fn rows(core: &Core, a: &Autonomy) -> Vec<Line<'static>> {
    let t = &core.theme;
    let mut out = Vec::new();
    let sensors = a
        .pilot
        .as_ref()
        .and_then(|p| p.sensors.as_ref().map(|s| (s, p.stale)));
    if let Some((s, stale)) = sensors {
        let words = s.row_words();
        if !words.is_empty() {
            out.push(mark_stale(
                t,
                word_line(core, "Sensors", words, Vec::new()),
                stale,
            ));
        }
    }

    // The nearest return: the scan's, else the nav kind's nearest obstacle;
    // each value carries the staleness of the kind it came from.
    let from_scan = a
        .lidar
        .as_ref()
        .and_then(|l| l.min_range_m.map(|m| (m, l.stale)));
    let from_nav = a
        .nav
        .as_ref()
        .and_then(|n| n.min_obstacle_m.map(|m| (m, n.stale)));
    if a.lidar.is_some() || from_nav.is_some() || sensors.is_some() {
        let nearest = from_scan.or(from_nav);
        let obstacle_rate = a
            .nav
            .as_ref()
            .and_then(|n| n.obstacle_rate_hz.map(|r| (r, n.stale)));
        let stale = nearest.map_or_else(|| a.lidar.as_ref().is_some_and(|l| l.stale), |(_, s)| s)
            || obstacle_rate.is_some_and(|(_, s)| s)
            || sensors.is_some_and(|(_, s)| s);
        let mut words = Vec::new();
        let mut tail = Vec::new();
        if let Some((s, _)) = sensors {
            words.push(s.lidar_word());
            if let Some(h) = s.lidar.health.as_deref().filter(|h| *h != "good") {
                tail.push(h.to_string());
            }
        }
        if a.lidar.is_some() || from_nav.is_some() {
            tail.push(format!(
                "min {}",
                nearest.map_or_else(|| "–".to_string(), |(m, _)| format!("{m:.1} m"))
            ));
        }
        if let Some((r, _)) = obstacle_rate {
            tail.push(format!("obstacles {r:.0} Hz"));
        }
        if words.is_empty() {
            out.push(mark_stale(t, word_line(core, "Lidar", words, tail), stale));
        } else {
            // The stale mark goes right after the state word: the tail can
            // run past a narrow column, and a clipped mark reads live.
            let mut line = mark_stale(t, word_line(core, "Lidar", words, Vec::new()), stale);
            let style = if stale { t.dim() } else { t.bright() };
            line.spans.extend(
                tail.into_iter()
                    .map(|p| Span::styled(format!(" · {p}"), style)),
            );
            out.push(line);
        }
    }

    let cameras = core
        .view
        .dash
        .as_ref()
        .map(|d| d.camera_health.iter().map(|c| c.word()).collect::<Vec<_>>())
        .unwrap_or_default();
    if !cameras.is_empty() {
        out.push(word_line(core, "Cameras", cameras, Vec::new()));
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::super::tests::{land, live_app};
    use crate::model::sensor_health::CameraHealth;
    use crate::poll::ResourceId;
    use crate::ui::tests::draw;

    fn nav_with_sensors(sensors: serde_json::Value) -> serde_json::Value {
        json!({
            "pilot": {"ts_ms": 1, "age_ms": 100, "data": {"owner": "idle", "sensors": sensors}},
            "nav": {"ts_ms": 1, "age_ms": 30, "data": {
                "extnav_health": "good", "obstacle_rate_hz": 10.0, "min_obstacle_m": 3.0}},
            "scan": {"ts_ms": 1, "age_ms": 50, "data": {"ranges_m": [0.0, 2.5]}},
        })
    }

    #[test]
    fn flow_range_and_the_lidar_scan_rate_read_as_the_spec_words() {
        let mut app = live_app();
        let sensors = json!({
            "flow": {"state": "low", "quality": 40, "rate_hz": 19.8, "age_ms": 50},
            "range": {"state": "ok", "height_m": 3.42, "age_ms": 30},
            "lidar": {"state": "degraded", "rate_hz": 6.04, "health": "warning", "age_ms": 120},
        });
        land(&mut app, ResourceId::NavStatus, nav_with_sensors(sensors));
        let text = draw(&mut app, 120, 40);
        for want in [
            "Flow low 40 · ",
            "Range 3.4 m",
            "Lidar 6.0 Hz · warning · min 2.5 m",
        ] {
            assert!(text.contains(want), "missing {want:?}\n{text}");
        }
    }

    #[test]
    fn absent_flow_and_range_are_left_out_and_no_lidar_is_said() {
        let mut app = live_app();
        let sensors = json!({
            "flow": {"state": "absent"}, "range": {"state": "absent"}, "lidar": {"state": "absent"},
        });
        land(&mut app, ResourceId::NavStatus, nav_with_sensors(sensors));
        let text = draw(&mut app, 120, 40);
        // The `Sensors & clock` panel's title also says Sensors: look for the row.
        assert!(!text.contains("Sensors   "), "{text}");
        assert!(!text.contains("No flow"), "{text}");
        assert!(
            text.contains("No lidar · min 2.5 m · obstacles 10 Hz"),
            "{text}"
        );
    }

    #[test]
    fn each_camera_reads_its_state() {
        let mut app = live_app();
        if let Some(d) = app.core.view.dash.as_mut() {
            d.camera_health = CameraHealth::list(&json!({"health": {"cameras": [
                {"id": "front", "state": "live", "fps": 29.7},
                {"id": "belly", "state": "lost"}
            ]}}));
        }
        land(
            &mut app,
            ResourceId::NavStatus,
            nav_with_sensors(serde_json::Value::Null),
        );
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("front 30 fps · "), "{text}");
        assert!(text.contains("belly lost"), "{text}");
    }
}
