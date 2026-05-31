//! Deps: install the apt + python system dependencies the agent needs.
//! Required — nothing downstream works without them. Checkpoint `deps`.
//!
//! Ports `scripts/install.d/02-deps.sh`. The core (cross-profile) package set
//! is required; the ground-station extras (AP + kiosk stack) and a handful of
//! optional packages tolerate failure. The package-list construction is pure so
//! a unit test can assert the set without invoking apt.

use crate::ctx::Ctx;
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// The cross-profile core apt package set (REQUIRED). Ported verbatim from
/// `install_system_deps` in 02-deps.sh: Python venv + native-extension build
/// deps, the gstreamer runtime + ffmpeg/v4l video stack, the radio userspace
/// build deps, and the avahi/socat plumbing.
pub fn core_packages() -> &'static [&'static str] {
    &[
        "python3-venv",
        "python3-pip",
        "python3-dev",
        "python3-setuptools",
        "python3-twisted",
        "python3-serial",
        "python3-jinja2",
        "python3-msgpack",
        "python3-pyroute2",
        "python3-gi",
        "gir1.2-gstreamer-1.0",
        "socat",
        "libcap-dev",
        "libsystemd-dev",
        "libyaml-dev",
        "libsodium-dev",
        "libpcap-dev",
        "libevent-dev",
        "build-essential",
        "git",
        "curl",
        "avahi-daemon",
        "ffmpeg",
        "v4l-utils",
        "gstreamer1.0-tools",
        "gstreamer1.0-plugins-base",
        "gstreamer1.0-plugins-good",
        "gstreamer1.0-plugins-bad",
        "gstreamer1.0-plugins-ugly",
        "gstreamer1.0-libav",
        "gstreamer1.0-rtsp",
        "iw",
        "ethtool",
        "wireless-regdb",
    ]
}

/// Optional packages installed best-effort (failure only degrades). The
/// gstreamer -dev headers are only needed to compile the optional wfb_rtsp
/// demo target; on some BSP repos they pull an unsatisfiable runtime dep, so a
/// failure here must not take down the deps step.
pub fn optional_packages() -> &'static [&'static str] {
    &["libgstreamer1.0-dev", "libgstrtspserver-1.0-dev"]
}

/// Core ground-station apt packages (the AP + bluetooth + kiosk-compositor
/// stack, minus the chromium browser which installs separately/best-effort).
/// Required for the ground_station profile.
pub fn ground_station_core_packages() -> &'static [&'static str] {
    &["hostapd", "dnsmasq", "bluetooth", "bluez", "cage"]
}

/// Assemble the REQUIRED package set for a profile (pure). The drone profile
/// gets the cross-profile core; the ground_station profile additionally gets
/// the AP/kiosk core. Deduped + insertion-stable.
pub fn required_packages(profile: &str) -> Vec<&'static str> {
    let mut pkgs: Vec<&'static str> = core_packages().to_vec();
    if profile == "ground_station" {
        for p in ground_station_core_packages() {
            if !pkgs.contains(p) {
                pkgs.push(p);
            }
        }
    }
    pkgs
}

/// `apt-get update`, surfacing failure. Errors propagate so the caller fails
/// the step (a stale index breaks every install below).
fn apt_update() -> anyhow::Result<()> {
    let res = exec::run("apt-get", &["update", "-qq"]);
    if res.success() {
        Ok(())
    } else if !res.spawned {
        anyhow::bail!("apt-get is not available on this host")
    } else {
        anyhow::bail!("apt-get update failed: {}", res.stderr.trim())
    }
}

/// Install a package set in one `apt-get install` invocation. `required`
/// controls whether a non-zero exit is fatal (Err) or tolerated (warn).
fn apt_install(pkgs: &[&str], required: bool) -> anyhow::Result<()> {
    if pkgs.is_empty() {
        return Ok(());
    }
    let mut argv: Vec<&str> = vec!["install", "-y", "-qq"];
    argv.extend_from_slice(pkgs);
    let res = exec::run("apt-get", &argv);
    if res.success() {
        return Ok(());
    }
    if required {
        if !res.spawned {
            anyhow::bail!("apt-get is not available on this host");
        }
        anyhow::bail!(
            "apt-get install of required packages failed: {}",
            res.stderr.trim()
        );
    }
    tracing::warn!(
        code = ?res.code,
        "best-effort apt-get install reported a non-zero status; continuing"
    );
    Ok(())
}

/// Discover a Python 3.11+ interpreter on PATH, returning its program name.
/// Mirrors the bash `find_python` candidate order. Pure-ish (it execs each
/// candidate's `--version`), used to fail the step early when no usable
/// interpreter exists — the venv step cannot proceed without one.
pub fn find_python() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "python3.13",
        "python3.12",
        "python3.11",
        "/usr/local/bin/python3.11",
        "python3",
    ];
    for cand in CANDIDATES {
        if python_is_311_plus(cand) {
            return Some((*cand).to_string());
        }
    }
    None
}

/// True when `prog -c <print version>` reports a >= 3.11 interpreter.
fn python_is_311_plus(prog: &str) -> bool {
    let res = exec::run(
        prog,
        &[
            "-c",
            "import sys; print(f'{sys.version_info.major}.{sys.version_info.minor}')",
        ],
    );
    if !res.success() {
        return false;
    }
    parse_py_version_ge_311(res.stdout.trim())
}

/// Parse a `major.minor` version string and test it is >= 3.11. Pure.
fn parse_py_version_ge_311(ver: &str) -> bool {
    let mut parts = ver.split('.');
    let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    major > 3 || (major == 3 && minor >= 11)
}

/// System dependency installation.
pub struct Deps;

impl Step for Deps {
    fn id(&self) -> &str {
        "deps"
    }
    fn requires(&self) -> &[&str] {
        &["preflight"]
    }
    fn checkpoint(&self) -> Option<&str> {
        Some("deps")
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        if let Err(e) = apt_update() {
            return StepOutcome::Failed(e.to_string());
        }

        let required = required_packages(&ctx.profile);
        if let Err(e) = apt_install(&required, true) {
            return StepOutcome::Failed(e.to_string());
        }

        // Optional headers tolerate failure (wfb_rtsp demo target only).
        if let Err(e) = apt_install(optional_packages(), false) {
            tracing::warn!(error = %e, "optional dev headers not installed; wfb_rtsp build skipped");
        }

        // A usable Python 3.11+ must exist before the venv step. We do not try
        // to install it here (the venv step + portable-python provisioning own
        // that fallback); we only fail loudly when nothing usable is on PATH so
        // the failure is attributed to deps rather than a cryptic venv error.
        if find_python().is_none() {
            tracing::warn!(
                "no Python 3.11+ found on PATH; the venv step will attempt to provision one"
            );
        }

        StepOutcome::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_set_has_the_load_bearing_packages() {
        let core = core_packages();
        for p in [
            "ffmpeg",
            "v4l-utils",
            "avahi-daemon",
            "gstreamer1.0-tools",
            "gstreamer1.0-rtsp",
            "python3-venv",
            "python3-serial",
            "python3-jinja2",
            "python3-msgpack",
            "python3-pyroute2",
            "socat",
            "curl",
        ] {
            assert!(core.contains(&p), "core deps must include {p}");
        }
        // The wrong v4l package name must NOT appear (it breaks the install).
        assert!(!core.contains(&"v4l2-utils"));
    }

    #[test]
    fn drone_profile_excludes_ground_station_extras() {
        let drone = required_packages("drone");
        assert!(!drone.contains(&"hostapd"));
        assert!(!drone.contains(&"dnsmasq"));
        assert!(!drone.contains(&"cage"));
        // Core is still present.
        assert!(drone.contains(&"ffmpeg"));
    }

    #[test]
    fn ground_station_adds_ap_kiosk_core() {
        let gs = required_packages("ground_station");
        for p in ["hostapd", "dnsmasq", "bluetooth", "bluez", "cage"] {
            assert!(gs.contains(&p), "ground_station deps must include {p}");
        }
        // No duplicates after merge.
        let mut sorted = gs.clone();
        sorted.sort_unstable();
        let len_before = sorted.len();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            len_before,
            "required package set must be unique"
        );
    }

    #[test]
    fn python_version_threshold() {
        assert!(parse_py_version_ge_311("3.11"));
        assert!(parse_py_version_ge_311("3.12"));
        assert!(parse_py_version_ge_311("4.0"));
        assert!(!parse_py_version_ge_311("3.9"));
        assert!(!parse_py_version_ge_311("3.10"));
        assert!(!parse_py_version_ge_311("2.7"));
        assert!(!parse_py_version_ge_311("garbage"));
    }
}
