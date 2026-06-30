//! Radio-backend selection.
//!
//! Resolves the configured [`BackendChoice`] (`video.wfb.backend`) plus the build
//! platform into a concrete backend, NON-DESTRUCTIVELY (no adapter command is
//! issued — that is `bring_up`'s job). Phase A only constructs the kernel monitor
//! backend; the userspace USB arm is `userspace-usb`-gated and reports an
//! availability verdict until the devourer backend lands.

#![allow(dead_code)]

use ados_radio::config::{BackendChoice, WfbConfig};

use super::kernel::KernelMonitorBackend;
use super::{BackendAvailability, BackendKind};

/// The outcome of backend selection.
pub enum BackendSelection {
    /// The kernel monitor-mode backend (the SBC default; the only backend
    /// constructible in Phase A). Boxed: the kernel backend carries a config
    /// snapshot, so it dwarfs the `Unavailable` variant.
    Kernel(Box<KernelMonitorBackend>),
    /// No usable backend for the requested choice on this build/platform. Carries
    /// the would-be kind (when known) and the non-destructive availability verdict
    /// (Impossible off Linux, or Unavailable when userspace is asked for but the
    /// devourer backend has not landed yet).
    Unavailable {
        kind: Option<BackendKind>,
        availability: BackendAvailability,
    },
}

/// Choose the radio backend for `cfg` from `choice`, platform-gated. Pure: no
/// adapter command, no I/O beyond constructing the (un-brought-up) kernel wrapper.
pub fn select_backend(cfg: &WfbConfig, choice: BackendChoice) -> BackendSelection {
    match resolve(choice, cfg!(target_os = "linux")) {
        Resolution::Kernel => BackendSelection::Kernel(Box::new(KernelMonitorBackend::new(cfg))),
        Resolution::KernelImpossible(availability) => BackendSelection::Unavailable {
            kind: Some(BackendKind::KernelMonitor),
            availability,
        },
        Resolution::Userspace(availability) => BackendSelection::Unavailable {
            kind: None,
            availability,
        },
    }
}

/// Pure choice + platform → resolution, no I/O, testable off any host.
#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    /// Use the kernel monitor-mode backend.
    Kernel,
    /// Kernel requested/auto but impossible on this platform (carries the verdict).
    KernelImpossible(BackendAvailability),
    /// Userspace requested; not available in this build (carries the verdict —
    /// Unavailable under `userspace-usb`, Impossible without it).
    Userspace(BackendAvailability),
}

fn resolve(choice: BackendChoice, is_linux: bool) -> Resolution {
    match choice {
        // `auto` resolves to the kernel backend on Linux.
        BackendChoice::Kernel | BackendChoice::Auto => {
            if is_linux {
                Resolution::Kernel
            } else {
                Resolution::KernelImpossible(BackendAvailability::Impossible(
                    "kernel monitor backend requires Linux",
                ))
            }
        }
        BackendChoice::Userspace => {
            #[cfg(feature = "userspace-usb")]
            {
                Resolution::Userspace(BackendAvailability::Unavailable(
                    "userspace USB backend not yet implemented",
                ))
            }
            #[cfg(not(feature = "userspace-usb"))]
            {
                Resolution::Userspace(BackendAvailability::Impossible(
                    "userspace USB backend requires the userspace-usb build feature",
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_default_auto_on_linux_is_kernel() {
        // select_backend(default, Linux) → KernelMonitor.
        assert_eq!(resolve(BackendChoice::Auto, true), Resolution::Kernel);
        assert_eq!(resolve(BackendChoice::Kernel, true), Resolution::Kernel);
    }

    #[test]
    fn resolve_kernel_off_linux_is_impossible() {
        match resolve(BackendChoice::Auto, false) {
            Resolution::KernelImpossible(BackendAvailability::Impossible(_)) => {}
            other => panic!("expected KernelImpossible, got {other:?}"),
        }
    }

    #[test]
    fn resolve_userspace_without_feature_is_impossible() {
        // The default build (no `userspace-usb`): userspace needs the build feature.
        #[cfg(not(feature = "userspace-usb"))]
        match resolve(BackendChoice::Userspace, true) {
            Resolution::Userspace(BackendAvailability::Impossible(_)) => {}
            other => panic!("expected Userspace Impossible, got {other:?}"),
        }
    }

    #[cfg(feature = "userspace-usb")]
    #[test]
    fn resolve_userspace_with_feature_is_unavailable() {
        match resolve(BackendChoice::Userspace, true) {
            Resolution::Userspace(BackendAvailability::Unavailable(_)) => {}
            other => panic!("expected Userspace Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn select_backend_default_matches_host_platform() {
        // select_backend reads the real build platform: a Kernel backend on Linux,
        // an Unavailable verdict elsewhere (proves no adapter mutation either way).
        let sel = select_backend(&WfbConfig::default(), BackendChoice::Auto);
        if cfg!(target_os = "linux") {
            assert!(matches!(sel, BackendSelection::Kernel(_)));
        } else {
            assert!(matches!(
                sel,
                BackendSelection::Unavailable {
                    availability: BackendAvailability::Impossible(_),
                    ..
                }
            ));
        }
    }
}
