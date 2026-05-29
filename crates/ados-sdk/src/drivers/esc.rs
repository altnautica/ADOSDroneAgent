//! ESC telemetry driver trait.
//!
//! Ports `ados.sdk.drivers.esc`. An ESC driver reads per-motor telemetry from
//! electronic speed controllers. DShot-telemetry, KISS, and BLHeli32 share this
//! interface. ESC drivers are read-only by design: setpoints flow through the
//! FC, not the agent.

use std::collections::BTreeMap;

use async_trait::async_trait;
use rmpv::Value;

use super::{Bus, DriverResult, SampleStream};

/// An ESC bank a driver claims it can open.
#[derive(Debug, Clone, PartialEq)]
pub struct EscCandidate {
    pub driver_id: String,
    pub device_id: String,
    pub label: String,
    pub bus: Bus,
    pub motor_count: u8,
    pub vid_pid: Option<(u16, u16)>,
    pub metadata: BTreeMap<String, Value>,
}

/// Static capabilities of an open ESC session.
#[derive(Debug, Clone, PartialEq)]
pub struct EscCapabilities {
    pub protocol: String,
    pub motor_count: u8,
    pub has_rpm: bool,
    pub has_temperature: bool,
    pub has_voltage: bool,
    pub has_current: bool,
    pub update_hz: f32,
}

/// One telemetry sample for one motor. `throttle_pct` is the most recently
/// commanded throttle from the FC (0..100). `rpm` is mechanical RPM, corrected
/// for pole count when the driver knows it.
#[derive(Debug, Clone, PartialEq)]
pub struct EscTelemetry {
    pub timestamp_ns: i64,
    pub motor_index: u8,
    pub rpm: f32,
    pub temp_c: f32,
    pub voltage_v: f32,
    pub current_a: f32,
    pub throttle_pct: f32,
    pub metadata: BTreeMap<String, Value>,
}

/// Abstract ESC telemetry driver.
#[async_trait]
pub trait EscDriver: Send + Sync {
    type Session: Send + Sync;

    /// Scan for ESC banks this driver can open.
    async fn discover(&self) -> DriverResult<Vec<EscCandidate>>;

    /// Open a session against an ESC bank.
    async fn open(
        &self,
        candidate: &EscCandidate,
        config: &BTreeMap<String, Value>,
    ) -> DriverResult<Self::Session>;

    /// Release resources held by a session.
    async fn close(&self, session: Self::Session) -> DriverResult<()>;

    /// Return the static capabilities of an open session.
    fn capabilities(&self, session: &Self::Session) -> EscCapabilities;

    /// Yield per-motor telemetry samples until the session closes.
    async fn telemetry_iterator(
        &self,
        session: &Self::Session,
    ) -> DriverResult<SampleStream<EscTelemetry>>;
}
