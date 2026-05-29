//! LiDAR driver trait.
//!
//! Ports `ados.sdk.drivers.lidar`. A LiDAR driver yields point-cloud frames
//! from a spinning, solid-state, or single-line ranging device. RPLidar, Livox,
//! Velodyne, and custom UART/I2C rangefinders share this interface.

use std::collections::BTreeMap;

use async_trait::async_trait;
use rmpv::Value;

use super::{Bus, DriverResult, SampleStream};

/// A LiDAR a driver claims it can open.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarCandidate {
    pub driver_id: String,
    pub device_id: String,
    pub label: String,
    pub bus: Bus,
    pub vid_pid: Option<(u16, u16)>,
    pub metadata: BTreeMap<String, Value>,
}

/// Static capabilities of an open LiDAR session. `points_per_frame` is the
/// typical count for a single rotation or sweep.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarCapabilities {
    pub min_range_m: f32,
    pub max_range_m: f32,
    pub horizontal_fov_deg: f32,
    pub vertical_fov_deg: f32,
    pub points_per_frame: u32,
    pub fps: f32,
    pub has_intensity: bool,
    pub has_dual_return: bool,
}

/// A single point in a LiDAR frame. Coordinates are metres in the sensor body
/// frame: `+x` forward, `+y` left, `+z` up. `intensity` is sensor-native and
/// `None` on devices that do not report it.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarPoint {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub intensity: Option<f32>,
    pub return_index: u8,
}

/// One sweep or rotation of points yielded by a LiDAR driver.
#[derive(Debug, Clone, PartialEq)]
pub struct LidarFrame {
    pub timestamp_ns: i64,
    pub sequence: u64,
    pub points: Vec<LidarPoint>,
    pub metadata: BTreeMap<String, Value>,
}

/// Abstract LiDAR driver.
#[async_trait]
pub trait LidarDriver: Send + Sync {
    type Session: Send + Sync;

    /// Scan for LiDARs this driver can open.
    async fn discover(&self) -> DriverResult<Vec<LidarCandidate>>;

    /// Open a session against a LiDAR.
    async fn open(
        &self,
        candidate: &LidarCandidate,
        config: &BTreeMap<String, Value>,
    ) -> DriverResult<Self::Session>;

    /// Release resources held by a session.
    async fn close(&self, session: Self::Session) -> DriverResult<()>;

    /// Return the static capabilities of an open session.
    fn capabilities(&self, session: &Self::Session) -> LidarCapabilities;

    /// Yield point-cloud frames until the session closes.
    async fn frame_iterator(
        &self,
        session: &Self::Session,
    ) -> DriverResult<SampleStream<LidarFrame>>;

    /// Set a runtime parameter (rpm, scan rate, filter mode).
    async fn set_param(
        &self,
        session: &Self::Session,
        param: &str,
        value: Value,
    ) -> DriverResult<()>;
}
