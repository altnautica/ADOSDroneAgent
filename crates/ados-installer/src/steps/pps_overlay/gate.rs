//! Which nodes get the PPS overlay.
//!
//! The overlay claims a header line and asks for a reboot, so it is applied
//! only where the GNSS service runs (`ground_station`, `visor`) and the board's
//! GNSS binding is a receiver on a host UART (`serial`) whose PPS output reaches
//! that line. A drone (its GNSS belongs to the flight controller), a node
//! without a GNSS binding, a `hub` source (the sensor hub captures the pulse on
//! its own timer) and an `fc`/`udp` source (no wire to the host line) are
//! skipped.

use ados_hal_probe::bindings::{GnssBinding, GnssTransport};

/// The profiles whose GNSS service consumes a host PPS line.
const PPS_PROFILES: [&str; 2] = ["ground_station", "visor"];

/// The PPS line to enable for `profile` (installer spelling; the hyphenated
/// form is accepted) on a board whose GNSS binding is `gnss`, or `None` when
/// this node must not get the overlay.
pub fn pps_gpio_for(profile: &str, gnss: Option<&GnssBinding>) -> Option<i64> {
    let profile = profile.trim().replace('-', "_");
    if !PPS_PROFILES.contains(&profile.as_str()) {
        return None;
    }
    let gnss = gnss?;
    match gnss.transport {
        GnssTransport::Serial => gnss.pps_gpio,
        GnssTransport::Hub | GnssTransport::Fc | GnssTransport::Udp => None,
    }
}
