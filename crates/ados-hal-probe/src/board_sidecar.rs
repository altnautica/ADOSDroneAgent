//! The board fingerprint sidecar (`/run/ados/board.json`), written from Rust.
//!
//! Every Rust reader of the board's identity goes through this file: the pairing
//! route's `board` field, the native status route's board object, and the cloud
//! offload reconciler's `npu_tops` (which decides local-vs-offload detection).
//! It had exactly one writer — a Python call inside the FastAPI runtime's model
//! manager — so on the advertised zero-Python headless profile it was NEVER
//! written: Mission Control showed board `unknown`, the status route served an
//! empty board object, and a board with a 3-6 TOPS NPU read `npu_tops: 0` and
//! offloaded detection it could have run onboard. The same hole opened
//! transiently on every normal boot until uvicorn finished starting.
//!
//! So the write moves here, to the Rust plane, and this is the ONLY writer. The
//! board-profile YAMLs under `src/ados/hal/boards/` remain the single source of
//! truth for both languages — permanent HAL Python is the *detector*, not the
//! sidecar writer — and they are embedded in this binary rather than read from
//! the Python package, because a zero-Python node does not necessarily carry
//! that package and a sidecar writer that depends on it would reproduce the
//! defect it exists to fix.
//!
//! The document shape is the Python `BoardInfo.to_dict()` key set plus the
//! `version` the sidecar registry declares (`contracts.toml`, `board` = 1),
//! which the Python writer omitted — so every Rust reader logged a
//! schema-version warning on every boot.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::bindings::{BatteryGaugeBinding, GnssBinding, ImuBinding, SensorHubBinding};
use crate::host_facts::probe_host_facts;
use crate::sensor_hub::{SensorHubPresence, UsbTty};

/// The canonical sidecar path. Honours `ADOS_RUN_DIR` like the sibling
/// daemons, so a rootless per-user install lands it under `$HOME/.ados/run`.
pub fn sidecar_path() -> std::path::PathBuf {
    match std::env::var_os("ADOS_RUN_DIR") {
        Some(dir) => std::path::PathBuf::from(dir).join("board.json"),
        None => std::path::PathBuf::from("/run/ados/board.json"),
    }
}

/// Schema version of the `board` sidecar. Kept in lock-step with the shared
/// contract registry (asserted by a test).
pub const BOARD_SIDECAR_VERSION: u16 = 1;

/// The compute block of a board profile. Extra keys (cores, gpu, hw_encoder…)
/// are ignored, exactly as the Python `ComputeSection` ignores them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComputeSection {
    #[serde(default)]
    pub npu_tops: f64,
    /// `"none"` (default) or `"onnx"`: whether this board can run the detector
    /// on the CPU without an NPU.
    #[serde(default)]
    pub local_inference: Option<String>,
}

/// One display the board declares, by id. Only the id is parsed here: it is
/// what validates an operator's `--display <id>` against the board.
#[derive(Debug, Clone, Deserialize)]
pub struct DisplayBinding {
    pub id: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DisplaysSection {
    #[serde(default)]
    pub supported: Vec<DisplayBinding>,
}

/// The panel button roles a board wires, from the profile's optional
/// `buttons:` block. Each value is the board's GPIO number: BCM on a Raspberry
/// Pi, the Linux GPIO number (`bank * 32 + group * 8 + index`) on Rockchip.
/// Absent means the board has no panel buttons (a Raspberry Pi keeps the
/// reference-panel lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonPins {
    pub up: u32,
    pub down: u32,
    pub select: u32,
    pub back: u32,
}

impl ButtonPins {
    /// The four pins in role order.
    pub fn pins(&self) -> [u32; 4] {
        [self.up, self.down, self.select, self.back]
    }
}

/// The rotary encoder (KY-040 class) a board wires, from the profile's
/// optional `encoder:` block, in the same numbering as [`ButtonPins`]: the
/// quadrature outputs `a` (`CLK`) and `b` (`DT`) and the push switch `sw`.
/// Absent means the board has no encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderPins {
    pub a: u32,
    pub b: u32,
    pub sw: u32,
}

impl EncoderPins {
    /// The three pins in role order.
    pub fn pins(&self) -> [u32; 3] {
        [self.a, self.b, self.sw]
    }
}

/// One GPIO line a board profile reserves for a bus, a console or a
/// peripheral (`reserved_gpio:` entries). Only the number is parsed; the
/// header pin and the use are documentation for the reader of the YAML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ReservedGpio {
    pub gpio: u32,
}

/// The host facts a variant can be discriminated on.
///
/// Only `cpu_cores` today, because that is what separates the two SoC bins
/// Radxa ships behind ONE device tree.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct VariantMatch {
    #[serde(default)]
    pub cpu_cores: Option<i64>,
}

/// A same-device-tree hardware variant of a board.
///
/// Radxa publishes a single DT for the ROCK 5C (RK3588S2, 8 cores) and the
/// ROCK 5C Lite (RK3582, 6 cores), so a pattern match cannot tell them apart
/// and whichever profile filename sorted first won — a full 5C published
/// `soc: RK3582` and a name ending in "Lite" into `/run/ados/board.json` and
/// on to the operator's screen. A variant names the discriminator explicitly
/// and overrides only the identity fields that actually differ.
#[derive(Debug, Clone, Deserialize)]
pub struct BoardVariant {
    pub id: String,
    /// Facts that must hold for this variant. An EMPTY match never selects:
    /// a catch-all variant would silently rename every unit.
    #[serde(default)]
    pub when: VariantMatch,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub soc: Option<String>,
    #[serde(default)]
    pub default_tier: Option<i64>,
}

/// The slice of a board-profile YAML the fingerprint is built from. Every other
/// section (cameras, radios, gpio, flight_controller…) is ignored here; the
/// services that need them read the YAML themselves.
#[derive(Debug, Clone, Deserialize)]
pub struct BoardProfile {
    pub name: String,
    #[serde(default = "unknown")]
    pub vendor: String,
    #[serde(default = "unknown")]
    pub soc: String,
    #[serde(default = "default_arch")]
    pub arch: String,
    #[serde(default)]
    pub model_patterns: Vec<String>,
    #[serde(default = "default_tier_field")]
    pub default_tier: i64,
    #[serde(default)]
    pub hw_video_codecs: Vec<String>,
    #[serde(default)]
    pub compute: ComputeSection,
    #[serde(default)]
    pub displays: DisplaysSection,
    #[serde(default)]
    pub variants: Vec<BoardVariant>,
    #[serde(default)]
    pub buttons: Option<ButtonPins>,
    #[serde(default)]
    pub encoder: Option<EncoderPins>,
    #[serde(default)]
    pub imu: Option<ImuBinding>,
    #[serde(default)]
    pub gnss: Option<GnssBinding>,
    #[serde(default)]
    pub sensor_hub: Option<SensorHubBinding>,
    #[serde(default)]
    pub battery_gauge: Option<BatteryGaugeBinding>,
    #[serde(default)]
    pub reserved_gpio: Vec<ReservedGpio>,
    /// The profile's YAML filename stem (`cubie-a7s`, `rock-5c-lite`). Not a
    /// YAML key: [`load_profiles`] stamps it from the embedded file name, and it
    /// is the ONE canonical token `/etc/ados/board_override` accepts.
    #[serde(skip)]
    pub stem: String,
}

impl BoardProfile {
    /// Every GPIO number the profile binds to something other than a button:
    /// the `reserved_gpio` entries plus the GNSS PPS line.
    pub fn reserved_lines(&self) -> Vec<u32> {
        let mut lines: Vec<u32> = self.reserved_gpio.iter().map(|r| r.gpio).collect();
        if let Some(pps) = self
            .gnss
            .as_ref()
            .and_then(|g| g.pps_gpio)
            .and_then(|p| u32::try_from(p).ok())
        {
            lines.push(pps);
        }
        lines.sort_unstable();
        lines.dedup();
        lines
    }

    /// Why the declared panel inputs cannot be used, if they cannot: a line
    /// declared twice (within the buttons, within the encoder, or shared
    /// between them), or a button or encoder line on a reserved line.
    pub fn panel_conflict(&self) -> Option<String> {
        let mut declared: Vec<(&str, u32)> = Vec::new();
        if let Some(buttons) = self.buttons {
            declared.extend(buttons.pins().map(|p| ("button", p)));
        }
        if let Some(encoder) = self.encoder {
            declared.extend(encoder.pins().map(|p| ("encoder", p)));
        }
        let reserved = self.reserved_lines();
        let mut seen: Vec<u32> = Vec::new();
        for (role, pin) in declared {
            if seen.contains(&pin) {
                return Some(format!("{role} gpio {pin} is declared twice"));
            }
            if reserved.contains(&pin) {
                return Some(format!("{role} gpio {pin} is a reserved line"));
            }
            seen.push(pin);
        }
        None
    }
}

fn unknown() -> String {
    "unknown".to_string()
}
fn default_arch() -> String {
    "aarch64".to_string()
}
fn default_tier_field() -> i64 {
    2
}

mod embedded;
mod resolve;

pub use embedded::{declared_display_ids, load_profiles, parse_profile, BOARD_PROFILE_YAML};
#[cfg(test)]
use resolve::from_profile;
pub use resolve::{detect_tier, match_profile, resolve, resolve_profile, select_variant};

/// The board fingerprint document.
///
/// Field names and types are the Python `BoardInfo.to_dict()` contract verbatim,
/// plus `version`. The two derived booleans are materialised (not left for the
/// reader to compute) because the Python document carried them and the readers
/// key on them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardFingerprint {
    pub version: u16,
    pub name: String,
    pub model: String,
    /// The board-profile YAML filename stem (`cm4`, `rock-5c-lite`): the
    /// canonical token plugin `supported_boards` match against. Empty when no
    /// profile resolved.
    #[serde(default)]
    pub profile: String,
    pub tier: i64,
    pub ram_mb: i64,
    pub cpu_cores: i64,
    pub vendor: String,
    pub soc: String,
    pub arch: String,
    pub hw_video_codecs: Vec<String>,
    pub npu_tops: f64,
    pub has_accelerator: bool,
    pub local_inference: String,
    pub has_local_inference: bool,
    /// Panel button pins from the profile's optional `buttons:` block.
    #[serde(default)]
    pub buttons: Option<ButtonPins>,
    /// Rotary-encoder pins from the profile's optional `encoder:` block.
    #[serde(default)]
    pub encoder: Option<EncoderPins>,
    /// The USB sensor hub: present when the board declares one or one is
    /// attached; omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensor_hub: Option<SensorHubPresence>,
    /// The profile's battery gauge binding, copied verbatim; omitted when the
    /// board declares none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_gauge: Option<BatteryGaugeBinding>,
    /// GPIO numbers the profile binds to a bus, a console or a sensor line
    /// (`reserved_gpio` plus the GNSS PPS line). The button reader refuses a
    /// button on one of them. Omitted when the board reserves none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reserved_gpio: Vec<u32>,
}

/// The host facts detection needs, injected so the whole pipeline is testable
/// without a device tree.
#[derive(Debug, Clone, Default)]
pub struct HostFacts {
    /// `/proc/device-tree/model`.
    pub model: String,
    /// The FIRST (most specific) token of `/proc/device-tree/compatible`.
    pub compatible: String,
    /// The `Hardware:` / `model:` line from `/proc/cpuinfo`.
    pub cpuinfo_model: String,
    pub ram_mb: i64,
    pub cpu_cores: i64,
    /// `uname -m`, for the unknown-board fallback name.
    pub machine: String,
    /// The operating-system id (`std::env::consts::OS`), so a dev host is
    /// labelled the same way the Python detector labels it.
    pub os: String,
    /// `/etc/ados/board_override`: the board-profile YAML filename STEM
    /// (`cubie-a7s`, `rock-5c-lite`). Empty when unset.
    pub board_override: String,
    /// The USB-backed `ttyACM*` nodes, for sensor-hub presence.
    pub usb_ttys: Vec<UsbTty>,
}

/// Which profile a resolution matched, and the model string to record with it.
#[derive(Debug, Clone)]
pub struct ProfileMatch<'a> {
    pub profile: &'a BoardProfile,
    pub model: String,
    /// How it matched: `override` | `compatible` | `model` | `cpuinfo`.
    pub source: &'static str,
}

/// Write the fingerprint atomically (tmp + rename), creating the run dir.
///
/// Best-effort by contract: a run dir that is not yet writable returns the error
/// for the caller to log, never blocks startup — the value self-heals on the
/// next call.
pub fn write_sidecar(path: &Path, fp: &BoardFingerprint) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec(fp).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read the published fingerprint back. `None` when the sidecar is absent (a
/// node that has not probed yet) or does not parse as this contract, so a reader
/// reports "unknown board" rather than a half-read one.
pub fn read_sidecar(path: &Path) -> Option<BoardFingerprint> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Probe the host, resolve the board, and publish the sidecar. The one call a
/// daemon makes at startup; returns the fingerprint it wrote.
pub fn publish() -> std::io::Result<BoardFingerprint> {
    let facts = probe_host_facts();
    let fp = resolve(&load_profiles(), &facts);
    write_sidecar(&sidecar_path(), &fp)?;
    let hub_present = crate::sensor_hub::publish_presence(fp.sensor_hub.as_ref());
    tracing::info!(
        board = %fp.name,
        tier = fp.tier,
        npu_tops = fp.npu_tops,
        ram_mb = fp.ram_mb,
        sensor_hub = hub_present,
        "board fingerprint published"
    );
    Ok(fp)
}

#[cfg(test)]
#[path = "board_sidecar_bindings_tests.rs"]
mod bindings_tests;

#[cfg(test)]
mod tests;
