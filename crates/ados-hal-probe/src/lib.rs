//! Probe-first hardware-capability detection.
//!
//! The agent must not trust hardware *declarations*. A board YAML can be wrong,
//! `ffmpeg -encoders` advertises wrapper codecs that are backed by no real
//! device, and a vendor binary can list a feature it cannot deliver. This crate
//! derives capabilities by *probing the real silicon*: it reads the sysfs node,
//! the device-tree compatible string, issues the V4L2 ioctl, or runs a bounded
//! trial-init, and records the [`Evidence`](ados_protocol::hwcaps::Evidence)
//! that produced each answer.
//!
//! The result shapes live in [`ados_protocol::hwcaps`] so any service can read
//! them; this crate owns the I/O. The dangerous part — trial-initializing a
//! real device that might hang the kernel ioctl into a D-state that no signal
//! can reap — is isolated in [`runner`], which spawns the trial in its own
//! process group and SIGKILLs the whole group on timeout so the agent always
//! survives.
//!
//! ## Phases
//! Destructive / stateful probes (encoder trial-init) only run when the
//! [`ProbePhase`] permits it (`Setup` / `BootPreArm`, never armed `Runtime`).
//! Read-only probes (CPU, SoC, USB, serial) run in any phase.

pub mod probe;
pub mod runner;

pub use ados_protocol::hwcaps::{
    AbsenceReason, CoreCluster, EncoderDevice, Evidence, HardwareCapabilities, Midr, ProbePhase,
    Probed, SerialPort, SocCompatible, UsbId, V4l2Node,
};

pub use probe::{
    cpu::probe_cores, serial::probe_serial_ports, soc::probe_soc, usb::probe_usb_ids,
    video::probe_h264_encoder,
};

pub use runner::{run_probe_sandboxed, ProbeOutcome};

/// Run every probe and assemble a full [`HardwareCapabilities`] snapshot.
///
/// `phase` gates the destructive probes: the H.264 encoder trial-init runs only
/// when [`ProbePhase::allows_destructive`] is true; otherwise it is left
/// [`Probed::NotProbed`] so a reader knows it was deferred, not absent. The
/// read-only probes (CPU, SoC, USB, serial) always run.
///
/// `crsf_pin` is the pinned CRSF/ELRS RC-module device (`radio.crsf.device`),
/// if configured: the future wiring must pass it so the serial inventory
/// scores that port 0 (never an FC candidate), mirroring the exclusion the
/// MAVLink router's live discovery path enforces. `None` = no pin configured.
pub fn probe_all(phase: ProbePhase, crsf_pin: Option<&str>) -> HardwareCapabilities {
    HardwareCapabilities {
        cpu: probe_cores(),
        soc: probe_soc(),
        h264_encoder: probe_h264_encoder(phase),
        usb: probe_usb_ids(),
        serial: probe_serial_ports(crsf_pin),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_all_runtime_defers_destructive_encoder_probe() {
        // In armed runtime the encoder trial-init must not run; the stub returns
        // NotProbed (and on a real rig the runtime gate keeps it deferred).
        let caps = probe_all(ProbePhase::Runtime, None);
        assert!(matches!(caps.h264_encoder, Probed::NotProbed));
    }
}
