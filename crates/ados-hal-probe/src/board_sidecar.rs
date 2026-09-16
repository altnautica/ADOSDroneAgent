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

/// The slice of a board-profile YAML the fingerprint is built from. Every other
/// section (displays, cameras, radios, gpio, flight_controller…) is ignored
/// here; the services that need them read the YAML themselves.
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

/// The board-profile YAMLs, embedded at compile time.
///
/// Embedded rather than read from disk so the lean headless node — the one this
/// writer exists for — needs nothing but its own binary. [`BOARD_PROFILE_YAML`]
/// is asserted to cover every file in the directory, so adding a board without
/// registering it here fails a test instead of silently detecting as unknown.
pub const BOARD_PROFILE_YAML: &[(&str, &str)] = &[
    (
        "beagley-ai",
        include_str!("../../../src/ados/hal/boards/beagley-ai.yaml"),
    ),
    ("cm3", include_str!("../../../src/ados/hal/boards/cm3.yaml")),
    ("cm4", include_str!("../../../src/ados/hal/boards/cm4.yaml")),
    ("cm5", include_str!("../../../src/ados/hal/boards/cm5.yaml")),
    (
        "cubie-a7s",
        include_str!("../../../src/ados/hal/boards/cubie-a7s.yaml"),
    ),
    (
        "cubie-a7z",
        include_str!("../../../src/ados/hal/boards/cubie-a7z.yaml"),
    ),
    (
        "generic-arm64",
        include_str!("../../../src/ados/hal/boards/generic-arm64.yaml"),
    ),
    (
        "jetson-nano",
        include_str!("../../../src/ados/hal/boards/jetson-nano.yaml"),
    ),
    (
        "jetson-orin-nano",
        include_str!("../../../src/ados/hal/boards/jetson-orin-nano.yaml"),
    ),
    (
        "orange-pi-5",
        include_str!("../../../src/ados/hal/boards/orange-pi-5.yaml"),
    ),
    (
        "radxa-cm3",
        include_str!("../../../src/ados/hal/boards/radxa-cm3.yaml"),
    ),
    (
        "rdk-x3",
        include_str!("../../../src/ados/hal/boards/rdk-x3.yaml"),
    ),
    (
        "rk3566",
        include_str!("../../../src/ados/hal/boards/rk3566.yaml"),
    ),
    (
        "rk3576",
        include_str!("../../../src/ados/hal/boards/rk3576.yaml"),
    ),
    (
        "rk3588s2",
        include_str!("../../../src/ados/hal/boards/rk3588s2.yaml"),
    ),
    (
        "rock-5c-lite",
        include_str!("../../../src/ados/hal/boards/rock-5c-lite.yaml"),
    ),
    (
        "rpi3",
        include_str!("../../../src/ados/hal/boards/rpi3.yaml"),
    ),
    (
        "rpi4b",
        include_str!("../../../src/ados/hal/boards/rpi4b.yaml"),
    ),
    (
        "rpi5",
        include_str!("../../../src/ados/hal/boards/rpi5.yaml"),
    ),
    (
        "rv1126b",
        include_str!("../../../src/ados/hal/boards/rv1126b.yaml"),
    ),
    (
        "zero2w",
        include_str!("../../../src/ados/hal/boards/zero2w.yaml"),
    ),
];

/// Parse every embedded profile, in filename order — the same order the Python
/// loader walks (`sorted()` over the directory), which is what makes the
/// first-match scan in [`match_profile`] agree across the two languages.
///
/// A profile that fails to parse is skipped with a warning rather than taking
/// the whole detection down: one malformed board must not make every board
/// unknown.
pub fn load_profiles() -> Vec<BoardProfile> {
    let mut out = Vec::with_capacity(BOARD_PROFILE_YAML.len());
    for (stem, body) in BOARD_PROFILE_YAML {
        match serde_norway::from_str::<BoardProfile>(body) {
            Ok(p) => out.push(p),
            Err(e) => tracing::warn!(board = stem, error = %e, "board profile did not parse"),
        }
    }
    out
}

/// Assign a tier from available RAM. Mirrors the Python `detect_tier` exactly:
/// <512 MB = 1, <2048 = 2, <=4096 = 3, else 4.
pub fn detect_tier(ram_mb: i64) -> i64 {
    if ram_mb < 512 {
        1
    } else if ram_mb < 2048 {
        2
    } else if ram_mb <= 4096 {
        3
    } else {
        4
    }
}

/// The first profile with a pattern contained in `model_string`,
/// case-insensitively. First-match over the filename-ordered list, matching the
/// Python `_match_profile`.
pub fn match_profile<'a>(
    profiles: &'a [BoardProfile],
    model_string: &str,
) -> Option<&'a BoardProfile> {
    if model_string.is_empty() {
        return None;
    }
    let needle = model_string.to_ascii_lowercase();
    profiles.iter().find(|p| {
        p.model_patterns
            .iter()
            .any(|pattern| !pattern.is_empty() && needle.contains(&pattern.to_ascii_lowercase()))
    })
}

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
}

/// Build the fingerprint from a matched profile, keeping the detected model
/// string. Mirrors the Python `_board_from_profile`.
fn from_profile(profile: &BoardProfile, model_string: &str, facts: &HostFacts) -> BoardFingerprint {
    let local_inference = profile
        .compute
        .local_inference
        .clone()
        .unwrap_or_else(|| "none".to_string());
    BoardFingerprint {
        version: BOARD_SIDECAR_VERSION,
        name: profile.name.clone(),
        model: if model_string.is_empty() {
            profile.name.clone()
        } else {
            model_string.to_string()
        },
        tier: profile.default_tier,
        ram_mb: facts.ram_mb,
        cpu_cores: facts.cpu_cores,
        vendor: profile.vendor.clone(),
        soc: profile.soc.clone(),
        arch: profile.arch.clone(),
        hw_video_codecs: profile.hw_video_codecs.clone(),
        npu_tops: profile.compute.npu_tops,
        has_accelerator: profile.compute.npu_tops > 0.0,
        has_local_inference: !matches!(local_inference.as_str(), "" | "none"),
        local_inference,
    }
}

/// Resolve the board fingerprint from host facts.
///
/// Detection order matches the Python pipeline exactly, and for the same
/// reasons: the device-tree `compatible` first token uniquely identifies a
/// board, while the model string can be a generic SoC-family name shared by
/// several boards (every Allwinner A733 board reports `sun60iw2`, so only the
/// compatible token tells the Cubie A7S from the A7Z). `/proc/cpuinfo` is the
/// fallback, and an unmatched board falls back to the generic profile for its
/// architecture so it keeps that profile's declarations instead of nothing.
pub fn resolve(profiles: &[BoardProfile], facts: &HostFacts) -> BoardFingerprint {
    if let Some(p) = match_profile(profiles, &facts.compatible) {
        return from_profile(
            p,
            if facts.model.is_empty() {
                &facts.compatible
            } else {
                &facts.model
            },
            facts,
        );
    }
    if let Some(p) = match_profile(profiles, &facts.model) {
        return from_profile(p, &facts.model, facts);
    }
    if let Some(p) = match_profile(profiles, &facts.cpuinfo_model) {
        return from_profile(p, &facts.cpuinfo_model, facts);
    }

    // Unmatched. Resolve through the generic profile for this architecture
    // rather than minting a bare record: the generic profile exists to carry
    // the fallback UART candidates and camera defaults, and a board with no
    // profile at all is worse off on exactly the hardware that needs a fallback
    // most.
    let generic_name = if facts.machine == "x86_64" || facts.machine == "AMD64" {
        "generic-x86_64"
    } else {
        "generic-arm64"
    };
    let detected_model = [
        facts.model.as_str(),
        facts.cpuinfo_model.as_str(),
        facts.compatible.as_str(),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or("")
    .to_string();
    if let Some(p) = profiles.iter().find(|p| p.name == generic_name) {
        let mut fp = from_profile(p, &detected_model, facts);
        // The generic profile's default_tier is a placeholder; an unknown board
        // is tiered from what it actually has.
        fp.tier = detect_tier(facts.ram_mb);
        return fp;
    }
    BoardFingerprint {
        version: BOARD_SIDECAR_VERSION,
        name: generic_name.to_string(),
        model: detected_model,
        tier: detect_tier(facts.ram_mb),
        ram_mb: facts.ram_mb,
        cpu_cores: facts.cpu_cores,
        vendor: "unknown".to_string(),
        soc: "unknown".to_string(),
        arch: if facts.machine.is_empty() {
            "aarch64".to_string()
        } else {
            facts.machine.clone()
        },
        hw_video_codecs: Vec::new(),
        npu_tops: 0.0,
        has_accelerator: false,
        local_inference: "none".to_string(),
        has_local_inference: false,
    }
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

/// Probe the host, resolve the board, and publish the sidecar. The one call a
/// daemon makes at startup; returns the fingerprint it wrote.
pub fn publish() -> std::io::Result<BoardFingerprint> {
    let facts = probe_host_facts();
    let fp = resolve(&load_profiles(), &facts);
    write_sidecar(&sidecar_path(), &fp)?;
    tracing::info!(
        board = %fp.name,
        tier = fp.tier,
        npu_tops = fp.npu_tops,
        ram_mb = fp.ram_mb,
        "board fingerprint published"
    );
    Ok(fp)
}

/// Read the host facts detection keys on. Linux reads procfs/sysfs; elsewhere
/// the fields stay empty and detection lands on the generic fallback, which is
/// the honest answer on a dev host.
pub fn probe_host_facts() -> HostFacts {
    HostFacts {
        model: read_device_tree_model(),
        compatible: read_device_tree_compatible(),
        cpuinfo_model: read_cpuinfo_model(),
        ram_mb: read_ram_mb(),
        cpu_cores: read_cpu_cores(),
        machine: read_machine(),
    }
}

fn read_device_tree_model() -> String {
    std::fs::read("/proc/device-tree/model")
        .map(|b| {
            String::from_utf8_lossy(&b)
                .trim_end_matches('\0')
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

/// The FIRST token of the NUL-separated `compatible` list: the most specific
/// one, which uniquely identifies the board. Generic SoC-family tokens later in
/// the list must never enter pattern matching.
fn read_device_tree_compatible() -> String {
    let Ok(bytes) = std::fs::read("/proc/device-tree/compatible") else {
        return String::new();
    };
    bytes
        .split(|b| *b == 0)
        .map(|t| String::from_utf8_lossy(t).trim().to_string())
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}

fn read_cpuinfo_model() -> String {
    let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") else {
        return String::new();
    };
    parse_cpuinfo_model(&text)
}

/// The `Hardware:` / `model:` value from a `/proc/cpuinfo` body. Pure.
pub fn parse_cpuinfo_model(text: &str) -> String {
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("hardware") || lower.starts_with("model") {
            if let Some((_, value)) = line.split_once(':') {
                let value = value.trim();
                if !value.is_empty() {
                    return value.to_string();
                }
            }
        }
    }
    String::new()
}

fn read_ram_mb() -> i64 {
    std::fs::read_to_string("/proc/meminfo")
        .map(|t| parse_mem_total_mb(&t))
        .unwrap_or(0)
}

/// MemTotal in MiB from a `/proc/meminfo` body. Pure.
pub fn parse_mem_total_mb(text: &str) -> i64 {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: i64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            return kb / 1024;
        }
    }
    0
}

fn read_cpu_cores() -> i64 {
    // The online-CPU list is the authority; `/proc/cpuinfo` processor lines are
    // the fallback on a kernel that does not expose it.
    if let Ok(text) = std::fs::read_to_string("/sys/devices/system/cpu/online") {
        let n = parse_cpu_online(&text);
        if n > 0 {
            return n;
        }
    }
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|t| {
            t.lines()
                .filter(|l| l.to_ascii_lowercase().starts_with("processor"))
                .count() as i64
        })
        .unwrap_or(0)
}

/// Count the CPUs in a `/sys/devices/system/cpu/online` range list
/// (`0-3`, `0,2-3`). Pure.
pub fn parse_cpu_online(text: &str) -> i64 {
    let mut total = 0i64;
    for part in text.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((lo, hi)) => {
                if let (Ok(lo), Ok(hi)) = (lo.trim().parse::<i64>(), hi.trim().parse::<i64>()) {
                    if hi >= lo {
                        total += hi - lo + 1;
                    }
                }
            }
            None => {
                if part.parse::<i64>().is_ok() {
                    total += 1;
                }
            }
        }
    }
    total
}

#[cfg(target_os = "linux")]
fn read_machine() -> String {
    std::fs::read_to_string("/proc/sys/kernel/arch")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "aarch64".to_string())
}

#[cfg(not(target_os = "linux"))]
fn read_machine() -> String {
    std::env::consts::ARCH.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(compatible: &str, model: &str) -> HostFacts {
        HostFacts {
            model: model.to_string(),
            compatible: compatible.to_string(),
            cpuinfo_model: String::new(),
            ram_mb: 8192,
            cpu_cores: 8,
            machine: "aarch64".to_string(),
        }
    }

    #[test]
    fn every_board_profile_yaml_is_registered_and_parses() {
        // The drift guard: a board added to the directory but not to the
        // embedded table would detect as unknown on a Rust-only node, which is
        // exactly the defect this writer exists to fix.
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src/ados/hal/boards");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("the board profile directory must exist")
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_suffix(".yaml").map(str::to_string)
            })
            .collect();
        on_disk.sort();
        let mut embedded: Vec<String> = BOARD_PROFILE_YAML
            .iter()
            .map(|(stem, _)| stem.to_string())
            .collect();
        embedded.sort();
        assert_eq!(
            embedded, on_disk,
            "the embedded board table and src/ados/hal/boards/ have drifted"
        );

        // And every one of them parses into the fingerprint's view of a profile.
        assert_eq!(
            load_profiles().len(),
            BOARD_PROFILE_YAML.len(),
            "a board profile failed to parse"
        );
    }

    #[test]
    fn the_sidecar_version_matches_the_shared_contract_registry() {
        assert_eq!(
            BOARD_SIDECAR_VERSION,
            ados_protocol::contracts::sidecar_version("board").unwrap()
        );
    }

    #[test]
    fn an_npu_board_reports_its_real_tops_and_accelerator_flag() {
        // The offload reconciler keys on this: `npu_tops: 0` made an NPU board
        // offload detection it could run onboard.
        let profiles = load_profiles();
        let fp = resolve(&profiles, &facts("rockchip,rk3588s2", "Rockchip RK3588S2"));
        assert!(fp.npu_tops > 0.0, "fingerprint: {fp:?}");
        assert!(fp.has_accelerator);
        assert_ne!(fp.name, "generic-arm64");
        assert_eq!(fp.version, BOARD_SIDECAR_VERSION);
    }

    #[test]
    fn the_compatible_token_disambiguates_two_boards_with_one_soc_model() {
        // Every Allwinner A733 board reports the same model string, so only the
        // device-tree compatible token tells the A7S from the A7Z. Matching on
        // the model alone would report the wrong board's tier and NPU.
        let profiles = load_profiles();
        let a7z = resolve(&profiles, &facts("radxa,cubie-a7z", "sun60iw2"));
        let a7s = resolve(&profiles, &facts("radxa,cubie-a7s", "sun60iw2"));
        assert_eq!(a7z.name, "Radxa Cubie A7Z");
        assert_eq!(a7s.name, "Radxa Cubie A7S");
        assert_ne!(a7z.tier, a7s.tier);
        // The detected model string is preserved, not replaced by the profile.
        assert_eq!(a7z.model, "sun60iw2");
    }

    #[test]
    fn an_unknown_board_falls_back_to_the_generic_profile_not_to_nothing() {
        let profiles = load_profiles();
        let fp = resolve(&profiles, &facts("acme,unheard-of-v9", "ACME Unheard Of"));
        assert_eq!(fp.name, "generic-arm64");
        assert_eq!(fp.model, "ACME Unheard Of");
        // Tiered from what it actually has, not from the generic placeholder.
        assert_eq!(fp.tier, detect_tier(8192));
        assert_eq!(fp.npu_tops, 0.0);
        assert!(!fp.has_accelerator);
    }

    #[test]
    fn the_cpuinfo_fallback_is_used_when_the_device_tree_says_nothing() {
        let profiles = load_profiles();
        let mut f = facts("", "");
        f.cpuinfo_model = "Raspberry Pi 4 Model B Rev 1.4".to_string();
        let fp = resolve(&profiles, &f);
        assert!(fp.name.contains("Raspberry Pi 4"), "fingerprint: {fp:?}");
    }

    #[test]
    fn tiers_follow_the_ram_bands() {
        assert_eq!(detect_tier(256), 1);
        assert_eq!(detect_tier(1024), 2);
        assert_eq!(detect_tier(4096), 3);
        assert_eq!(detect_tier(8192), 4);
    }

    #[test]
    fn the_document_carries_the_python_writer_key_set_exactly() {
        // The two writers must agree by construction: this is the contract the
        // Rust readers (pairing route, status route, offload reconciler) and any
        // remaining Python reader share.
        let profiles = load_profiles();
        let fp = resolve(&profiles, &facts("raspberrypi,5-model-b", "Raspberry Pi 5"));
        let value = serde_json::to_value(&fp).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "arch",
                "cpu_cores",
                "has_accelerator",
                "has_local_inference",
                "hw_video_codecs",
                "local_inference",
                "model",
                "name",
                "npu_tops",
                "ram_mb",
                "soc",
                "tier",
                "vendor",
                "version",
            ]
        );
    }

    #[test]
    fn the_sidecar_is_written_atomically_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run").join("board.json");
        let profiles = load_profiles();
        let fp = resolve(&profiles, &facts("raspberrypi,5-model-b", "Raspberry Pi 5"));
        write_sidecar(&path, &fp).unwrap();

        let back: BoardFingerprint =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, fp);
        // No tmp file left behind.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_local_inference_board_reports_the_cpu_path() {
        // cm5 declares `local_inference: onnx` and no NPU: the perception tier
        // must read a local path, not an offload one.
        let profiles = load_profiles();
        let cm5 = profiles
            .iter()
            .find(|p| p.name == "Raspberry Pi CM5")
            .expect("cm5 profile");
        let fp = from_profile(cm5, "Raspberry Pi CM5", &facts("", ""));
        assert_eq!(fp.local_inference, "onnx");
        assert!(fp.has_local_inference);
        assert!(!fp.has_accelerator);
    }

    #[test]
    fn host_fact_parsers_read_the_real_shapes() {
        assert_eq!(parse_mem_total_mb("MemTotal:        8123456 kB\n"), 7933);
        assert_eq!(parse_mem_total_mb("SwapTotal: 0 kB\n"), 0);
        assert_eq!(parse_cpu_online("0-7\n"), 8);
        assert_eq!(parse_cpu_online("0,2-3\n"), 3);
        assert_eq!(parse_cpu_online("\n"), 0);
        assert_eq!(
            parse_cpuinfo_model("processor\t: 0\nHardware\t: BCM2835\n"),
            "BCM2835"
        );
        assert_eq!(parse_cpuinfo_model("flags : none\n"), "");
    }
}
