//! Motion-lease host methods: `flight.lease.*`, `flight.goal.send`,
//! `flight.sticks.subscribe`.

use super::*;
use crate::pilot_client::lease_id_of;

fn unavailable(method: &str) -> HostResult {
    Value::Map(vec![
        (Value::from("ok"), Value::from(false)),
        (Value::from("err"), Value::from("pilot_unavailable")),
        (Value::from("method"), Value::from(method)),
    ])
}

fn bad_args(msg: &str) -> HostError {
    HostError::Rpc(format!("bad_args: {msg}"))
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_str())
}

impl RealHost {
    pub(super) async fn serve_flight_lease_acquire(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(pilot) = &self.pilot else {
            return Ok(unavailable("flight.lease.acquire"));
        };
        let skill_id = str_arg(args, "skill_id").ok_or_else(|| bad_args("skill_id is required"))?;
        let kind = str_arg(args, "kind").unwrap_or("motion");
        Ok(pilot.acquire(plugin_id, skill_id, kind).await)
    }

    pub(super) fn serve_flight_lease_release(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(pilot) = &self.pilot else {
            return Ok(unavailable("flight.lease.release"));
        };
        let id = lease_id_of(args).ok_or_else(|| bad_args("lease_id is required"))?;
        Ok(pilot.release(plugin_id, id))
    }

    pub(super) fn serve_flight_goal_send(
        &self,
        plugin_id: &str,
        args: &Value,
    ) -> Result<HostResult, HostError> {
        let Some(pilot) = &self.pilot else {
            return Ok(unavailable("flight.goal.send"));
        };
        let id = lease_id_of(args).ok_or_else(|| bad_args("lease_id is required"))?;
        let goal = args
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("goal")))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| bad_args("goal is required"))?;
        // Reject a malformed goal here, where the plugin sees the reason,
        // rather than at the pilot, which would drop it silently.
        let probe = ados_protocol::pilot::encode_op(
            "goal",
            &Value::Map(vec![
                (Value::from("lease_id"), Value::from(id)),
                (Value::from("goal"), goal.clone()),
            ]),
        )
        .map_err(|e| bad_args(&e.to_string()))?;
        ados_protocol::pilot::decode_request(&probe).map_err(|e| bad_args(&e.to_string()))?;
        Ok(pilot.goal(plugin_id, id, goal))
    }

    pub(super) fn serve_flight_sticks_subscribe(
        &self,
        plugin_id: &str,
    ) -> Result<HostResult, HostError> {
        let Some(pilot) = &self.pilot else {
            return Ok(unavailable("flight.sticks.subscribe"));
        };
        Ok(pilot.subscribe_sticks(plugin_id))
    }

    /// Release a stopped plugin's motion lease.
    pub(super) fn release_plugin_lease(&self, plugin_id: &str) {
        if let Some(pilot) = &self.pilot {
            pilot.release_plugin(plugin_id);
        }
    }

    /// The pilot, when it is up and can take a legacy setpoint stream.
    pub(super) fn reachable_pilot(&self) -> Option<&Arc<crate::pilot_client::PilotClient>> {
        self.pilot.as_ref().filter(|p| p.reachable())
    }
}
