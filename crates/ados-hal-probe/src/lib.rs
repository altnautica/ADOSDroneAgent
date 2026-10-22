#![doc = include_str!("../README.md")]

pub mod bindings;
pub mod board_sidecar;
pub mod camera_probe;
pub mod daemon;
pub mod host_facts;
pub mod probe;
pub mod runner;
pub mod sensor_hub;

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
