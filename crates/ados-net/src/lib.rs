//! Ground-station uplink matrix: priority failover, cloud-reachability health
//! probing, the active-uplink sidecar that drives mesh gateway election, the
//! router control-loop FSM, the hardware uplink managers (ethernet + Wi-Fi
//! client), the cellular data-cap tracker, and the share-uplink firewall /
//! throttle. The hostapd and modem managers (the latter HW-gated), plus the
//! USB-gadget surface, land in later chunks.

pub mod cmd;
pub mod data_cap;
pub mod firewall;
pub mod managers;
pub mod nmcli;
pub mod paths;
pub mod router;
pub mod sidecar;
pub mod throttle;

// Re-export the surface a consumer (or the daemon) reaches for.
pub use cmd::{CmdOut, CmdRunner, TokioCmdRunner};
pub use data_cap::{DataCapTracker, SysfsUsageSource, UsageSource};
pub use firewall::{FirewallBackend, ShareUplinkFirewall};
pub use managers::{EthernetManager, WifiClientManager};
pub use router::active_flag::{ActiveFlagWriter, ActiveUplinkFlag};
pub use router::events::{DataCapState, UplinkEvent, UplinkEventBus, UplinkEventKind};
pub use router::failover;
pub use router::health;
pub use router::{CloudProber, IpRouteApplier, StubManager, UplinkManager, UplinkRouter};
