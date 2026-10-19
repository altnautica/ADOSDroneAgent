//! The host facts board detection keys on, read from procfs/sysfs, and the
//! pure parsers of those files.

use std::path::Path;

use crate::board_sidecar::HostFacts;

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
        usb_ttys: crate::sensor_hub::usb_acm_ttys(Path::new(crate::sensor_hub::TTY_CLASS_DIR)),
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
