//! Counting agent services from the consolidated status document.

use super::*;

impl Dashboard {
    /// Count the agent's units from a `/api/status/full` body. Its `services`
    /// rows carry the systemd ActiveState as `state`; the setup status the rest
    /// of the dashboard is built from has no service list at all. Best-effort:
    /// called only when the route answered this poll.
    ///
    /// `active` is running. An `inactive` unit is off by choice (a disabled
    /// optional lane, a unit of the other profile) and an `idle` one was skipped
    /// on an unmet unit condition, so neither counts. Anything else (`failed`,
    /// `activating` in a restart loop) is a unit that should be up and is not.
    pub fn merge_services(&mut self, status_full: &Value) {
        let Some(services) = status_full.get("services").and_then(Value::as_array) else {
            return;
        };
        let units = services.iter().filter_map(|svc| {
            let state = svc.get("state").and_then(Value::as_str)?;
            let sub = svc.get("sub_state").and_then(Value::as_str).unwrap_or("");
            let name = svc.get("name").and_then(Value::as_str).unwrap_or("");
            (!matches!(state, "inactive" | "idle")).then_some((name, state, sub))
        });
        let (mut total, mut running) = (0, 0);
        let mut first_down = None;
        for (name, state, sub) in units {
            total += 1;
            running += usize::from(state == "active");
            if state != "active" && first_down.is_none() && !name.is_empty() {
                let name = name.trim_end_matches(".service");
                first_down = Some(format!("{name} {}", down_word(state, sub)));
            }
        }
        self.services_total = total;
        self.services_running = running;
        self.first_down = first_down;
    }

    /// What makes the verdict DEGRADED, in words, for the line under it:
    /// the first unit down, else a lost flight controller.
    pub fn health_cause(&self) -> Option<String> {
        if self.health() != Health::Degraded {
            return None;
        }
        Some(match &self.first_down {
            Some(cause) => cause.clone(),
            None if self.services_running < self.services_total => format!(
                "{} of {} services down",
                self.services_total - self.services_running,
                self.services_total
            ),
            None => "flight controller lost".to_string(),
        })
    }
}

/// A unit that should be up and is not, by its systemd ActiveState (and, for
/// `activating`, its SubState): only `failed` reads as failed.
fn down_word<'a>(state: &'a str, sub: &str) -> &'a str {
    match state {
        "activating" if sub == "auto-restart" => "restarting",
        "activating" => "starting",
        "deactivating" => "stopping",
        "maintenance" => "in maintenance",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A set-up drone as `/api/v1/setup/status` reports it: the body carries no
    /// `services` key.
    fn setup_status() -> Value {
        json!({
            "version": "0.103.0",
            "device_id": "9f2c1a",
            "device_name": "ados-9f2c1a",
            "profile": "drone",
            "setup_complete": true,
            "completion_percent": 100,
            "next_action": "",
            "steps": [{"label": "Profile", "state": "complete"}],
            "access_urls": [],
            "network": {"hotspot_enabled": false},
            "mavlink": {"connected": true},
            "video": {"state": "running"},
            "remote_access": {"status": "disabled"},
            "paired": true
        })
    }

    fn row(name: &str, state: &str, sub: &str) -> Value {
        json!({"name": name, "state": state, "sub_state": sub, "task_done": sub != "running"})
    }

    #[test]
    fn a_set_up_node_with_its_services_up_is_healthy() {
        let mut dash = Dashboard::from_status(&setup_status());
        assert_eq!(dash.services_total, 0, "setup status has no service list");
        dash.merge_services(&json!({"services": [
            row("ados-control", "active", "running"),
            row("ados-mavlink", "active", "running"),
            row("ados-tunnel-config", "active", "exited"),
            row("ados-logd", "inactive", "dead"),
            {"name": "ados-vision-hailo", "state": "idle", "sub_state": "dead",
             "reason": "condition_unmet"},
        ]}));
        assert_eq!((dash.services_running, dash.services_total), (3, 3));
        assert_eq!(dash.health(), Health::Healthy);
    }

    #[test]
    fn a_unit_that_is_down_degrades_the_verdict_and_is_worded_by_its_state() {
        for (state, sub, cause) in [
            ("failed", "failed", "ados-video failed"),
            ("activating", "auto-restart", "ados-video restarting"),
            ("activating", "start", "ados-video starting"),
            ("deactivating", "stop-sigterm", "ados-video stopping"),
            ("reloading", "reload", "ados-video reloading"),
            ("maintenance", "dead", "ados-video in maintenance"),
            ("refreshing", "running", "ados-video refreshing"),
        ] {
            let mut dash = Dashboard::from_status(&setup_status());
            dash.merge_services(&json!({"services": [
                row("ados-control", "active", "running"),
                row("ados-video", state, sub),
            ]}));
            assert_eq!((dash.services_running, dash.services_total), (1, 2));
            assert_eq!(dash.health(), Health::Degraded, "{state}");
            assert_eq!(dash.health_cause().as_deref(), Some(cause), "{state}");
        }
    }

    #[test]
    fn a_lost_flight_controller_is_named_as_the_cause() {
        let mut status = setup_status();
        status["mavlink"] = json!({"connected": false});
        let mut dash = Dashboard::from_status(&status);
        dash.merge_services(&json!({"services": [row("ados-control", "active", "running")]}));
        assert_eq!(dash.health(), Health::Degraded);
        assert_eq!(
            dash.health_cause().as_deref(),
            Some("flight controller lost")
        );

        let healthy = Dashboard::from_status(&setup_status());
        assert_eq!(healthy.health_cause(), None);
    }

    #[test]
    fn a_body_without_services_leaves_the_counts_alone() {
        let mut dash = Dashboard::from_status(&setup_status());
        dash.merge_services(&json!({"version": "0.103.0"}));
        assert_eq!((dash.services_running, dash.services_total), (0, 0));
    }
}
