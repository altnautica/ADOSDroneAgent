//! Pluggable radio-backend seam.
//!
//! The WFB link runs over one of several backends that all expose the SAME
//! localhost UDP-plane contract (video 5600 / stats 5601 / control 5803/5810) to
//! everything above the radio. Today the only backend is the Linux kernel
//! monitor-mode path that spawns the `wfb_tx` / `wfb_rx` C binaries; a future
//! cross-platform userspace USB (devourer) backend drives the same dongle on a
//! laptop with no kernel monitor mode. This module is the abstraction both share.
//!
//! ## Why this seam is built, inert, and staying that way for now
//!
//! Nothing here is reachable from the live path: the trait, the kernel wrapper
//! and the selector are built and unit-tested, while the live bring-up still
//! runs inline in `run_service`. `KernelMonitorBackend` is a thin delegation
//! wrapper over the existing `adapter` / `bringup` / `process` primitives, so
//! putting the service loop on this seam is a behaviour-preserving move when it
//! happens. Hence the module-wide dead-code allowance.
//!
//! Three facts decide "wire it or delete it", and they are recorded here because
//! that question has been re-opened three times:
//!
//! 1. **The second backend is specified and deliberately deferred by the project
//!    owner.** The cross-platform userspace-USB backend has a written design (the
//!    radio-backend abstraction spec under `product/specs/ados-direct-link/`),
//!    and the owner's build plan explicitly defers it — ship the kernel path,
//!    leave this trait inert. Deleting it would throw away specced substrate that
//!    a named, scheduled piece of work resumes from.
//! 2. **The trait cannot host that backend as it stands.** [`BroughtUp`] carries
//!    an `Arc<Mutex<RadioProcesses>>` — a forked `wfb_tx`/`wfb_rx` process group.
//!    A userspace backend has no forked process at all (it drives the dongle
//!    in-process), so it cannot produce that handle. Resuming therefore begins by
//!    reshaping this trait's bring-up result, NOT by wiring the kernel backend
//!    into `run_service` first; wiring first would harden the leak.
//! 3. **Wiring is verifiable only on real radio hardware.** Whether the loop on
//!    this seam behaves identically is a claim about monitor mode, injection and
//!    a live link, so no off-rig test can settle it. It belongs to a bench
//!    session with a radio attached, not to a refactor pass.

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
// are reachable via their submodule paths. No flat re-exports while the seam is
// inert: nothing in the live path uses them, so a re-export would warn as
// unused. Whatever puts `run_service` on this seam adds the ergonomic re-exports
// then.

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
/// interface, the verified adapter, and the running radio process group.
///
/// The (future) `run_service` wiring consumes this; while the seam is inert it is
/// only constructed inside [`RadioBackend::bring_up`]. It is also the structural
/// reason a second backend cannot implement this trait unchanged: a userspace
/// backend drives the dongle in-process and has no `RadioProcesses` group to hand
/// back, so resuming that work starts by reshaping this type.
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
