//! Pluggable radio-backend seam.
//!
//! The WFB link runs over one of several backends that all expose the SAME
//! localhost UDP-plane contract (video 5600 / stats 5601 / control 5803/5810) to
//! everything above the radio. Today the only backend is the Linux kernel
//! monitor-mode path that spawns the `wfb_tx` / `wfb_rx` C binaries; a future
//! cross-platform userspace USB (devourer) backend drives the same dongle on a
//! laptop with no kernel monitor mode. This module is the abstraction both share.
//!
//! Phase A is a PURE-ADD: the trait + the kernel wrapper + the selector are built
//! and unit-tested but NOT wired into the service loop. The live bring-up still
//! lives inline in `run_service`; `KernelMonitorBackend` is a thin delegation
//! wrapper over the EXISTING `adapter` / `bringup` / `process` primitives so a
//! later wave can swap the loop onto this seam with no behaviour change. Nothing
//! here is reachable from the live path yet, hence the module-wide dead-code
//! allowance.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use ados_radio::adapter::SelectedAdapter;
use ados_radio::config::WfbConfig;
use ados_radio::process::RadioProcesses;

pub mod kernel;
pub mod select;

// `kernel::KernelMonitorBackend` and `select::{select_backend, BackendSelection}`
// are reachable via their submodule paths. No flat re-exports yet: Phase A wires
// nothing into the live path, so a re-export would warn as unused. The wave that
// puts `run_service` on this seam adds the ergonomic re-exports then.

/// Which concrete radio backend is driving the link. Surfaced (via
/// [`BackendKind::as_wire`]) on the `wfb-stats.json` `backend` field so Mission
/// Control can badge the live radio path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The Linux kernel monitor-mode backend: `iw` monitor mode + the `wfb_tx` /
    /// `wfb_rx` C binaries in their own process groups (the SBC default).
    KernelMonitor,
    /// The cross-platform userspace USB (devourer libusb) backend — drives the
    /// RTL8812EU dongle on Mac/Windows/Linux with no kernel monitor mode. Built
    /// only under the `userspace-usb` feature; the implementation is future work.
    #[cfg(feature = "userspace-usb")]
    UserspaceUsb,
}

impl BackendKind {
    /// The stable wire token for the `wfb-stats.json` `backend` field. `"kernel"`
    /// for the kernel monitor backend, `"userspace"` for the devourer USB backend.
    pub fn as_wire(self) -> &'static str {
        match self {
            BackendKind::KernelMonitor => "kernel",
            #[cfg(feature = "userspace-usb")]
            BackendKind::UserspaceUsb => "userspace",
        }
    }
}

/// The NON-DESTRUCTIVE availability verdict a backend reports for a given config
/// + build + platform, without issuing any adapter command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendAvailability {
    /// The backend can run on this build and platform.
    Ready,
    /// Buildable in principle but not usable right now (e.g. the userspace USB
    /// backend before the devourer driver lands). Carries a static reason.
    Unavailable(&'static str),
    /// Structurally impossible on this build/platform (e.g. the kernel monitor
    /// backend off Linux). Carries a static reason.
    Impossible(&'static str),
}

/// A radio-backend bring-up / teardown error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioError {
    /// No injection-capable adapter could be verified.
    NoAdapter,
    /// The injection interface never landed verified monitor mode + the channel.
    MonitorSetupFailed,
    /// The PHY stayed muted at the not-permitted floor after every recovery.
    PhyMuted,
    /// Spawning the wfb process group failed (carries the OS error text).
    Spawn(String),
    /// The requested backend is not available on this build/platform.
    Unavailable(&'static str),
}

impl std::fmt::Display for RadioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RadioError::NoAdapter => write!(f, "no injection-capable adapter verified"),
            RadioError::MonitorSetupFailed => {
                write!(f, "monitor mode + channel never landed (verified)")
            }
            RadioError::PhyMuted => write!(f, "PHY muted at the not-permitted floor"),
            RadioError::Spawn(e) => write!(f, "wfb process group spawn failed: {e}"),
            RadioError::Unavailable(why) => write!(f, "backend unavailable: {why}"),
        }
    }
}

impl std::error::Error for RadioError {}

/// The cross-backend transmit-plane counters, read identically whichever backend
/// is live so the link-state / liveness logic above the radio is backend-agnostic.
///
/// Only userspace-observable counters live here: `data_{tx,rx}_bytes` mirror the
/// `/sys/class/net/<iface>/statistics/{tx,rx}_bytes` the Rule-37 watchdog tracks,
/// and the received-side fields come from the decoded stats stream + the link
/// proof. The kernel-internal `/proc/<pid>/io` `rchar` and `/proc/net/udp`
/// receive-queue signals are deliberately NOT here — a userspace backend has no
/// analogue for them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaneStats {
    /// `/sys/class/net/<iface>/statistics/tx_bytes` — frames the driver accepted
    /// into the TX ring (the same counter the Rule-37 TX-liveness watchdog reads).
    pub data_tx_bytes: u64,
    /// `/sys/class/net/<iface>/statistics/rx_bytes`.
    pub data_rx_bytes: u64,
    /// Decoded received packets from the link-quality stats stream.
    pub ctrl_rx_packets: u64,
    /// Reed-Solomon-recovered packets from the link-quality stats stream.
    pub fec_recovered: u64,
    /// Unix-epoch millis of the last verified received signal, or `None` when
    /// none has been heard within the link-proof grace window.
    pub last_valid_rx_unix_ms: Option<u64>,
    /// The selected adapter's enumerated USB link speed (Mbps), or `None` when
    /// not USB-backed / unknown / not yet brought up.
    pub adapter_usb_mbps: Option<u32>,
}

/// The handle a successful kernel bring-up produces: the selected injection
/// interface, the verified adapter, and the running radio process group. The
/// (future) `run_service` wiring consumes this; Phase A only constructs it inside
/// [`RadioBackend::bring_up`].
pub struct BroughtUp {
    pub iface: String,
    pub adapter: SelectedAdapter,
    pub proc: Arc<Mutex<RadioProcesses>>,
}

/// A radio backend: the bring-up / retune / teardown lifecycle plus the
/// backend-agnostic transmit-plane counters the link logic above it reads.
///
/// `probe` is a NON-DESTRUCTIVE static check (no `&self`, no adapter command) so a
/// caller can ask "could this backend run here?" before constructing one.
#[async_trait]
pub trait RadioBackend: Send {
    /// Which concrete backend this is (for the `backend` sidecar field).
    fn kind(&self) -> BackendKind;

    /// NON-DESTRUCTIVE availability check for this backend given the config — a
    /// pure build/platform verdict, no adapter command issued.
    fn probe(cfg: &WfbConfig) -> BackendAvailability
    where
        Self: Sized;

    /// Bring the radio up: select + verify the injection adapter, land monitor
    /// mode + the channel, coax the PHY off the muted floor, and spawn the wfb
    /// process group. Returns the running handle.
    async fn bring_up(&mut self, cfg: &WfbConfig) -> Result<BroughtUp, RadioError>;

    /// Retune the live injection interface onto `channel`, verified.
    async fn retune(&mut self, channel: u8) -> Result<(), RadioError>;

    /// The current transmit-plane counters. Lock-DISJOINT from the process
    /// handle's mutex (reads only `/sys`, the shared link stats, and the link
    /// proof) so a status read never contends with a respawn.
    async fn plane_stats(&self) -> PlaneStats;

    /// Tear the radio down: kill the process group and restore the adapter.
    async fn shut_down(&mut self);
}
