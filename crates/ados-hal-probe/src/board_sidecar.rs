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
    /// The profile's YAML filename stem (`cubie-a7s`, `rock-5c-lite`). Not a
    /// YAML key: [`load_profiles`] stamps it from the embedded file name, and it
    /// is the ONE canonical token `/etc/ados/board_override` accepts.
    #[serde(skip)]
    pub stem: String,
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
            Ok(mut p) => {
                // The filename stem is the canonical board-override token; it
                // is not a YAML key, so it is stamped here.
                p.stem = (*stem).to_string();
                out.push(p);
            }
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
    /// The operating-system id (`std::env::consts::OS`), so a dev host is
    /// labelled the same way the Python detector labels it.
    pub os: String,
    /// `/etc/ados/board_override`: the board-profile YAML filename STEM
    /// (`cubie-a7s`, `rock-5c-lite`). Empty when unset.
    pub board_override: String,
}

/// Which profile a resolution matched, and the model string to record with it.
#[derive(Debug, Clone)]
pub struct ProfileMatch<'a> {
    pub profile: &'a BoardProfile,
    pub model: String,
    /// How it matched: `override` | `compatible` | `model` | `cpuinfo`.
    pub source: &'static str,
}

/// The variant whose `when` holds for these facts, if any (pure).
///
/// An empty `when` never selects — a catch-all variant would rename every unit
/// of the board silently.
pub fn select_variant(profile: &BoardProfile, cpu_cores: i64) -> Option<&BoardVariant> {
    profile.variants.iter().find(|v| match v.when.cpu_cores {
        Some(n) => n == cpu_cores,
        None => false,
    })
}

/// Resolve the board profile for these facts, mirroring the Python
/// `_resolve_profile_match` exactly.
///
/// Order: the operator's `/etc/ados/board_override` stem, then the device-tree
/// `compatible` first token (which uniquely identifies a board), then the model
/// string (which can be a generic SoC-family name shared by several boards),
/// then `/proc/cpuinfo`.
///
/// An override that names no profile falls THROUGH to auto-detection with a
/// warning rather than minting a profile-less board: the override exists to
/// correct a mis-detection, and a typo must not cost the node its UART
/// candidates and its perception tier.
pub fn resolve_profile<'a>(
    profiles: &'a [BoardProfile],
    facts: &HostFacts,
) -> Option<ProfileMatch<'a>> {
    let detected = [
        facts.model.as_str(),
        facts.compatible.as_str(),
        facts.cpuinfo_model.as_str(),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or("");

    let token = facts.board_override.trim();
    if !token.is_empty() {
        match profiles
            .iter()
            .find(|p| p.stem.eq_ignore_ascii_case(token))
        {
            Some(p) => {
                return Some(ProfileMatch {
                    profile: p,
                    model: if detected.is_empty() {
                        token.to_string()
                    } else {
                        detected.to_string()
                    },
                    source: "override",
                })
            }
            None => tracing::warn!(
                token,
                "board_override names no board profile (expected a YAML filename stem, e.g. rock-5c-lite); auto-detecting"
            ),
        }
    }

    if let Some(p) = match_profile(profiles, &facts.compatible) {
        return Some(ProfileMatch {
            profile: p,
            model: if facts.model.is_empty() {
                facts.compatible.clone()
            } else {
                facts.model.clone()
            },
            source: "compatible",
        });
    }
    if let Some(p) = match_profile(profiles, &facts.model) {
        return Some(ProfileMatch {
            profile: p,
            model: facts.model.clone(),
            source: "model",
        });
    }
    if let Some(p) = match_profile(profiles, &facts.cpuinfo_model) {
        return Some(ProfileMatch {
            profile: p,
            model: facts.cpuinfo_model.clone(),
            source: "cpuinfo",
        });
    }
    None
}

/// Build the fingerprint from a matched profile, keeping the detected model
/// string and applying the matching same-device-tree variant. Mirrors the
/// Python `_board_from_profile`.
fn from_profile(profile: &BoardProfile, model_string: &str, facts: &HostFacts) -> BoardFingerprint {
    let local_inference = profile
        .compute
        .local_inference
        .clone()
        .unwrap_or_else(|| "none".to_string());
    let variant = select_variant(profile, facts.cpu_cores);
    let name = variant
        .and_then(|v| v.name.clone())
        .unwrap_or_else(|| profile.name.clone());
    let soc = variant
        .and_then(|v| v.soc.clone())
        .unwrap_or_else(|| profile.soc.clone());
    let tier = variant
        .and_then(|v| v.default_tier)
        .unwrap_or(profile.default_tier);
    BoardFingerprint {
        version: BOARD_SIDECAR_VERSION,
        model: if model_string.is_empty() {
            name.clone()
        } else {
            model_string.to_string()
        },
        name,
        tier,
        ram_mb: facts.ram_mb,
        cpu_cores: facts.cpu_cores,
        vendor: profile.vendor.clone(),
        soc,
        arch: profile.arch.clone(),
        hw_video_codecs: profile.hw_video_codecs.clone(),
        npu_tops: profile.compute.npu_tops,
        has_accelerator: profile.compute.npu_tops > 0.0,
        has_local_inference: !matches!(local_inference.as_str(), "" | "none"),
        local_inference,
    }
}

/// The generic profile stem for an architecture, and the name an unmatched
/// board is published under. Kept together because the Python half resolves the
/// identical pair — the two used to disagree (Rust resolved through
/// `generic-arm64`, Python minted a bare `generic-aarch64` with no profile and
/// therefore no fallback UART candidates).
fn generic_identity(facts: &HostFacts) -> (&'static str, String) {
    let stem = if facts.machine == "x86_64" || facts.machine == "AMD64" {
        "generic-x86_64"
    } else {
        "generic-arm64"
    };
    let name = if facts.os == "macos" {
        // A dev Mac is not a board; say so rather than claiming an SBC profile.
        "macOS (dev)".to_string()
    } else {
        stem.to_string()
    };
    (stem, name)
}

/// Resolve the board fingerprint from host facts.
///
/// Detection order is [`resolve_profile`]'s. An unmatched board resolves
/// through the generic profile for its architecture so it keeps that profile's
/// declarations (the fallback UART candidates, the camera defaults) instead of
/// nothing, and is tiered from the RAM it actually has.
pub fn resolve(profiles: &[BoardProfile], facts: &HostFacts) -> BoardFingerprint {
    if let Some(m) = resolve_profile(profiles, facts) {
        return from_profile(m.profile, &m.model, facts);
    }

    let (generic_stem, generic_name) = generic_identity(facts);
    let detected_model = [
        facts.model.as_str(),
        facts.cpuinfo_model.as_str(),
        facts.compatible.as_str(),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .unwrap_or("")
    .to_string();
    if let Some(p) = profiles.iter().find(|p| p.stem == generic_stem) {
        let mut fp = from_profile(p, &detected_model, facts);
        fp.name = generic_name;
        if fp.model.is_empty() {
            fp.model = fp.name.clone();
        }
        // The generic profile's default_tier is a placeholder; an unknown board
        // is tiered from what it actually has.
        fp.tier = detect_tier(facts.ram_mb);
        return fp;
    }
    BoardFingerprint {
        version: BOARD_SIDECAR_VERSION,
        model: if detected_model.is_empty() {
            generic_name.clone()
        } else {
            detected_model
        },
        name: generic_name,
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

/// The display ids the resolved board declares, for validating an operator's
/// `--display <id>`. Empty when the board is unknown or declares none, which a
/// caller must treat as "cannot validate", never as "invalid".
pub fn declared_display_ids() -> Vec<String> {
    let profiles = load_profiles();
    let facts = probe_host_facts();
    match resolve_profile(&profiles, &facts) {
        Some(m) => m
            .profile
            .displays
            .supported
            .iter()
            .map(|d| d.id.clone())
            .collect(),
        None => Vec::new(),
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
        os: std::env::consts::OS.to_string(),
        board_override: read_board_override(),
    }
}

/// The path of the board-override file, honouring `ADOS_ETC_DIR` like the
/// Python `ados.setup.advanced` writer and the install scripts.
pub fn board_override_path() -> std::path::PathBuf {
    match std::env::var_os("ADOS_ETC_DIR") {
        Some(dir) => std::path::PathBuf::from(dir).join("board_override"),
        None => std::path::PathBuf::from("/etc/ados/board_override"),
    }
}

/// The operator's forced board — a board-profile YAML filename STEM — or `""`.
///
/// This writer used to ignore the override entirely, so a correctly-spelled
/// override reached the Python detector and never reached `/run/ados/board.json`
/// (and therefore never reached `/api/status` or Mission Control): the two
/// halves reported different boards on the same node.
pub fn read_board_override() -> String {
    std::fs::read_to_string(board_override_path())
        .map(|s| s.trim().trim_matches('\0').trim().to_string())
        .unwrap_or_default()
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
            os: "linux".to_string(),
            board_override: String::new(),
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

        // With NO compatible token the shared SoC-family string identifies
        // nothing: it must resolve to neither board rather than to whichever
        // profile happened to list it (the A7Z listed `sun60iw2`, which is the
        // A7S's own verified on-rig model string, so a real A7S came up with
        // the A7Z's FC UART and a 40-pin GPIO map it does not have).
        let ambiguous = resolve(&profiles, &facts("", "sun60iw2"));
        assert_eq!(ambiguous.name, "generic-arm64", "{ambiguous:?}");
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

    /// The board-override grammar: the YAML filename STEM, and nothing else.
    ///
    /// Asserted over EVERY embedded board so a new profile cannot be added with
    /// an unreachable override. The Python half asserts the same property over
    /// the same directory (`tests/test_hal.py::TestBoardOverride`), which is
    /// what makes the two halves resolve one token to one board.
    #[test]
    fn every_board_stem_is_a_resolvable_override_token() {
        let profiles = load_profiles();
        for (stem, _) in BOARD_PROFILE_YAML {
            let mut f = facts("", "");
            f.board_override = (*stem).to_string();
            let fp = resolve(&profiles, &f);
            let expected = profiles
                .iter()
                .find(|p| &p.stem == stem)
                .expect("every embedded stem parses");
            // The variant-aware name for these facts, so the 8-core Rock 5C row
            // compares against its own variant rather than the Lite's name.
            let expected_name = select_variant(expected, f.cpu_cores)
                .and_then(|v| v.name.clone())
                .unwrap_or_else(|| expected.name.clone());
            assert_eq!(fp.name, expected_name, "override {stem} resolved wrong");
        }
    }

    #[test]
    fn the_override_stem_pins_the_a7s_against_its_shared_soc_family_string() {
        // The documented escape hatch for a mis-detected board. `cubie-a7s` is
        // a legal slug (the display-name form never was), so this is the token
        // an operator and a shell script can actually write.
        let profiles = load_profiles();
        let mut f = facts("", "sun60iw2");
        f.board_override = "cubie-a7s".to_string();
        let fp = resolve(&profiles, &f);
        assert_eq!(fp.name, "Radxa Cubie A7S");
        assert_eq!(fp.soc, "Allwinner A733");
        // The real device-tree model is still recorded, not replaced by the token.
        assert_eq!(fp.model, "sun60iw2");
    }

    #[test]
    fn an_override_naming_no_profile_falls_through_to_auto_detection() {
        // A typo must not cost the node its profile: the override exists to
        // correct a mis-detection, so an unresolvable token degrades to what the
        // hardware says rather than to a profile-less phantom board.
        let profiles = load_profiles();
        let mut f = facts("raspberrypi,5-model-b", "Raspberry Pi 5");
        f.board_override = "Radxa ROCK 5C Lite (RK3582)".to_string();
        let fp = resolve(&profiles, &f);
        assert!(fp.name.contains("Raspberry Pi 5"), "fingerprint: {fp:?}");
    }

    /// The Rock 5C / 5C Lite share ONE device tree, so the core count decides.
    /// Filename order used to, which published `soc: RK3582` and a name ending
    /// in "Lite" for a full 8-core RK3588S2 board.
    #[test]
    fn the_shared_rock_5c_device_tree_is_split_by_core_count() {
        let profiles = load_profiles();
        let mut lite = facts("radxa,rock-5c", "Radxa ROCK 5C ");
        lite.cpu_cores = 6;
        let lite = resolve(&profiles, &lite);
        assert_eq!(lite.name, "Radxa ROCK 5C Lite (RK3582)");
        assert_eq!(lite.soc, "RK3582");

        let mut full = facts("radxa,rock-5c", "Radxa ROCK 5C ");
        full.cpu_cores = 8;
        let full = resolve(&profiles, &full);
        assert_eq!(full.name, "Radxa ROCK 5C (RK3588S2)");
        assert_eq!(full.soc, "RK3588S2");
        // Both bins keep the NPU and the VPU, so the perception tier is the same.
        assert_eq!(full.npu_tops, lite.npu_tops);
        assert!(full.has_accelerator);
    }

    /// No two boards may claim one device-tree string. The three collisions this
    /// guards were invisible to a byte-identical-pattern comparison because they
    /// were substring overlaps, a shared SoC-family name, and a shared DT.
    #[test]
    fn real_device_tree_strings_resolve_to_exactly_one_board() {
        let profiles = load_profiles();
        // (compatible, model, cpu_cores) -> resolved name.
        let cases: &[(&str, &str, i64, &str)] = &[
            ("radxa,cubie-a7s", "sun60iw2", 8, "Radxa Cubie A7S"),
            ("radxa,cubie-a7z", "sun60iw2", 8, "Radxa Cubie A7Z"),
            (
                "radxa,rock-5c",
                "Radxa ROCK 5C ",
                6,
                "Radxa ROCK 5C Lite (RK3582)",
            ),
            (
                "radxa,rock-5c",
                "Radxa ROCK 5C ",
                8,
                "Radxa ROCK 5C (RK3588S2)",
            ),
            ("radxa,cm3", "Radxa CM3 IO Board", 4, "Radxa CM3 (RK3566)"),
            ("rockchip,rk3566", "RK3566 EVB", 4, "Radxa CM3 (RK3566)"),
            ("radxa,cm4", "Radxa CM4", 8, "Radxa CM4 (RK3588S2)"),
            (
                "raspberrypi,4-model-b",
                "Raspberry Pi 4 Model B Rev 1.4",
                4,
                "Raspberry Pi 4B",
            ),
            (
                "raspberrypi,3-model-b-plus",
                "Raspberry Pi 3 Model B Plus Rev 1.3",
                4,
                "Raspberry Pi 3",
            ),
            (
                "raspberrypi,3-compute-module",
                "Raspberry Pi Compute Module 3 Plus Rev 1.0",
                4,
                "Raspberry Pi Compute Module 3",
            ),
        ];
        for (compatible, model, cores, expected) in cases {
            let mut f = facts(compatible, model);
            f.cpu_cores = *cores;
            let fp = resolve(&profiles, &f);
            assert_eq!(
                fp.name, *expected,
                "{compatible} / {model} / {cores} cores resolved to {}",
                fp.name
            );
        }
    }

    #[test]
    fn the_rk3566_board_keeps_its_npu_and_its_third_uart() {
        // Two profiles used to claim RK3566 with different `npu_tops` (0.0 vs
        // 0.8) and different UART sets, so `has_accelerator` and the perception
        // tier flipped on identical hardware depending on which filename sorted
        // first. One profile survives, and it is the measured one.
        let profiles = load_profiles();
        let fp = resolve(&profiles, &facts("radxa,cm3", "Radxa CM3 IO Board"));
        assert_eq!(fp.npu_tops, 0.8);
        assert!(fp.has_accelerator);
        assert!(fp.hw_video_codecs.iter().any(|c| c == "vp9_dec"));
    }

    #[test]
    fn a_dev_mac_is_labelled_as_one_but_still_carries_the_generic_declarations() {
        // Parity with the Python fallback: same name, and the generic profile's
        // declarations (the fallback UART candidates) rather than nothing.
        let profiles = load_profiles();
        let mut f = facts("", "");
        f.os = "macos".to_string();
        f.machine = "aarch64".to_string();
        let fp = resolve(&profiles, &f);
        assert_eq!(fp.name, "macOS (dev)");
        assert_eq!(fp.tier, detect_tier(8192));
        assert_eq!(fp.vendor, "unknown");
    }
}
