//! Hardware managers for the ground-station profile.
//!
//! The ethernet, wifi-client, and modem managers implement the
//! [`crate::router::UplinkManager`] trait so the router can probe them as
//! uplinks. The hostapd manager owns the LAN-side AP (not an uplink) and the
//! usb-gadget manager owns the OTG tether netdev. The modem manager is the
//! cellular (`wwan0`) uplink: D-Bus-first via ModemManager1, AT fallback
//! delegated to Python; HW-gated and disabled by default.

pub mod ethernet;
pub mod hostapd;
pub mod modem;
pub mod usb_gadget;
pub mod wifi_client;

pub use ethernet::EthernetManager;
pub use hostapd::{build_ssid, HostapdManager};
pub use modem::{apn_for_imsi, ModemConfig, ModemManager};
pub use usb_gadget::UsbGadgetManager;
pub use wifi_client::{ClientConfig, WifiClientManager};
