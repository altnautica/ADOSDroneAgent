// MAVLink router for the lite agent.
//
// Owns the flight controller serial connection. Parses incoming v2 frames,
// publishes them on a tokio::sync::broadcast channel for in-process
// consumers, and exposes TCP / UDP / WebSocket listeners for direct GCS
// connections on the LAN. Outbound frames from cloud relay or local GCS
// clients route back to the FC.

#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MavlinkError {
    #[error("FC serial open failed: {0}")]
    SerialOpen(String),

    #[error("MAVLink frame parse failed: {0}")]
    Parse(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Placeholder router entry point. Filled in during the next phase.
///
/// At v0.1 this returns immediately with `Ok(())`. The real implementation
/// opens the FC serial port from the agent config, spins parser tasks, and
/// broadcasts parsed frames to subscribers.
pub async fn run_router() -> Result<(), MavlinkError> {
    tracing::info!("mavlink router placeholder — full implementation pending");
    Ok(())
}
