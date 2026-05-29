//! GPS driver trait.
//!
//! Ports `ados.sdk.drivers.gps`. A GPS driver decodes a position-fix stream
//! from a u-blox, NMEA, RTK, or vendor-custom receiver and exposes it as a
//! series of [`GpsFix`] samples. RTK-capable drivers accept RTCM corrections
//! via [`GpsDriver::inject_rtcm`].

use std::collections::BTreeMap;

use async_trait::async_trait;
use rmpv::Value;

use super::{Bus, DriverResult, SampleStream};

/// A GPS receiver a driver claims it can open.
#[derive(Debug, Clone, PartialEq)]
pub struct GpsCandidate {
    pub driver_id: String,
    pub device_id: String,
    pub label: String,
    pub bus: Bus,
    pub vid_pid: Option<(u16, u16)>,
    pub metadata: BTreeMap<String, Value>,
}

/// Static capabilities of an open GPS session.
#[derive(Debug, Clone, PartialEq)]
pub struct GpsCapabilities {
    pub protocol: String,
    pub constellations: Vec<String>,
    pub max_update_hz: f32,
    pub supports_rtk: bool,
    pub supports_dual_band: bool,
    pub supports_heading: bool,
}

/// One position fix. Latitude/longitude are degrees (WGS-84), altitude metres
/// above mean sea level. `fix_type`: 0 none, 2 2D, 3 3D, 4 DGPS, 5 RTK float,
/// 6 RTK fixed.
#[derive(Debug, Clone, PartialEq)]
pub struct GpsFix {
    pub timestamp_ns: i64,
    pub latitude_deg: f64,
    pub longitude_deg: f64,
    pub altitude_msl_m: f32,
    pub fix_type: u8,
    pub satellites_used: u8,
    pub hdop: f32,
    pub vdop: f32,
    pub horizontal_accuracy_m: Option<f32>,
    pub vertical_accuracy_m: Option<f32>,
    pub speed_mps: Option<f32>,
    pub course_deg: Option<f32>,
    pub heading_deg: Option<f32>,
    pub metadata: BTreeMap<String, Value>,
}

/// Abstract GPS driver.
#[async_trait]
pub trait GpsDriver: Send + Sync {
    type Session: Send + Sync;

    /// Scan for GPS receivers this driver can open.
    async fn discover(&self) -> DriverResult<Vec<GpsCandidate>>;

    /// Open a session against a GPS receiver.
    async fn open(
        &self,
        candidate: &GpsCandidate,
        config: &BTreeMap<String, Value>,
    ) -> DriverResult<Self::Session>;

    /// Release resources held by a session.
    async fn close(&self, session: Self::Session) -> DriverResult<()>;

    /// Return the static capabilities of an open session.
    fn capabilities(&self, session: &Self::Session) -> GpsCapabilities;

    /// Yield fixes until the session closes.
    async fn fix_iterator(&self, session: &Self::Session) -> DriverResult<SampleStream<GpsFix>>;

    /// Forward an RTCM correction packet into the receiver. Receivers without
    /// RTK return [`super::DriverError::NotSupported`].
    async fn inject_rtcm(&self, session: &Self::Session, payload: &[u8]) -> DriverResult<()>;
}
