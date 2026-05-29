//! Hardware driver traits for driver-layer plugins.
//!
//! Ports `ados.sdk.drivers`. A driver-plugin implements one of these traits and
//! registers the instance with the host's peripheral manager from its
//! `on_start` hook (via [`crate::context::PeripheralClient`]). The host owns
//! discovery and arbitration; the driver answers `discover` honestly, opens
//! sessions on demand, and yields samples until the session closes.
//!
//! Each trait mirrors the matching Python abstract base class method-for-method.
//! The Python `AsyncIterator` returns become a [`SampleStream`] (a boxed
//! [`futures_core::Stream`]); the `async def` methods use [`async_trait`] so the
//! traits stay object-safe behind a `Box<dyn ...>`. A `Session` associated type
//! replaces the Python opaque `*Session` base class: the host treats it as a
//! token and only hands it back to the driver's own methods.

mod camera;
mod errors;
mod esc;
mod gimbal;
mod gps;
mod lidar;
mod payload_actuator;

use std::pin::Pin;

use futures_core::Stream;

pub use camera::{CameraCandidate, CameraCapabilities, CameraDriver, FrameBuffer};
pub use errors::{DriverError, DriverResult};
pub use esc::{EscCandidate, EscCapabilities, EscDriver, EscTelemetry};
pub use gimbal::{GimbalCandidate, GimbalCapabilities, GimbalDriver, GimbalState};
pub use gps::{GpsCandidate, GpsCapabilities, GpsDriver, GpsFix};
pub use lidar::{LidarCandidate, LidarCapabilities, LidarDriver, LidarFrame, LidarPoint};
pub use payload_actuator::{
    PayloadActuatorDriver, PayloadCandidate, PayloadCapabilities, PayloadCommand, PayloadState,
};

/// A driver sample stream: the Rust analogue of the Python `AsyncIterator` the
/// driver base classes return from `frame_iterator` / `fix_iterator` /
/// `state_iterator` / `telemetry_iterator`. Boxed so the trait is object-safe.
pub type SampleStream<T> = Pin<Box<dyn Stream<Item = DriverResult<T>> + Send>>;

/// A unique, short reference id for a driver instance the host records. The
/// peripheral manager never sees the live driver object; it routes by this id.
/// Mirrors `_driver_ref`: `driver_id` else the type name.
pub trait DriverRef {
    /// A stable id for this driver instance within one plugin process.
    fn driver_id(&self) -> &str;
}

/// A bus a candidate device is attached to. Free-form to match the Python
/// `bus: str` field (`usb`, `csi`, `uart`, `i2c`, `spi`, `rtsp`, ...).
pub type Bus = String;
