//! The flight-state chip's detail for the skill that owns the aircraft
//! (`ORBIT 8 m`), the port of the shared `web-shared/src/hud/skill-detail.ts`:
//! the skill's state topic from its plugin's GCS manifest
//! (`GET /api/plugins/{id}`), then the parameters the plugin echoes on that
//! topic (`GET /api/plugins/{id}/state`). Held to the shared golden cases in
//! `web-shared/src/hud/fixtures/skill-detail-cases.json`.

use serde_json::Value;

/// The chip's mode detail from a running skill's echoed parameters:
/// follow-me's `ORBIT 8 m` / `FOLLOW 6 m`; `None` when nothing is known.
pub fn skill_detail(echo: &Value) -> Option<String> {
    let echo = echo.as_object()?;
    let metres = |key: &str| {
        echo.get(key)
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite())
            .map(|v| format!(" {} m", round1(v)))
            .unwrap_or_default()
    };
    match echo.get("follow_mode").and_then(Value::as_str)? {
        "orbit" => Some(format!("ORBIT{}", metres("orbit_radius_m"))),
        "follow" => Some(format!("FOLLOW{}", metres("follow_distance_m"))),
        _ => None,
    }
}

/// One decimal the way the shared helper prints it (`Math.round(v * 10) / 10`):
/// half rounds up, and a whole number has no decimal.
fn round1(v: f64) -> String {
    let r = (v * 10.0 + 0.5).floor() / 10.0;
    if r.fract() == 0.0 {
        format!("{}", r as i64)
    } else {
        format!("{r:.1}")
    }
}

/// The state topic a plugin's manifest declares for one skill.
pub fn skill_state_topic(detail: &Value, skill_id: &str) -> Option<String> {
    detail
        .get("manifest")?
        .get("gcs")?
        .get("contributes")?
        .get("skills")?
        .as_array()?
        .iter()
        .find(|s| s.get("id").and_then(Value::as_str) == Some(skill_id))?
        .get("state")?
        .get("topic")?
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// The chip detail from a plugin state body (`{ topic: { payload } }`).
pub fn skill_detail_from_state(body: &Value, topic: &str) -> Option<String> {
    let payload = body.get(topic)?.get("payload")?;
    skill_detail(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CASES: &str =
        include_str!("../../../../web-shared/src/hud/fixtures/skill-detail-cases.json");

    #[test]
    fn skill_detail_cases() {
        let cases: Vec<Value> = serde_json::from_str(CASES).expect("cases are JSON");
        assert!(!cases.is_empty());
        for c in &cases {
            let want = c["expected"].as_str().map(str::to_string);
            assert_eq!(skill_detail(&c["input"]["echo"]), want, "{}", c["name"]);
        }
    }

    #[test]
    fn reads_the_topic_from_the_manifest_then_the_echo() {
        let detail = json!({"manifest": {"gcs": {"contributes": {"skills": [
            {"id": "follow", "state": {"topic": "follow.state"}}]}}}});
        let topic = skill_state_topic(&detail, "follow").expect("topic");
        assert_eq!(topic, "follow.state");
        let body =
            json!({"follow.state": {"payload": {"follow_mode": "orbit", "orbit_radius_m": 8}}});
        assert_eq!(
            skill_detail_from_state(&body, &topic).as_deref(),
            Some("ORBIT 8 m")
        );
        assert_eq!(skill_state_topic(&detail, "orbit"), None);
        assert_eq!(skill_detail_from_state(&json!({}), "follow.state"), None);
    }
}
