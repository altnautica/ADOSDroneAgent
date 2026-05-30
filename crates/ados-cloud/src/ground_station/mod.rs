//! Ground-station cloud relay bridge.
//!
//! On a ground station the cloud uplink can change interface at any moment
//! (Ethernet → Wi-Fi client → cellular → USB tether) as the uplink router fails
//! over. An MQTT client that auto-reconnects does NOT re-bind to the new source
//! interface, so a reconnect after a failover would keep dialing the old route.
//! This bridge ports the Python `cloud_relay_bridge.py`: it watches the active
//! uplink, EXPLICITLY tears the MQTT connection down and brings it back up on
//! every uplink/health change (so the kernel routing table carries the new
//! traffic), downshifts what it forwards to the cloud under cellular data-cap
//! pressure, and posts a small GS status heartbeat every 30 s.
//!
//! Cross-process seam: in the all-Python agent the bridge subscribed to the
//! in-process `UplinkEventBus`. The uplink router now runs in the separate
//! `ados-net` daemon, which publishes the live uplink as the active-uplink
//! sidecar file. The bridge reads that file each tick; the file carries the
//! `active_uplink`, `internet_reachable`, and (additively) the `data_cap_state`
//! the router stamps on it. The bridge reconciles its MQTT lifecycle from the
//! file's transitions, which is the file equivalent of the old bus events.
//!
//! Module layout: [`bridge`] holds the lifecycle + decision logic; the public
//! types are re-exported here.

pub mod bridge;

pub use bridge::{CloudRelayBridge, GsHeartbeat, ThrottleState, UplinkSnapshot};
