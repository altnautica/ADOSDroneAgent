//! Payload actuator driver trait.
//!
//! Ports `ados.sdk.drivers.payload_actuator`. A payload actuator driver
//! triggers a discrete or continuous mechanism attached to the airframe:
//! sprayer pump, dropper servo, claw, sampler, parachute, beacon. Actions are
//! addressed by id with a free-form argument bag so vendor payloads can model
//! their own command surface.

use std::collections::BTreeMap;

use async_trait::async_trait;
use rmpv::Value;

use super::{Bus, DriverResult};

/// A payload actuator a driver claims it can open.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadCandidate {
    pub driver_id: String,
    pub device_id: String,
    pub label: String,
    pub bus: Bus,
    pub vid_pid: Option<(u16, u16)>,
    pub metadata: BTreeMap<String, Value>,
}

/// Static capabilities of an open payload session. `actions` is the menu of
/// action ids the driver accepts; the host uses it to populate the GCS panel
/// and to validate [`PayloadActuatorDriver::actuate`] before it reaches the
/// driver subprocess.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadCapabilities {
    pub actions: Vec<String>,
    pub has_position_feedback: bool,
    pub has_flow_feedback: bool,
    pub metadata: BTreeMap<String, Value>,
}

/// A single actuation request. `action_id` is one of the ids declared in
/// [`PayloadCapabilities`]; `args` carries action-specific parameters
/// (duration, volume, angle).
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadCommand {
    pub action_id: String,
    pub args: BTreeMap<String, Value>,
}

/// A snapshot of payload state after actuation.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadState {
    pub timestamp_ns: i64,
    pub last_action_id: Option<String>,
    pub busy: bool,
    pub metadata: BTreeMap<String, Value>,
}

/// Abstract payload actuator driver.
#[async_trait]
pub trait PayloadActuatorDriver: Send + Sync {
    type Session: Send + Sync;

    /// Scan for payload actuators this driver can open.
    async fn discover(&self) -> DriverResult<Vec<PayloadCandidate>>;

    /// Open a session against a payload actuator.
    async fn open(
        &self,
        candidate: &PayloadCandidate,
        config: &BTreeMap<String, Value>,
    ) -> DriverResult<Self::Session>;

    /// Release resources held by a session.
    async fn close(&self, session: Self::Session) -> DriverResult<()>;

    /// Return the static capabilities of an open session.
    fn capabilities(&self, session: &Self::Session) -> PayloadCapabilities;

    /// Execute one payload action. Return
    /// [`super::DriverError::InvalidParam`] if `command.action_id` is not in
    /// [`PayloadCapabilities::actions`] so the host can reject the request
    /// before it reaches hardware.
    async fn actuate(&self, session: &Self::Session, command: &PayloadCommand) -> DriverResult<()>;

    /// Return the most recent payload state snapshot.
    fn get_state(&self, session: &Self::Session) -> PayloadState;
}
