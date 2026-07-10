//! Background "newer version available?" check for the cockpit.
//!
//! Pings GitHub for the latest agent version on `main` (the same source the
//! `ados update` CLI reads) and compares it to the installed version. The fetch
//! shells out to `curl` — matching the installer's house pattern (the workspace
//! `ureq` is HTTP-only / no-TLS, see `ados-installer::net`) — so the tiny cockpit
//! binary never links a TLS stack. It runs on a background thread that re-checks
//! periodically, so an open cockpit auto-notices a fresh release without ever
//! blocking startup, the poll loop, or input, and degrades silently when
//! offline (a failed re-check keeps the last-good value). Set
//! `ADOS_NO_UPDATE_CHECK` to skip it entirely.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The raw `__init__.py` on `main` — the same source `ados update` reads.
const REMOTE_VERSION_URL: &str =
    "https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/main/src/ados/__init__.py";

/// How often the background thread re-fetches the latest version. Modest so an
/// open cockpit notices a new release within a few minutes, low-volume enough
/// (one unauthenticated raw fetch) to never look like polling abuse.
const RECHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// A shared slot holding the fetched latest version once the background check
/// completes. `None` until then (and when the check is disabled / offline).
pub type LatestSlot = Arc<Mutex<Option<String>>>;

/// Spawn the background version check and return the slot it fills. The thread
/// fetches immediately, then re-fetches every [`RECHECK_INTERVAL`] so an open
/// cockpit auto-notices a new release. Honours `ADOS_NO_UPDATE_CHECK` (returns
/// an empty slot, no thread, no network).
pub fn spawn_check() -> LatestSlot {
    let slot: LatestSlot = Arc::new(Mutex::new(None));
    if std::env::var_os("ADOS_NO_UPDATE_CHECK").is_some() {
        return slot;
    }
    let out = Arc::clone(&slot);
    thread::spawn(move || loop {
        store_result(&out, fetch_latest());
        thread::sleep(RECHECK_INTERVAL);
    });
    slot
}

/// Fold a fetch result into the slot: a successful fetch replaces the value; a
/// failed fetch (`None`) leaves the last-good value untouched, so a transient
/// network blip never clears an already-known "update available". Extracted so
/// the keep-last-good behaviour is unit-testable without a network.
fn store_result(slot: &LatestSlot, fetched: Option<String>) {
    if let Some(latest) = fetched {
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(latest);
        }
    }
}

/// Fetch the latest `__version__` from GitHub via `curl`. `None` on any error
/// (offline, curl missing, non-2xx, unparseable) so the cockpit stays silent.
fn fetch_latest() -> Option<String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "4",
            "--max-time",
            "8",
            REMOTE_VERSION_URL,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_version(&String::from_utf8_lossy(&output.stdout))
}

/// Extract `__version__ = "X.Y.Z"` from a Python source string (single or double
/// quotes). No `regex` dependency — a small manual scan.
fn parse_version(src: &str) -> Option<String> {
    let after_marker = src.split_once("__version__")?.1;
    let after_eq = after_marker.split_once('=')?.1;
    let open = after_eq.find(['"', '\''])?;
    let quote = after_eq.as_bytes()[open] as char;
    let rest = &after_eq[open + 1..];
    let end = rest.find(quote)?;
    let value = rest[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// True when `latest` is strictly newer than `installed`, compared as dotted
/// numeric tuples so a local dev / ahead build never shows a false "update
/// available". Falls back to a plain inequality when either side is not a clean
/// numeric version, and never fires when the installed version is unknown (`?`).
pub fn is_newer(latest: &str, installed: &str) -> bool {
    match (parse_tuple(latest), parse_tuple(installed)) {
        (Some(l), Some(i)) => l > i,
        _ => !latest.is_empty() && installed != "?" && latest != installed,
    }
}

/// Parse a dotted numeric version into a comparable tuple, e.g. `0.99.108` →
/// `[0, 99, 108]`. `None` if any component is non-numeric (a pre-release tag).
fn parse_tuple(v: &str) -> Option<Vec<u64>> {
    let parts: Option<Vec<u64>> = v.split('.').map(|p| p.parse().ok()).collect();
    parts.filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_from_source() {
        assert_eq!(
            parse_version("__version__ = \"0.99.108\"\n").as_deref(),
            Some("0.99.108")
        );
        assert_eq!(
            parse_version("x = 1\n__version__='1.2.3'  # note\n").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(parse_version("no version here"), None);
    }

    #[test]
    fn newer_uses_numeric_tuple_order() {
        assert!(is_newer("0.99.109", "0.99.108"));
        assert!(is_newer("0.100.0", "0.99.108")); // 100 > 99, not string order
        assert!(is_newer("0.99.108", "0.99")); // longer tuple is greater
        assert!(!is_newer("0.99.108", "0.99.108"));
        assert!(!is_newer("0.99.107", "0.99.108")); // behind → no update
        assert!(!is_newer("0.99.108", "?")); // installed unknown → never
    }

    #[test]
    fn newer_falls_back_for_non_numeric() {
        assert!(is_newer("2.0.0-rc1", "1.9.9")); // unparseable → inequality
        assert!(!is_newer("", "1.0.0")); // empty latest → never
    }

    #[test]
    fn store_result_keeps_last_good_on_failed_refetch() {
        let slot: LatestSlot = Arc::new(Mutex::new(None));
        // First successful fetch fills the slot.
        store_result(&slot, Some("0.99.120".to_string()));
        assert_eq!(slot.lock().unwrap().as_deref(), Some("0.99.120"));
        // A failed re-fetch must NOT clear the last-good value.
        store_result(&slot, None);
        assert_eq!(slot.lock().unwrap().as_deref(), Some("0.99.120"));
        // A newer fetch replaces it.
        store_result(&slot, Some("0.99.121".to_string()));
        assert_eq!(slot.lock().unwrap().as_deref(), Some("0.99.121"));
    }
}
