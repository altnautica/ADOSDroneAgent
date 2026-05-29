//! Hardware managers for the ground-station profile.
//!
//! The ethernet and wifi-client managers implement the
//! [`crate::router::UplinkManager`] trait so the router can probe them as
//! uplinks. The hostapd manager owns the LAN-side AP (not an uplink) and the
//! usb-gadget manager owns the OTG tether netdev. The modem manager (HW-gated)
//! lands in the last chunk.

pub mod ethernet;
pub mod hostapd;
pub mod usb_gadget;
pub mod wifi_client;

pub use ethernet::EthernetManager;
pub use hostapd::{build_ssid, HostapdManager};
pub use usb_gadget::UsbGadgetManager;
pub use wifi_client::{ClientConfig, WifiClientManager};
