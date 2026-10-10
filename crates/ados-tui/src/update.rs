//! Background "newer version available?" check for the cockpit.
//!
//! Asks GitHub for the latest agent version on the node's release channel and
//! compares it to the installed version. The channel is the one the installer
//! recorded in `/etc/ados/profile.conf` (`edge` when unrecorded, the installer's
//! default): an edge node follows `__version__` on `main`; a stable node follows
//! the `__version__` of the newest published `v*` release, because that is all
//! `ados update` installs there. The fetch shells out to `curl` — matching the
//! installer's house pattern (the workspace `ureq` is HTTP-only / no-TLS, see
//! `ados-installer::net`) — so the tiny cockpit binary never links a TLS stack.
//! It runs on a background thread that re-checks periodically, so an open
//! cockpit auto-notices a fresh release without ever blocking startup, the poll
//! loop, or input, and degrades silently when offline (a failed re-check keeps
//! the last-good value). Set `ADOS_NO_UPDATE_CHECK` to skip it entirely.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// The raw `__init__.py` at a git ref.
fn init_py_url(git_ref: &str) -> String {
    format!("https://raw.githubusercontent.com/altnautica/ADOSDroneAgent/{git_ref}/src/ados/__init__.py")
}

/// The published releases, newest first.
const RELEASES_URL: &str =
    "https://api.github.com/repos/altnautica/ADOSDroneAgent/releases?per_page=50";

/// Where the installer records the node's release channel.
const PROFILE_CONF: &str = "/etc/ados/profile.conf";

/// The release channel a node follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Tip of `main`.
    Edge,
    /// Published `v<X.Y.Z>` releases.
    Stable,
}

/// The channel a `profile.conf` body records (`channel: stable` or the legacy
/// `channel=stable`). Anything else, or no line, is the installer default:
/// edge.
pub fn parse_channel(profile_conf: &str) -> Channel {
    for line in profile_conf.lines() {
        let line = line.trim();
        let value = line
            .strip_prefix("channel:")
            .or_else(|| line.strip_prefix("channel="));
        if let Some(v) = value {
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            return if v == "stable" {
                Channel::Stable
            } else {
                Channel::Edge
            };
        }
    }
    Channel::Edge
}

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
        let channel = std::fs::read_to_string(PROFILE_CONF)
            .map(|body| parse_channel(&body))
            .unwrap_or(Channel::Edge);
        store_result(&out, fetch_latest(channel));
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

/// `curl` a URL to a string. `None` on any error (offline, curl missing,
/// non-2xx).
fn curl(url: &str) -> Option<String> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "4",
            "--max-time",
            "8",
            "-H",
            "User-Agent: ados-tui",
            url,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fetch the latest `__version__` on `channel`. `None` on any error so the
/// cockpit stays silent.
fn fetch_latest(channel: Channel) -> Option<String> {
    let git_ref = match channel {
        Channel::Edge => "main".to_string(),
        Channel::Stable => latest_release_tag(&curl(RELEASES_URL)?)?,
    };
    parse_version(&curl(&init_py_url(&git_ref))?)
}

/// The tag of the newest published `v<X.Y.Z>` release in a GitHub releases
/// listing: drafts, pre-releases and non-version tags (the rolling binary
/// tags) are skipped, and "newest" is the highest version, not list order.
pub fn latest_release_tag(releases_json: &str) -> Option<String> {
    let releases: serde_json::Value = serde_json::from_str(releases_json).ok()?;
    releases
        .as_array()?
        .iter()
        .filter(|r| r["draft"] != true && r["prerelease"] != true)
        .filter_map(|r| r["tag_name"].as_str())
        .filter(|tag| {
            tag.strip_prefix('v')
                .is_some_and(|v| parse_tuple(v).is_some())
        })
        .max_by(|a, b| parse_tuple(&a[1..]).cmp(&parse_tuple(&b[1..])))
        .map(str::to_string)
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
/// available". A pre-release or build suffix (`-dev`, `-rc1`, `+abc`) is dropped
/// before comparing, and either side that still does not parse (including an
/// unknown `?` installed version) never reports an update.
pub fn is_newer(latest: &str, installed: &str) -> bool {
    match (parse_tuple(latest), parse_tuple(installed)) {
        (Some(l), Some(i)) => l > i,
        _ => false,
    }
}

/// Parse the dotted numeric core of a version into a comparable tuple, e.g.
/// `0.99.108-dev` → `[0, 99, 108]`. `None` if any core component is not a number.
fn parse_tuple(v: &str) -> Option<Vec<u64>> {
    let core = v.split(['-', '+']).next().unwrap_or_default();
    core.split('.').map(|p| p.parse().ok()).collect()
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
    fn newer_compares_the_numeric_core_of_suffixed_versions() {
        assert!(is_newer("2.0.0-rc1", "1.9.9"));
        // A dev build ahead of main is never offered a downgrade.
        assert!(!is_newer("0.99.108", "0.99.109-dev"));
        assert!(!is_newer("0.99.108-rc1", "0.99.109"));
        assert!(!is_newer("0.99.108", "0.99.108+local"));
        assert!(is_newer("0.99.110", "0.99.109-dev"));
        // Anything that does not parse never reports an update.
        assert!(!is_newer("", "1.0.0"));
        assert!(!is_newer("next", "1.0.0"));
        assert!(!is_newer("1.0.1", "unknown"));
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

    #[test]
    fn the_channel_comes_from_profile_conf() {
        assert_eq!(
            parse_channel("profile: drone\nchannel: stable\n"),
            Channel::Stable
        );
        assert_eq!(
            parse_channel("profile=drone\nchannel=stable\n"),
            Channel::Stable
        );
        assert_eq!(
            parse_channel("profile: drone\nchannel: edge\n"),
            Channel::Edge
        );
        assert_eq!(parse_channel("profile: drone\n"), Channel::Edge);
    }

    #[test]
    fn stable_channel_no_badge_when_main_ahead() {
        let releases = r#"[
            {"tag_name": "prebuilt-main", "draft": false, "prerelease": false},
            {"tag_name": "v0.104.0-rc1", "draft": false, "prerelease": true},
            {"tag_name": "v0.103.0", "draft": true, "prerelease": false},
            {"tag_name": "v0.101.0", "draft": false, "prerelease": false},
            {"tag_name": "v0.102.0", "draft": false, "prerelease": false}
        ]"#;
        let tag = latest_release_tag(releases).unwrap();
        assert_eq!(tag, "v0.102.0");
        let installed = "0.102.0";
        // main is ahead, but a stable node is not offered it.
        let main_version = "0.103.0";
        assert!(is_newer(main_version, installed));
        assert!(!is_newer(&tag[1..], installed));
        assert_eq!(latest_release_tag("not json"), None);
    }
}
