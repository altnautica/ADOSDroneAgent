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
        let states = services
            .iter()
            .filter_map(|svc| svc.get("state").and_then(Value::as_str))
            .filter(|state| !matches!(*state, "inactive" | "idle"));
        let (mut total, mut running) = (0, 0);
        for state in states {
            total += 1;
            running += usize::from(state == "active");
        }
        self.services_total = total;
        self.services_running = running;
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
    fn a_failed_or_restarting_unit_degrades_the_verdict() {
        for (state, sub) in [("failed", "failed"), ("activating", "auto-restart")] {
            let mut dash = Dashboard::from_status(&setup_status());
            dash.merge_services(&json!({"services": [
                row("ados-control", "active", "running"),
                row("ados-video", state, sub),
            ]}));
            assert_eq!((dash.services_running, dash.services_total), (1, 2));
            assert_eq!(dash.health(), Health::Degraded, "{state}");
        }
    }

    #[test]
    fn a_body_without_services_leaves_the_counts_alone() {
        let mut dash = Dashboard::from_status(&setup_status());
        dash.merge_services(&json!({"version": "0.103.0"}));
        assert_eq!((dash.services_running, dash.services_total), (0, 0));
    }
}
