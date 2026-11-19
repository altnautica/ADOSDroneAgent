//! The `pilot` kind's payload as a [`Pilot`]: its banner, mission stall,
//! route check and every reading the Vehicle tab shows.

use serde_json::Value;

use super::{
    f, one_of, s, Banner, FcStatusText, MissionStall, Pilot, PilotMission, PositionUnverified,
    ReadinessItem, LAUNCH_STEPS, MISSION_STATES,
};
use crate::model::flight_state::PILOT_OWNERS;
use crate::model::route_check::{parse_place_class, parse_route_check};
use crate::model::sensor_health::SensorHealth;

pub(super) fn pilot(d: &Value, age_ms: Option<u64>, stale: bool) -> Pilot {
    let readiness = d
        .get("readiness")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some(ReadinessItem {
                id: r.get("id")?.as_str()?.to_string(),
                ok: r.get("ok").and_then(Value::as_bool) == Some(true),
                block: r.get("severity").and_then(Value::as_str) != Some("warn"),
                msg: r
                    .get("msg")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();
    let statustext = d
        .get("statustext")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(8)
        .filter_map(|s| {
            Some(FcStatusText {
                severity: f(s, "severity")?,
                text: s.get("text")?.as_str()?.to_string(),
            })
        })
        .collect();
    let goal_ned = d
        .get("goal_ned")
        .and_then(Value::as_array)
        .filter(|a| a.len() >= 3)
        .and_then(|a| {
            let n = |i: usize| a[i].as_f64().filter(|x| x.is_finite());
            Some([n(0)?, n(1)?, n(2)?])
        });
    Pilot {
        owner: one_of(d, "owner", &PILOT_OWNERS),
        skill_label: d.get("skill").and_then(|k| {
            // A skill names its plugin and skill id; the label defaults to the id.
            k.get("plugin_id")?.as_str()?;
            let id = k.get("skill_id")?.as_str()?;
            Some(
                k.get("label")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
            )
        }),
        skill_ref: d.get("skill").and_then(|k| {
            Some((
                k.get("plugin_id")?.as_str()?.to_string(),
                k.get("skill_id")?.as_str()?.to_string(),
            ))
        }),
        control_mode: one_of(d, "control_mode", &["ados", "fc_manual", "elrs"]),
        profile: one_of(d, "profile", &["cine", "normal", "sport"]),
        sim_clock: d.get("clock").and_then(sim_clock_label),
        avoid: one_of(d, "avoid", &["brake", "bypass", "off"]),
        avoid_state: one_of(
            d,
            "avoid_state",
            &["clear", "detour", "blocked", "off", "stale", "unavailable"],
        ),
        hold_reason: s(d, "hold_reason"),
        min_clearance_m: f(d, "min_clearance_m"),
        fc_mode: f(d, "fc_mode"),
        fc_mode_name: d
            .get("fc_mode_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        vehicle_class: one_of(d, "vehicle_class", &["multirotor", "fixedwing", "vtol"]),
        vtol_transitioning: d.get("vtol_transitioning").and_then(Value::as_bool) == Some(true),
        goal_ned,
        readiness,
        link_loss_in_s: f(d, "link_loss_in_s"),
        link_loss_action: one_of(
            d,
            "link_loss_action",
            &["rth", "rtl", "hold", "loiter", "land"],
        ),
        link_loss_s: f(d, "link_loss_s"),
        mission: d
            .get("mission")
            .filter(|m| m.is_object())
            .map(|m| PilotMission {
                state: one_of(m, "state", &MISSION_STATES),
                seq: f(m, "seq").or_else(|| f(m, "current_seq")),
                total: f(m, "total"),
                landing_seq: m.get("landing_seq").and_then(Value::as_bool) == Some(true),
            }),
        banner: d.get("banner").and_then(banner),
        rth_countdown_s: f(d, "rth_countdown_s"),
        mission_stall: d.get("mission_stall").and_then(mission_stall),
        agl_m: f(d, "agl_m"),
        agl_source: one_of(d, "agl_source", &["rangefinder", "home"]),
        sensors: d.get("sensors").and_then(SensorHealth::from_value),
        statustext,
        setup_step: d
            .get("setup_step")
            .and_then(Value::as_str)
            .map(str::to_string),
        launch_step: running_launch(d).map(|q| q["step"].as_str().unwrap_or_default().to_string()),
        liftoff_text: running_launch(d)
            .and_then(|q| q.get("liftoff"))
            .and_then(|l| {
                let text = l.get("text")?.as_str().filter(|t| !t.is_empty())?;
                Some(match l.get("detail").and_then(Value::as_str) {
                    Some(detail) if !detail.is_empty() => format!("{text} · {detail}"),
                    _ => text.to_string(),
                })
            }),
        fc_failsafe: d.get("fc_failsafe").and_then(Value::as_bool) == Some(true),
        failsafe_cause: one_of(d, "failsafe_cause", &["fc", "rc_or_failsafe"]),
        setup_abort: d
            .get("setup_abort")
            .and_then(Value::as_str)
            .filter(|k| !k.is_empty())
            .map(str::to_string),
        low_battery_action: f(d, "rth_countdown_s")
            .is_none()
            .then(|| one_of(d, "low_battery_action", &["land", "rth"]))
            .flatten(),
        low_battery_countdown_land: f(d, "rth_countdown_s").is_some()
            && d.get("low_battery_action").and_then(Value::as_str) == Some("land"),
        energy_need_pct: d
            .get("battery_reserve")
            .filter(|r| r.get("basis").and_then(Value::as_str) == Some("energy"))
            .and_then(|r| r.get("need_pct"))
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        speed_cap_mps: d
            .get("speed_cap")
            .filter(|c| c.get("reason").and_then(Value::as_str).is_some())
            .and_then(|c| c.get("v_h_mps"))
            .and_then(Value::as_f64),
        position_unverified: d.get("position_unverified").and_then(|p| {
            Some(PositionUnverified {
                reason: p.get("reason")?.as_str()?.to_string(),
                detail: s(p, "detail"),
            })
        }),
        speed_cap_reason: d
            .get("speed_cap")
            .filter(|c| c.get("v_h_mps").and_then(Value::as_f64).is_some())
            .and_then(|c| c.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        place_class: parse_place_class(d),
        rc_absent: d.get("rc_receiver").and_then(Value::as_str) == Some("absent"),
        route_check: d.get("route_check").and_then(parse_route_check),
        surface_ops: d
            .get("surface_ops")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        armed: d.get("armed").and_then(Value::as_bool),
        landed: d.get("landed").and_then(Value::as_bool),
        input_label: d
            .get("input_session")
            .filter(|s| s.get("src").and_then(Value::as_str).is_some())
            .and_then(|s| {
                s.get("label")
                    .and_then(Value::as_str)
                    .or(s.get("src")?.as_str())
            })
            .or_else(|| d.get("input_src").and_then(Value::as_str))
            .filter(|l| !l.is_empty())
            .map(str::to_string),
        profile_limits: d
            .get("profile_limits")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        stick: d
            .get("stick")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        command_limits: d
            .get("command")
            .and_then(|c| c.get("limits"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        age_ms,
        stale,
    }
}

/// The `sequence` of a running launch: a known step other than `done`, with
/// no error.
fn running_launch(d: &Value) -> Option<&Value> {
    let q = d.get("sequence").filter(|q| q.is_object())?;
    let step = q.get("step")?.as_str()?;
    let running = LAUNCH_STEPS.contains(&step)
        && step != "done"
        && q.get("error").and_then(Value::as_str).is_none();
    running.then_some(q)
}

/// A banner with a text key; each arg as text (a JSON string as itself,
/// anything else in its JSON form, so `45` reads `45`).
fn banner(b: &Value) -> Option<Banner> {
    let text_key = b.get("text_key")?.as_str()?.to_string();
    let args = b
        .get("args")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, v)| !v.is_null())
        .map(|(k, v)| {
            let text = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            (k.clone(), text)
        })
        .collect();
    Some(Banner {
        text_key,
        args,
        until_ms: b.get("until_ms").and_then(Value::as_u64),
    })
}

/// A stall of a known kind, else `None`.
fn mission_stall(v: &Value) -> Option<MissionStall> {
    Some(MissionStall {
        kind: one_of(v, "kind", &["mission", "return"])?,
        seq: f(v, "seq"),
        wp_label: s(v, "wp_label"),
        since_ms: v.get("since_ms").and_then(Value::as_u64),
        skip_in_s: f(v, "skip_in_s"),
        return_in_s: f(v, "return_in_s"),
        window_s: f(v, "window_s"),
    })
}

/// The pilot frame's `clock` as the "SIM ×N" badge (as the web surfaces
/// write it): `None` on wall time. Speed 0 is a simulator running as fast as
/// it can.
pub(crate) fn sim_clock_label(c: &Value) -> Option<String> {
    if c.get("source").and_then(Value::as_str) != Some("sim") {
        return None;
    }
    Some(match c.get("speed").and_then(Value::as_f64) {
        None => "SIM".into(),
        Some(s) if s <= 0.0 => "SIM ×max".into(),
        Some(s) => {
            let n = (s * 10.0).round() / 10.0;
            if n.fract() == 0.0 {
                format!("SIM ×{n:.0}")
            } else {
                format!("SIM ×{n:.1}")
            }
        }
    })
}
