//! `ados-supervisor`: service orchestration for the ADOS Drone Agent.
//!
//! Owns the decision of *when* to start/stop/restart each managed unit and
//! *which* units run for the active profile and role. The host process manager
//! (systemd on Linux, launchd on macOS) owns the running processes (restart,
//! supervision, watchdog); this binary issues lifecycle verbs through the
//! [`process_manager`] backend and never spawns a service itself.

pub mod auto_pair;
pub mod bind;
pub mod config;
pub mod hardware;
pub mod hotplug;
pub mod lifecycle;
pub mod mac_pin;
pub mod mgmt_failover;
pub mod mgmt_link_guardian;
pub mod process_manager;
pub mod reg_reconciler;
pub mod registry;
pub mod role;
pub mod rtl_modprobe;
pub mod sdnotify;
pub mod service_memory;
pub mod usb_rehome;
pub mod video_cmd;
pub mod wifi_powersave;
pub mod wifi_selfheal;
