//! Gimbal driver trait.
//!
//! Ports `ados.sdk.drivers.gimbal`. A gimbal driver moves a stabilised mount to
//! a commanded attitude or rate and reports the pointing state. Vendor serial
//! mounts, SBGC-family controllers, and MAVLink mount protocols share this
//! interface.

use std::collections::BTreeMap;

use async_trait::async_trait;
use rmpv::Value;

use super::{Bus, DriverResult, SampleStream};

/// A gimbal a driver claims it can open.
#[derive(Debug, Clone, PartialEq)]
pub struct GimbalCandidate {
    pub driver_id: String,
    pub device_id: String,
    pub label: String,
    pub bus: Bus,
    pub vid_pid: Option<(u16, u16)>,
    pub metadata: BTreeMap<String, Value>,
}

/// Static capabilities of an open gimbal session. Limits are degrees from
/// neutral. `max_rate_dps == None` on an axis means it is position-controlled
/// only.
#[derive(Debug, Clone, PartialEq)]
pub struct GimbalCapabilities {
    pub has_pitch: bool,
    pub has_yaw: bool,
    pub has_roll: bool,
    pub pitch_min_deg: f32,
    pub pitch_max_deg: f32,
    pub yaw_min_deg: f32,
    pub yaw_max_deg: f32,
    pub roll_min_deg: f32,
    pub roll_max_deg: f32,
    pub max_rate_dps: Option<f32>,
    pub supports_follow_mode: bool,
    pub supports_lock_mode: bool,
}

/// A single state sample reported by a gimbal.
#[derive(Debug, Clone, PartialEq)]
pub struct GimbalState {
    pub timestamp_ns: i64,
    pub pitch_deg: f32,
    pub yaw_deg: f32,
    pub roll_deg: f32,
    pub pitch_rate_dps: f32,
    pub yaw_rate_dps: f32,
    pub roll_rate_dps: f32,
    pub mode: String,
    pub metadata: BTreeMap<String, Value>,
}

/// Abstract gimbal driver.
#[async_trait]
pub trait GimbalDriver: Send + Sync {
    type Session: Send + Sync;

    /// Scan for gimbals this driver can open.
    async fn discover(&self) -> DriverResult<Vec<GimbalCandidate>>;

    /// Open a session against a gimbal.
    async fn open(
        &self,
        candidate: &GimbalCandidate,
        config: &BTreeMap<String, Value>,
    ) -> DriverResult<Self::Session>;

    /// Release resources held by a session.
    async fn close(&self, session: Self::Session) -> DriverResult<()>;

    /// Return the static capabilities of an open session.
    fn capabilities(&self, session: &Self::Session) -> GimbalCapabilities;

    /// Drive the gimbal to an absolute pitch/yaw/roll setpoint. Out-of-range
    /// setpoints should be clamped against [`GimbalCapabilities`] rather than
    /// silently ignored.
    async fn command_attitude(
        &self,
        session: &Self::Session,
        pitch_deg: f32,
        yaw_deg: f32,
        roll_deg: f32,
    ) -> DriverResult<()>;

    /// Drive the gimbal at a commanded angular rate. Drivers without rate
    /// control return [`super::DriverError::NotSupported`] so the host falls
    /// back to position commands.
    async fn command_rate(
        &self,
        session: &Self::Session,
        pitch_rate_dps: f32,
        yaw_rate_dps: f32,
        roll_rate_dps: f32,
    ) -> DriverResult<()>;

    /// Return the most recent attitude sample.
    fn get_state(&self, session: &Self::Session) -> GimbalState;

    /// Yield attitude samples until the session closes.
    async fn state_iterator(
        &self,
        session: &Self::Session,
    ) -> DriverResult<SampleStream<GimbalState>>;
}
