//! Local host-hardware collector for the non-SBC (workstation / macOS) path.
//!
//! On an SBC the durable logging daemon (`ados-logd`) samples CPU / memory / disk
//! into its store and the status routes read those merged signals. On a
//! workstation host (a Mac, a dev box) `ados-logd` is not the running collector,
//! so `latest_hw_signals()` is `None` and the status surfaces would report zeros.
//! This module fills that gap with a cross-platform `sysinfo` read, shaped as the
//! same `logd` signal map (`mem.total_bytes`, `cpu.util.all`, …) so the existing
//! `derive_system` / `derive_health` mappers consume it unchanged. The board
//! block is derived the same way when no HAL board sidecar is present.
//!
//! Honest by construction (Rule 44): a signal the host cannot supply is omitted
//! (so the mapper degrades it to its documented default) and a board field that
//! is unknown is `null` — never faked. There is no portable thermal source here,
//! so no `thermal.*` signal is emitted and the temperature reads `null`.

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Map, Value};
use sysinfo::{Disks, System};

const BYTES_PER_MB: f64 = 1024.0 * 1024.0;

/// One process-global `System`, kept alive so CPU utilisation is computed as the
/// delta between successive status reads: `sysinfo` reports usage relative to the
/// previous refresh, so persisting the instance means each read measures the
/// interval since the last poll. Guarded by a `Mutex` — the refresh + read is
/// fast and never held across an `.await`. The first read after process start has
/// only the priming refresh to diff against (a near-zero window), so it can read
/// low; it self-corrects on the next poll. That is a real measurement over a
/// short window, not a fabricated value.
fn shared_system() -> &'static Mutex<System> {
    static SYS: OnceLock<Mutex<System>> = OnceLock::new();
    SYS.get_or_init(|| {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Mutex::new(sys)
    })
}

/// Collect the host hardware signals as a `logd`-compatible signal map, so the
/// existing `derive_system` / `derive_health` mappers produce their canonical
/// shapes from a workstation host with no logging daemon. Emits the five
/// essential signals (memory total + available, aggregate CPU, filesystem total +
/// used) plus swap when available; omits thermal (no portable source → the
/// mappers leave temperature `null`).
pub fn collect_signals() -> Map<String, Value> {
    let mut signals = Map::new();
    if let Ok(mut sys) = shared_system().lock() {
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        signals.insert("cpu.util.all".into(), json!(sys.global_cpu_usage() as f64));
        signals.insert("mem.total_bytes".into(), json!(sys.total_memory()));
        signals.insert("mem.avail_bytes".into(), json!(sys.available_memory()));
        signals.insert("mem.swap_total_bytes".into(), json!(sys.total_swap()));
        signals.insert("mem.swap_free_bytes".into(), json!(sys.free_swap()));
    }
    if let Some((total, used)) = root_disk_bytes() {
        signals.insert("disk.fs_total_bytes".into(), json!(total));
        signals.insert("disk.fs_used_bytes".into(), json!(used));
    }
    signals
}

/// Total + used bytes of the root filesystem. Prefers the volume mounted at `/`;
/// when there is no exact `/` mount it falls back to the largest volume (the boot
/// disk). `None` when no usable disk is enumerated, so the caller omits the disk
/// signals rather than reporting zero capacity.
fn root_disk_bytes() -> Option<(u64, u64)> {
    let disks = Disks::new_with_refreshed_list();
    let root = std::path::Path::new("/");
    let list = disks.list();
    let chosen = list
        .iter()
        .find(|d| d.mount_point() == root)
        .or_else(|| list.iter().max_by_key(|d| d.total_space()))?;
    let total = chosen.total_space();
    if total == 0 {
        return None;
    }
    let avail = chosen.available_space();
    Some((total, total.saturating_sub(avail)))
}

/// The logical CPU count for the `/api/system` `cpu_count` field on a host with
/// no `/proc/cpuinfo` (the macOS / workstation fallback). The online logical core
/// count via `available_parallelism`; `null` when indeterminate (the same null
/// `os.cpu_count()` can return).
pub fn cpu_count_fallback() -> Value {
    match std::thread::available_parallelism() {
        Ok(n) => json!(n.get()),
        Err(_) => Value::Null,
    }
}

/// The host board block, derived from the host when no HAL board sidecar is
/// present (a workstation / Mac, where no detector runs). Mirrors the SBC board
/// dict keys the GCS reads: `name`, `model`, `arch`, `soc`, `vendor`, `ram_mb`,
/// `cpu_cores`. No `tier` — that classifies an SBC capability tier, not a
/// workstation host. The identity is static for the process, so it is resolved
/// once and cached. Unknown fields are `null` (Rule 44).
pub fn host_board() -> Value {
    static BOARD: OnceLock<Value> = OnceLock::new();
    BOARD.get_or_init(probe_host_board).clone()
}

/// Resolve the host board dict once. `arch` is normalised to the SBC vocabulary
/// (`aarch64` → `arm64`); `ram_mb` + `cpu_cores` are cross-platform; `model` /
/// `soc` / `vendor` come from macOS `sysctl` first, degrading to `sysinfo`'s CPU
/// brand/vendor off macOS, and to `null` when unknown. `name` carries the product
/// model id (never the hostname — that can be personal).
fn probe_host_board() -> Value {
    let arch = normalize_arch(std::env::consts::ARCH);
    let cpu_cores = std::thread::available_parallelism()
        .map(|n| n.get() as u64)
        .ok();
    let ram_mb = total_memory_mb();
    let model = host_model();
    let soc = host_soc();
    let vendor = host_vendor(soc.as_deref());
    let name = model.clone();
    json!({
        "name": name,
        "model": model,
        "arch": arch,
        "soc": soc,
        "vendor": vendor,
        "ram_mb": ram_mb,
        "cpu_cores": cpu_cores,
    })
}

/// Total physical memory in MB, or `None` when indeterminate.
fn total_memory_mb() -> Option<u64> {
    let mut sys = System::new();
    sys.refresh_memory();
    let bytes = sys.total_memory();
    if bytes == 0 {
        None
    } else {
        Some(((bytes as f64) / BYTES_PER_MB).round() as u64)
    }
}

/// The board model identifier from macOS `hw.model` (a `Mac<family>,<variant>`
/// style string), else `None`.
fn host_model() -> Option<String> {
    sysctl_string("hw.model")
}

/// The SoC / CPU descriptor: macOS `machdep.cpu.brand_string` (e.g.
/// `Apple M1 Pro`), else the `sysinfo` CPU brand, else `None`.
fn host_soc() -> Option<String> {
    if let Some(s) = sysctl_string("machdep.cpu.brand_string") {
        return Some(s);
    }
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.cpus()
        .first()
        .map(|c| c.brand().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The machine vendor: `Apple` on macOS (the box manufacturer regardless of
/// silicon), else inferred from the SoC string or the `sysinfo` CPU vendor id,
/// else `None`.
fn host_vendor(soc: Option<&str>) -> Option<String> {
    if cfg!(target_os = "macos") {
        return Some("Apple".to_string());
    }
    match soc {
        Some(s) if s.contains("Intel") => Some("Intel".to_string()),
        Some(s) if s.contains("AMD") => Some("AMD".to_string()),
        _ => {
            let mut sys = System::new();
            sys.refresh_cpu_all();
            sys.cpus()
                .first()
                .map(|c| c.vendor_id().trim().to_string())
                .filter(|s| !s.is_empty())
        }
    }
}

/// Read a `sysctl -n <key>` string on macOS, or `None` off macOS / on any error /
/// empty output.
#[cfg(target_os = "macos")]
fn sysctl_string(key: &str) -> Option<String> {
    let out = std::process::Command::new("sysctl")
        .arg("-n")
        .arg(key)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(not(target_os = "macos"))]
fn sysctl_string(_key: &str) -> Option<String> {
    None
}

/// Normalise `std::env::consts::ARCH` to the SBC board vocabulary the GCS reads:
/// `aarch64` → `arm64`; every other arch passes through unchanged.
fn normalize_arch(arch: &str) -> String {
    match arch {
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_signals_carries_the_essential_resource_keys() {
        // On any real test host sysinfo resolves CPU + memory + a root disk, so the
        // five essentials the resource mapper requires are present and numeric.
        let s = collect_signals();
        for key in [
            "cpu.util.all",
            "mem.total_bytes",
            "mem.avail_bytes",
            "disk.fs_total_bytes",
            "disk.fs_used_bytes",
        ] {
            let v = s.get(key).unwrap_or_else(|| panic!("{key} present"));
            assert!(v.is_number(), "{key} is numeric");
        }
        // Total memory is a positive count, available never exceeds total.
        let total = s["mem.total_bytes"].as_f64().unwrap();
        let avail = s["mem.avail_bytes"].as_f64().unwrap();
        assert!(total > 0.0);
        assert!(avail <= total);
    }

    #[test]
    fn host_board_has_the_sbc_dict_keys_and_no_tier() {
        let b = host_board();
        let obj = b.as_object().expect("board is an object");
        for key in [
            "name",
            "model",
            "arch",
            "soc",
            "vendor",
            "ram_mb",
            "cpu_cores",
        ] {
            assert!(obj.contains_key(key), "{key} present");
        }
        // A workstation host carries no SBC capability tier.
        assert!(!obj.contains_key("tier"));
        // arch + cpu_cores + ram_mb are always resolvable on a test host.
        assert!(b["arch"].is_string());
        assert!(b["cpu_cores"].as_u64().map(|n| n > 0).unwrap_or(false));
        assert!(b["ram_mb"].as_u64().map(|n| n > 0).unwrap_or(false));
    }

    #[test]
    fn normalize_arch_maps_aarch64_to_arm64() {
        assert_eq!(normalize_arch("aarch64"), "arm64");
        assert_eq!(normalize_arch("x86_64"), "x86_64");
        assert_eq!(normalize_arch("riscv64"), "riscv64");
    }

    #[test]
    fn cpu_count_fallback_is_a_positive_integer() {
        let v = cpu_count_fallback();
        assert!(v.as_u64().map(|n| n > 0).unwrap_or(false));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_vendor_is_apple() {
        assert_eq!(host_vendor(Some("Apple M1 Pro")), Some("Apple".to_string()));
    }
}
