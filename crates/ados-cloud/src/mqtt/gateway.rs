//! Telemetry + status gateway.
//!
//! Publishes the vehicle telemetry to `ados/{id}/telemetry` at q0 and a small
//! status document to `ados/{id}/status` at q1, on the gateway's cadence
//! (~2 Hz). Ports the publish surface of `src/ados/services/mqtt/gateway.py`:
//! the two topics, their QoS, the status-doc shape, and the bare-`device_id`
//! username (distinct from the relays' `ados-{id}` form).
//!
//! The per-tick publish is factored into [`MqttGateway::publish_tick`] so it is
//! unit-testable against a fake transport. The continuous timer loop that calls
//! it on a cadence is wired with the relay loops.

use serde::Serialize;

use super::transport::{MqttQos, MqttTransport, TransportError};
use super::{topic_status, topic_telemetry};

/// The small status document published to `ados/{id}/status`. Field names mirror
/// the Python gateway's status dict exactly (snake_case, as the Python source
/// emits them).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusDoc {
    pub device_id: String,
    pub name: String,
    pub tier: i64,
    pub armed: bool,
    pub fc_connected: bool,
}

/// The telemetry + status gateway. Holds the device id + the (already
/// authenticated) transport; one tick publishes the current telemetry and
/// status.
pub struct MqttGateway<T: MqttTransport> {
    device_id: String,
    transport: T,
    topic_telemetry: String,
    topic_status: String,
}

impl<T: MqttTransport> MqttGateway<T> {
    /// Build the gateway over a connected transport. The transport must already
    /// be authenticated as the bare `device_id` (see [`super::gateway_username`]).
    pub fn new(device_id: impl Into<String>, transport: T) -> Self {
        let device_id = device_id.into();
        MqttGateway {
            topic_telemetry: topic_telemetry(&device_id),
            topic_status: topic_status(&device_id),
            transport,
            device_id,
        }
    }

    /// Publish one telemetry payload (q0) followed by one status doc (q1).
    /// `telemetry` is the already-serialized vehicle-state JSON the caller folds
    /// from the live state; `status` is the small status document. Mirrors the
    /// two `client.publish(...)` calls in the Python gateway loop body.
    pub async fn publish_tick(
        &self,
        telemetry: &serde_json::Value,
        status: &StatusDoc,
    ) -> Result<(), TransportError> {
        let telemetry_bytes = serde_json::to_vec(telemetry)
            .map_err(|e| TransportError::Client(format!("telemetry encode: {e}")))?;
        self.transport
            .publish(&self.topic_telemetry, MqttQos::AtMostOnce, telemetry_bytes)
            .await?;

        let status_bytes = serde_json::to_vec(status)
            .map_err(|e| TransportError::Client(format!("status encode: {e}")))?;
        self.transport
            .publish(&self.topic_status, MqttQos::AtLeastOnce, status_bytes)
            .await?;
        Ok(())
    }

    /// The gateway's device id.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::transport::test_support::FakeTransport;

    fn status_doc() -> StatusDoc {
        StatusDoc {
            device_id: "dev1".to_string(),
            name: "test-drone".to_string(),
            tier: 1,
            armed: false,
            fc_connected: true,
        }
    }

    #[tokio::test]
    async fn tick_publishes_telemetry_q0_then_status_q1_on_the_exact_topics() {
        let fake = FakeTransport::default();
        let gw = MqttGateway::new("dev1", fake);
        let telemetry = serde_json::json!({"lat": 12.34, "lon": 56.78, "armed": false});
        gw.publish_tick(&telemetry, &status_doc()).await.unwrap();

        let pubs = gw.transport.publishes.lock().unwrap();
        assert_eq!(pubs.len(), 2);
        // First: telemetry at q0.
        assert_eq!(pubs[0].0, "ados/dev1/telemetry");
        assert_eq!(pubs[0].1, MqttQos::AtMostOnce);
        let t: serde_json::Value = serde_json::from_slice(&pubs[0].2).unwrap();
        assert_eq!(t, telemetry);
        // Second: status at q1, with the snake_case status-doc shape.
        assert_eq!(pubs[1].0, "ados/dev1/status");
        assert_eq!(pubs[1].1, MqttQos::AtLeastOnce);
        let s: serde_json::Value = serde_json::from_slice(&pubs[1].2).unwrap();
        assert_eq!(s["device_id"], "dev1");
        assert_eq!(s["fc_connected"], true);
        assert_eq!(s["tier"], 1);
    }
}
