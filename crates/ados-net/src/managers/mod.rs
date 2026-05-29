//! Hardware uplink managers for the ground-station profile.
//!
//! Each manager implements the [`crate::router::UplinkManager`] trait so the
//! router can probe it uniformly. The ethernet and wifi-client managers ship
//! here; the hostapd and modem managers (the latter HW-gated) land in later
//! chunks.

pub mod ethernet;
pub mod wifi_client;

pub use ethernet::EthernetManager;
pub use wifi_client::{ClientConfig, WifiClientManager};
