//! Ground-station uplink matrix: priority failover, cloud-reachability health
//! probing, the active-uplink sidecar that drives mesh gateway election, and
//! the router control-loop FSM. The hardware managers (Wi-Fi client, ethernet,
//! hostapd, modem), the firewall, the USB-gadget surface, and the data-cap
//! tracker land in later chunks.

pub mod paths;
pub mod router;
pub mod sidecar;

// Re-export the surface a consumer (or the daemon) reaches for.
pub use router::active_flag::{ActiveFlagWriter, ActiveUplinkFlag};
pub use router::events::{DataCapState, UplinkEvent, UplinkEventBus, UplinkEventKind};
pub use router::failover;
pub use router::health;
pub use router::{CloudProber, IpRouteApplier, StubManager, UplinkManager, UplinkRouter};
