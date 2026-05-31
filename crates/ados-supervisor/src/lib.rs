//! `ados-supervisor`: orchestration-over-systemd for the ADOS Drone Agent.
//!
//! Owns the decision of *when* to start/stop/restart each managed unit and
//! *which* units run for the active profile and role. systemd remains the
//! process manager (cgroups, restart, journald, watchdog); this binary issues
//! `systemctl` and never spawns a service itself.

pub mod auto_pair;
pub mod bind;
pub mod config;
pub mod hardware;
pub mod hotplug;
pub mod lifecycle;
pub mod registry;
pub mod role;
pub mod sdnotify;
pub mod systemctl;
