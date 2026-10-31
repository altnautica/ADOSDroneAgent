//! The inert default host.

use super::HostServices;

/// The default host: every host-coupled method returns `not_implemented`.
///
/// The MAVLink router and runtime lookups stay unwired here. The
/// handshake and the capability gate are fully exercised against this host; the
/// only thing it does not do is the real service work, which is the intended
/// boundary for this core crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHost;

impl HostServices for NoopHost {}
