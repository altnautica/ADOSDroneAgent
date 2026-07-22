//! Probed hardware-capability contract.
//!
//! The agent must not trust hardware *declarations* (a board YAML, an ffmpeg
//! `-encoders` listing, a vendor wrapper that is present but backed by no real
//! device). It must derive capabilities by *probing the real silicon*: read the
//! sysfs / device-tree node, issue the ioctl, or trial-init the device, and
//! record the evidence that produced the answer.
//!
//! This module is the typed result shape that crosses process boundaries:
//! every probe returns a [`Probed<T>`], and a full snapshot is a
//! [`HardwareCapabilities`]. The probes themselves (the I/O) live in the
//! `ados-hal-probe` crate; this crate only owns the contract so any service —
//! Rust or, over the state socket, Python — can read it.
//!
//! ## Why `Probed<T>` and not `Option<T>`
//! An `Option` collapses "we looked and it is genuinely not there" with "we did
//! not look" and with "the thing advertised itself but failed when we trial-ran
//! it". Those three answers drive different agent behavior (fall back to
//! software, defer the probe, blacklist a lying wrapper), so they are distinct
//! variants here and each carries the evidence or the reason behind it.

use serde::{Deserialize, Serialize};

/// The outcome of one capability probe.
///
/// `Present` always carries the [`Evidence`] that justified the answer, so a
/// reader can audit *why* the agent believes a device exists. `Absent` carries
/// the [`AbsenceReason`] so a reader can tell "looked, not there" apart from
/// "advertised, but lied". `NotProbed` is the only honest answer when the probe
/// has not run yet (e.g. a destructive probe deferred out of the runtime phase).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Probed<T> {
    /// The capability is present; `evidence` records what proved it.
    Present { value: T, evidence: Evidence },
    /// The capability is genuinely absent; `reason` records why we concluded so.
    Absent { reason: AbsenceReason },
    /// The probe has not run (deferred, or this phase forbids it).
    #[default]
    NotProbed,
}

impl<T> Probed<T> {
    /// True only for [`Probed::Present`].
    pub fn is_present(&self) -> bool {
        matches!(self, Probed::Present { .. })
    }

    /// The probed value, if present.
    pub fn value(&self) -> Option<&T> {
        match self {
            Probed::Present { value, .. } => Some(value),
            _ => None,
        }
    }

    /// Construct a `Present` carrying its evidence.
    pub fn present(value: T, evidence: Evidence) -> Self {
        Probed::Present { value, evidence }
    }

    /// Construct an `Absent` carrying its reason.
    pub fn absent(reason: AbsenceReason) -> Self {
        Probed::Absent { reason }
    }
}

/// What concretely proved a [`Probed::Present`] answer.
///
/// Evidence is ordered weakest-to-strongest: a sysfs path or a device-tree
/// compatible string is a *declaration the kernel exposes*, an ioctl readback
/// confirms the kernel driver answered, and a [`Evidence::TrialInit`] is the
/// strongest — the device accepted a real (bounded) initialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    /// A node was found at this sysfs path.
    SysfsPath(String),
    /// `/proc/device-tree/compatible` (or an `of_node`) carried this string.
    DeviceTreeCompatible(String),
    /// An ioctl on `node` (e.g. `VIDIOC_QUERYCAP`) answered successfully.
    Ioctl { node: String, call: String },
    /// The device accepted a bounded trial init on `node`, completing in `ms`.
    TrialInit { node: String, ms: u32 },
    /// A core cluster's MIDR (main ID register) read from `/proc/cpuinfo`.
    ProcCpuinfo { midr: u32 },
}

/// Why a capability was concluded [`Probed::Absent`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum AbsenceReason {
    /// No node / device file existed to probe.
    NodeMissing,
    /// A node existed but enumerated no usable format (e.g. no H.264 output).
    FormatUnsupported,
    /// The device advertised itself but a trial init failed (exited) after
    /// `exit_after_ms` milliseconds.
    TrialInitFailed { exit_after_ms: u32 },
    /// The trial init hung (never reaped within the bounded window); the probe
    /// runner SIGKILLed the process group and gave up so the agent survives.
    TrialInitHung,
    /// A host quirk explicitly denied this capability under `key`.
    DeniedByQuirk { key: String },
}

/// When a probe runs.
///
/// Some probes are read-only (sysfs, device-tree, ioctl-query) and safe in any
/// phase. Others are *destructive or stateful* — a video encoder trial-init
/// opens and configures a real device, anything that toggles a GPIO or resets a
/// peripheral mutates hardware state. **Destructive / stateful probes are
/// FORBIDDEN unless `phase != Runtime` while the vehicle is armed.** Run them at
/// [`ProbePhase::Setup`] or [`ProbePhase::BootPreArm`], cache the result, and
/// read the cache during armed runtime.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProbePhase {
    /// Operator-driven setup (the safest window; full trial-init allowed).
    Setup,
    /// Boot, before the vehicle can arm (trial-init allowed).
    BootPreArm,
    /// Live runtime. Read-only probes only; destructive probes must read the
    /// cached Setup / BootPreArm result, never re-run while armed.
    Runtime,
}

impl ProbePhase {
    /// True if a destructive / stateful probe (encoder trial-init, anything
    /// that toggles a device) may run in this phase.
    pub fn allows_destructive(&self) -> bool {
        !matches!(self, ProbePhase::Runtime)
    }
}

/// A CPU main-ID-register value (`MIDR_EL1`-style), as read from
/// `/proc/cpuinfo`'s `CPU implementer` / `CPU part` fields.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Midr(pub u32);

impl Midr {
    /// Map the implementer (`0x41` = Arm) + part bits onto a Cortex name.
    ///
    /// Only the part field is consulted; the table covers the cores the agent's
    /// supported boards ship. An unknown part returns `None` so the caller can
    /// surface the raw MIDR rather than guess.
    pub fn cortex_name(&self) -> Option<&'static str> {
        // part number lives in bits [15:4] of the MIDR.
        let part = (self.0 >> 4) & 0xFFF;
        Some(match part {
            0xd0b => "Cortex-A76",
            0xd05 => "Cortex-A55",
            0xd03 => "Cortex-A53",
            0xd08 => "Cortex-A72",
            0xc07 => "Cortex-A7",
            _ => return None,
        })
    }
}

/// One homogeneous cluster of CPU cores (a big.LITTLE rig has more than one).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreCluster {
    /// Resolved Cortex name (e.g. `Cortex-A76`), or the raw MIDR hex when the
    /// part is not in the table.
    pub cortex: String,
    /// The MIDR that identified the cluster.
    pub midr: Midr,
    /// How many cores share this MIDR.
    pub count: u8,
}

/// The SoC's `/proc/device-tree/compatible` strings (most-specific first).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SocCompatible(pub Vec<String>);

/// A V4L2 node found under `/dev`, with the formats it enumerates on its
/// output (the format a memory-to-memory encoder *produces*).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct V4l2Node {
    /// The device path, e.g. `/dev/video11`.
    pub path: String,
    /// True if `VIDIOC_QUERYCAP` reports the memory-to-memory capability
    /// (`V4L2_CAP_VIDEO_M2M*`) — the shape a stateful encoder has.
    pub is_m2m: bool,
    /// FourCCs enumerated on the node's output (capture-from-encoder) side.
    pub output_fourccs: Vec<[u8; 4]>,
}

/// A confirmed hardware H.264 encoder device: a real node that enumerates an
/// H.264 output FourCC (`H264`) and accepted a bounded trial init.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncoderDevice {
    /// The encoder device node, e.g. `/dev/video11`.
    pub node: String,
    /// The H.264 FourCC the node produces (`*b"H264"`).
    pub fourcc: [u8; 4],
}

/// A USB `(idVendor, idProduct)` pair.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsbId {
    pub vid: u16,
    pub pid: u16,
}

/// Whether a USB `(vid, pid)` identifies a bridge commonly fronting an
/// ExpressLRS / CRSF RC transmitter module: a CP2102 (`10c4:ea60`), a CH340
/// (`1a86:7523`), or any Espressif native-USB device (`303a:*`, an
/// ESP32-S3-based module).
///
/// A VID:PID alone CANNOT distinguish an RC module from a flight controller
/// behind the same bridge (plenty of FCs ship on CP2102/CH340 UART bridges),
/// so a match is only ever a HINT to be combined with stronger evidence:
/// an explicit `radio.crsf.device` pin / lane opt-in in the config, or a
/// failed MAVLink/MSP probe on the port. It must never disqualify a port
/// outright on its own.
pub fn is_rc_bridge_usb_id(vid: u16, pid: u16) -> bool {
    matches!((vid, pid), (0x10C4, 0xEA60) | (0x1A86, 0x7523)) || vid == 0x303A
}

/// A serial port the agent may attach a flight controller (or other UART
/// peripheral) to.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerialPort {
    /// The device path, e.g. `/dev/ttyACM0`.
    pub path: String,
    /// The backing USB id when the port is a USB-serial bridge (CDC-ACM / FTDI).
    pub usb: Option<UsbId>,
    /// A heuristic 0-100 score that the port is a flight controller (a higher
    /// score for known FC USB ids / ACM nodes); the caller picks the best.
    pub fc_score: u8,
}

/// A full probed hardware-capability snapshot.
///
/// Each field is a [`Probed<T>`] so a reader can tell a genuine absence apart
/// from "not probed yet". This is the shape the HAL publishes and the encoder /
/// supervisor / setup surfaces consume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareCapabilities {
    /// CPU core clusters (big.LITTLE aware).
    pub cpu: Probed<Vec<CoreCluster>>,
    /// SoC device-tree compatible strings.
    pub soc: Probed<SocCompatible>,
    /// A confirmed hardware H.264 encoder device, if any.
    pub h264_encoder: Probed<EncoderDevice>,
    /// USB device ids on the bus.
    pub usb: Probed<Vec<UsbId>>,
    /// Serial ports (FC candidates scored).
    pub serial: Probed<Vec<SerialPort>>,
}

impl Default for HardwareCapabilities {
    fn default() -> Self {
        Self {
            cpu: Probed::NotProbed,
            soc: Probed::NotProbed,
            h264_encoder: Probed::NotProbed,
            usb: Probed::NotProbed,
            serial: Probed::NotProbed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midr_maps_known_cortex_parts() {
        // implementer bits are ignored; only part [15:4] matters.
        assert_eq!(Midr(0x410f_d0b0 & 0xFFF0).cortex_name(), Some("Cortex-A76"));
        assert_eq!(Midr(0xd0b << 4).cortex_name(), Some("Cortex-A76"));
        assert_eq!(Midr(0xd05 << 4).cortex_name(), Some("Cortex-A55"));
        assert_eq!(Midr(0xd03 << 4).cortex_name(), Some("Cortex-A53"));
        assert_eq!(Midr(0xd08 << 4).cortex_name(), Some("Cortex-A72"));
        assert_eq!(Midr(0xc07 << 4).cortex_name(), Some("Cortex-A7"));
        assert_eq!(Midr(0xfff << 4).cortex_name(), None);
    }

    #[test]
    fn rc_bridge_ids_match_full_pairs_and_espressif_vendor() {
        // The two UART bridges match on the full (vid, pid) pair only.
        assert!(is_rc_bridge_usb_id(0x10C4, 0xEA60)); // CP2102
        assert!(is_rc_bridge_usb_id(0x1A86, 0x7523)); // CH340
        assert!(!is_rc_bridge_usb_id(0x10C4, 0x0001)); // other CP210x product
        assert!(!is_rc_bridge_usb_id(0x1A86, 0x0001)); // other WCH product
                                                       // Espressif native USB matches on the vendor (product ids vary).
        assert!(is_rc_bridge_usb_id(0x303A, 0x1001));
        assert!(is_rc_bridge_usb_id(0x303A, 0x0009));
        // Common FC vendors never match.
        assert!(!is_rc_bridge_usb_id(0x0483, 0x5740)); // STM native USB
        assert!(!is_rc_bridge_usb_id(0x0403, 0x6001)); // FTDI
        assert!(!is_rc_bridge_usb_id(0x1209, 0x5741)); // open-hardware FC
    }

    #[test]
    fn phase_gates_destructive_probes() {
        assert!(ProbePhase::Setup.allows_destructive());
        assert!(ProbePhase::BootPreArm.allows_destructive());
        assert!(!ProbePhase::Runtime.allows_destructive());
    }

    #[test]
    fn probed_present_carries_evidence() {
        let p = Probed::present(
            EncoderDevice {
                node: "/dev/video11".into(),
                fourcc: *b"H264",
            },
            Evidence::TrialInit {
                node: "/dev/video11".into(),
                ms: 42,
            },
        );
        assert!(p.is_present());
        assert_eq!(p.value().unwrap().node, "/dev/video11");
    }

    #[test]
    fn probed_default_is_not_probed() {
        let p: Probed<u32> = Probed::default();
        assert!(matches!(p, Probed::NotProbed));
        assert!(!p.is_present());
    }

    #[test]
    fn snapshot_round_trips_json() {
        let caps = HardwareCapabilities {
            h264_encoder: Probed::absent(AbsenceReason::TrialInitFailed { exit_after_ms: 130 }),
            cpu: Probed::present(
                vec![CoreCluster {
                    cortex: "Cortex-A55".into(),
                    midr: Midr(0xd05 << 4),
                    count: 8,
                }],
                Evidence::ProcCpuinfo { midr: 0xd05 << 4 },
            ),
            ..Default::default()
        };
        let json = serde_json::to_string(&caps).unwrap();
        let back: HardwareCapabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
        // The absent reason survives the round trip with its detail.
        assert!(matches!(
            back.h264_encoder,
            Probed::Absent {
                reason: AbsenceReason::TrialInitFailed { exit_after_ms: 130 }
            }
        ));
    }
}
