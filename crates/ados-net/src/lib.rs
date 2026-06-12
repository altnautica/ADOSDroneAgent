//! Ground-station uplink matrix: priority failover, cloud-reachability health
//! probing, the active-uplink sidecar that drives mesh gateway election, the
//! router control-loop FSM, the hardware managers (ethernet + Wi-Fi client +
//! hostapd AP + USB-gadget tether + cellular modem), the cellular data-cap
//! tracker, and the share-uplink firewall / throttle. The modem speaks D-Bus
//! (ModemManager1) and delegates its AT fallback to the Python service.

pub mod cmd;
pub mod cmdsock;
pub mod data_cap;
pub mod firewall;
pub mod managers;
pub mod nmcli;
pub mod paths;
pub mod process;
pub mod router;
pub mod sidecar;
pub mod sysfs;
pub mod throttle;

// Re-export the surface a consumer (or the daemon) reaches for.
pub use cmd::{CmdOut, CmdRunner, TokioCmdRunner};
pub use cmdsock::CmdState;
pub use data_cap::{DataCapTracker, SysfsUsageSource, UsageSource};
pub use firewall::{FirewallBackend, ShareUplinkFirewall};
pub use managers::{
    apn_for_imsi, build_ssid, EthernetManager, HostapdManager, ModemConfig, ModemManager,
    UsbGadgetManager, WifiClientManager,
};
pub use process::ManagedProcess;
pub use router::active_flag::{ActiveFlagWriter, ActiveUplinkFlag};
pub use router::events::{DataCapState, UplinkEvent, UplinkEventBus, UplinkEventKind};
pub use router::failover;
pub use router::health;
pub use router::{CloudProber, IpRouteApplier, StubManager, UplinkManager, UplinkRouter};
pub use sysfs::detect_ethernet_iface;
pub use throttle::run_throttle_consumer;
