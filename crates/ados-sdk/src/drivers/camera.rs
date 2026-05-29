//! Camera driver trait.
//!
//! Ports `ados.sdk.drivers.camera`. A camera driver pumps frames from a
//! physical or networked imaging device. Visible, thermal, depth, and
//! multi-spectral devices share this interface. The host owns discovery and
//! arbitration; the driver answers [`CameraDriver::discover`], opens sessions,
//! and yields [`FrameBuffer`] frames until the session closes.

use std::collections::BTreeMap;

use async_trait::async_trait;
use rmpv::Value;

use super::{Bus, DriverResult, SampleStream};

/// A device a driver claims it can open. `device_id` should be stable across
/// reboots when the bus exposes a stable identifier (USB serial, fixed CSI
/// lane, RTSP URL); otherwise it must at least be deterministic within one boot.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraCandidate {
    pub driver_id: String,
    pub device_id: String,
    pub label: String,
    pub bus: Bus,
    pub vid_pid: Option<(u16, u16)>,
    pub metadata: BTreeMap<String, Value>,
}

/// Static capabilities a session reports after [`CameraDriver::open`].
#[derive(Debug, Clone, PartialEq)]
pub struct CameraCapabilities {
    pub radiometric: bool,
    pub bit_depth: u32,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub pixel_format: String,
    pub streaming_protocol: String,
    pub color_spaces: Vec<String>,
    pub has_audio: bool,
}

/// One frame yielded by a camera driver. `radiometric_k` carries the per-pixel
/// temperature reconstruction matrix when the sensor is radiometric.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameBuffer {
    pub timestamp_ns: i64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub pixel_format: String,
    pub data: Vec<u8>,
    pub radiometric_k: Option<Vec<u8>>,
    pub metadata: BTreeMap<String, Value>,
}

/// Abstract camera driver. Implementations register with the peripheral
/// manager from `on_start`. The host then calls [`CameraDriver::discover`],
/// arbitrates among candidates, and routes selected devices through
/// [`CameraDriver::open`].
///
/// `Session` is the driver's opaque per-open state (file descriptors, decoder
/// pipelines, vendor SDK handles). The host treats it as a token and only
/// passes it back to the driver's lifecycle and streaming methods.
#[async_trait]
pub trait CameraDriver: Send + Sync {
    /// Opaque per-open state. The Rust analogue of the Python `CameraSession`
    /// base class.
    type Session: Send + Sync;

    /// Scan for devices this driver can open. Called on boot and on hotplug. An
    /// empty list means the driver recognised nothing and is fine.
    async fn discover(&self) -> DriverResult<Vec<CameraCandidate>>;

    /// Open a session against a device. `config` carries driver-specific
    /// options validated against the plugin's `config-schema.json`.
    async fn open(
        &self,
        candidate: &CameraCandidate,
        config: &BTreeMap<String, Value>,
    ) -> DriverResult<Self::Session>;

    /// Release resources held by a session.
    async fn close(&self, session: Self::Session) -> DriverResult<()>;

    /// Return the static capabilities of an open session.
    fn capabilities(&self, session: &Self::Session) -> CameraCapabilities;

    /// Yield frames until the session closes or the stream is dropped. The
    /// Rust analogue of the Python `frame_iterator` `AsyncIterator`.
    async fn frame_iterator(
        &self,
        session: &Self::Session,
    ) -> DriverResult<SampleStream<FrameBuffer>>;

    /// Set a runtime parameter (gain, exposure, palette, shutter). Return
    /// [`super::DriverError::InvalidParam`] for unknown parameter names so the
    /// GCS panel can surface the rejection.
    async fn set_param(
        &self,
        session: &Self::Session,
        param: &str,
        value: Value,
    ) -> DriverResult<()>;
}
