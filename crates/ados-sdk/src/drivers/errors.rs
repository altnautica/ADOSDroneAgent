//! Driver-layer error type.
//!
//! Ports `ados.sdk.drivers.errors`. A driver raises these for predictable
//! failure modes so the host can translate them into `sensor.<id>.error`
//! events and surface a meaningful reason in the GCS detail page. Unhandled
//! errors fall through to the plugin supervisor's circuit breaker, exactly as
//! the Python base `DriverError` chains under `PluginError`.

use thiserror::Error;

/// A driver-layer failure. The variants mirror the Python `DriverError`
/// subclasses; [`DriverError::Other`] carries any predictable, recoverable
/// condition that does not fit the named kinds.
#[derive(Debug, Error)]
pub enum DriverError {
    /// The device a candidate referenced could not be located (unplugged
    /// between `discover` and `open`, or during a hotplug race). Mirrors
    /// `DriverDeviceNotFound`.
    #[error("device not found: {0}")]
    DeviceNotFound(String),
    /// The driver lacks the OS permission to claim the device (missing udev
    /// rule, unreadable device node, denied capability token). Mirrors
    /// `DriverPermissionDenied`.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// An unknown runtime parameter name was passed to `set_param`, or an
    /// `actuate` action id is not in the driver's declared action menu. The
    /// Python ABCs raise `ValueError` for these so the GCS panel can surface
    /// the rejection cleanly.
    #[error("invalid parameter: {0}")]
    InvalidParam(String),
    /// The driver does not implement an optional capability (a rate command on
    /// a position-only gimbal, RTCM injection on a non-RTK GPS). Mirrors the
    /// Python `NotImplementedError` the optional methods raise.
    #[error("not supported: {0}")]
    NotSupported(String),
    /// Any other predictable, recoverable driver condition. Mirrors the base
    /// `DriverError`.
    #[error("driver error: {0}")]
    Other(String),
}

/// Result alias for driver methods.
pub type DriverResult<T> = Result<T, DriverError>;
