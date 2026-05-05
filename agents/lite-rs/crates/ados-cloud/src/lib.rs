// Cloud relay client for the lite agent.
//
// Speaks the same MQTT topic schema and HTTP heartbeat shape as the Python
// full agent under src/ados/services/cloud/. Topics are device-scoped under
// ados/{device_id}/* with stable QoS levels. The HTTPS heartbeat goes to
// the Convex relay endpoint; the pairing beacon POSTs the pairing code on
// a 30 s cadence when unpaired.

#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("MQTT client error: {0}")]
    Mqtt(String),

    #[error("HTTPS request failed: {0}")]
    Http(String),

    #[error("TLS configuration error: {0}")]
    Tls(String),

    #[error("Pairing not yet established")]
    Unpaired,
}

/// Placeholder cloud client entry point. Filled in during the next phase.
///
/// At v0.1 this returns immediately with `Ok(())`. The real implementation
/// connects to the configured MQTT broker over TLS via rumqttc, publishes
/// state at 2 Hz, polls the inbound command topic at 5 s, and runs the
/// HTTPS heartbeat task in parallel.
pub async fn run_cloud_client() -> Result<(), CloudError> {
    tracing::info!("cloud client placeholder — full implementation pending");
    Ok(())
}
