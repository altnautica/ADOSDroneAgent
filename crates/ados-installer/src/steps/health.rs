//! Health: assert the REQUIRED components are actually up. Required. The final
//! gate before the install result is written. Ports `run_health_gate`
//! (`scripts/install.d/14-orchestration.sh:565`) PLUS one new check that closes
//! a gap in the bash gate.
//!
//! REQUIRED checks, in order:
//!   1. venv importable        — `/opt/ados/venv/bin/python -c "import ados"`.
//!   2. supervisor active       — `systemctl is-active ados-supervisor`.
//!   3. profile units enabled   — `systemctl is-enabled <unit>` for the set the
//!      profile must enable.
//!   4. REST reachable          — `curl ... http://127.0.0.1:8080/api/pairing/info`
//!      (the UNAUTHENTICATED endpoint; `/api/status` returns 401 when paired,
//!      which `curl -f` would wrongly read as a miss).
//!   5. **NEW** — every `Gate::Hard` prebuilt binary for the profile EXISTS and
//!      is executable under `/opt/ados/bin`. The bash gate trusted that
//!      `fetch_binaries` ran but never re-verified the Hard binaries are still
//!      on disk by health time; this closes that gap so a vanished/zero-length
//!      Hard binary is caught here, not at first supervisor exec.
//!   6. **NEW** — the radio stack is on disk, for BOTH the drone and the ground
//!      station profile: the wfb-ng userspace binaries on PATH, the bind
//!      artifacts (`/etc/bind.key` + `/etc/bind.yaml`), and the `wifibroadcast@`
//!      service template. Without these a fresh rig cannot auto-pair, so their
//!      absence must FAIL the gate rather than report a misleading `ok` (the
//!      bash gate never asserted them here).
//!   7. **NEW** — `mediamtx` (the video relay) is present. Best-effort: a miss
//!      degrades streaming but does not fail the install (recorded non-required).
//!
//! Each miss is recorded into `ctx.failures` as Required, so the graph's
//! status derivation flips to `failed` and the result names the missing piece.

use std::path::Path;

use crate::binaries::{self, Gate};
use crate::ctx::Ctx;
use crate::env::{SERVICE_NAME, VENV_DIR};
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// The set of systemd units that MUST be enabled for a profile's install to be
/// complete (`expected_profile_units`). Drone needs only the supervisor (its
/// children are started dynamically). Ground station needs the supervisor plus
/// the receive + AP + setup units that the systemd step enable-links. Kept a
/// strict subset of what gets enabled so completeness never demands a unit the
/// installer does not create.
pub fn expected_profile_units(profile: &str) -> &'static [&'static str] {
    match profile {
        "ground_station" => &[
            "ados-supervisor.service",
            "ados-wfb-rx.service",
            "ados-mediamtx-gs.service",
            "ados-hostapd.service",
            "ados-dnsmasq-gs.service",
            "ados-setup-captive.service",
        ],
        _ => &["ados-supervisor.service"],
    }
}

/// True when `systemctl is-enabled <unit>` reports a state that means "will be
/// brought up" (`unit_enabled`). Tolerates the not-found case.
fn unit_enabled(unit: &str) -> bool {
    let res = exec::run("systemctl", &["is-enabled", unit]);
    let state = res.stdout.trim();
    matches!(
        state,
        "enabled" | "enabled-runtime" | "static" | "alias" | "indirect"
    )
}

/// True when the venv interpreter can `import ados`.
fn venv_importable() -> bool {
    exec::run_ok(&format!("{VENV_DIR}/bin/python"), &["-c", "import ados"])
}

/// True when the supervisor unit is active.
fn supervisor_active() -> bool {
    exec::run_ok("systemctl", &["is-active", "--quiet", SERVICE_NAME])
}

/// True when the agent REST API answers on the unauthenticated pairing-info
/// endpoint. A paired agent returns 401 on `/api/status` (which `curl -f`
/// treats as failure), so we probe `/api/pairing/info` exactly like the bash.
///
/// The `start` step launched the supervisor moments ago; its `ados-api` child
/// takes a few seconds to bind `:8080`, so a single immediate probe is a false
/// miss. Poll with a grace window (returns the instant it answers), the same
/// way the bash health gate waits for the API to come up.
fn rest_reachable() -> bool {
    const ATTEMPTS: u32 = 20;
    const DELAY: std::time::Duration = std::time::Duration::from_secs(3);
    for attempt in 0..ATTEMPTS {
        if exec::run_ok(
            "curl",
            &[
                "-fsS",
                "--max-time",
                "5",
                "http://127.0.0.1:8080/api/pairing/info",
                "-o",
                "/dev/null",
            ],
        ) {
            return true;
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(DELAY);
        }
    }
    false
}

/// True when a path exists and is executable. On a non-Unix host the exec-bit
/// check degrades to "exists + non-empty" (the dev path never reaches here on
/// Linux anyway, but keeps the helper total).
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// The names of the Hard-gated prebuilt binaries that are MISSING (not present
/// or not executable) under `/opt/ados/bin` for the profile. Pure given a
/// `present` predicate, so a unit test can exercise the gap-detection without
/// touching the filesystem.
pub fn missing_hard_binaries<F>(profile: &str, present: F) -> Vec<&'static str>
where
    F: Fn(&str) -> bool,
{
    binaries::for_profile(profile)
        .into_iter()
        .filter(|b| b.gate == Gate::Hard && !present(b.dest))
        .map(|b| b.service)
        .collect()
}

/// The wfb-ng userspace binaries the bind protocol + radio services need on
/// PATH. Mirrors the set the radio step provisions; checked here so a fresh rig
/// that lost the radio stack fails the gate instead of reporting a hollow `ok`.
const RADIO_BINS: &[&str] = &["wfb_tx", "wfb_rx", "wfb_keygen", "wfb-server"];

/// The bind artifacts the supervisor's bind FSM requires before it can run.
const BIND_ARTIFACTS: &[&str] = &["/etc/bind.key", "/etc/bind.yaml"];

/// The locations the `wifibroadcast@` service template may live in (a deployed
/// drop-in under `/etc/systemd/system` or a packaged unit under
/// `/usr/lib/systemd/system`).
const WIFIBROADCAST_UNIT_DIRS: &[&str] = &["/etc/systemd/system", "/usr/lib/systemd/system"];

/// True when `bin` resolves on PATH or in the two dirs install writes console
/// scripts to. Matches the radio step's resolution so the gate agrees with what
/// the build actually lands.
fn radio_bin_present(bin: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        if std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()) {
            return true;
        }
    }
    Path::new("/usr/bin").join(bin).is_file() || Path::new("/usr/local/bin").join(bin).is_file()
}

/// True when the `wifibroadcast@.service` template is deployed (under either the
/// drop-in or the packaged unit directory).
fn wifibroadcast_template_present() -> bool {
    WIFIBROADCAST_UNIT_DIRS
        .iter()
        .any(|dir| Path::new(dir).join("wifibroadcast@.service").is_file())
}

/// True when the `mediamtx` media server is on disk (its install location or on
/// PATH). The video pipeline cannot publish without it.
fn mediamtx_present() -> bool {
    if Path::new("/usr/local/bin/mediamtx").is_file() {
        return true;
    }
    radio_bin_present("mediamtx")
}

/// The names of the radio-stack components that are MISSING for a rig (the
/// wfb-ng binaries, the bind artifacts, the service template). Both the drone
/// and the ground station profile require the full set to auto-pair, so this is
/// profile-independent. mediamtx is intentionally NOT here: it is best-effort
/// (a missing video relay degrades streaming but must not abort the install), so
/// the gate checks it separately. The labels are namespaced so they read clearly
/// in the failure list.
fn missing_radio_stack() -> Vec<String> {
    let mut missing: Vec<String> = Vec::new();
    for bin in RADIO_BINS {
        if !radio_bin_present(bin) {
            missing.push(format!("radio-bin:{bin}"));
        }
    }
    for artifact in BIND_ARTIFACTS {
        if !Path::new(artifact).exists() {
            missing.push(format!("bind-artifact:{artifact}"));
        }
    }
    if !wifibroadcast_template_present() {
        missing.push("wifibroadcast-template".to_string());
    }
    missing
}

/// Post-start health gate.
pub struct Health;

impl Step for Health {
    fn id(&self) -> &str {
        "health"
    }
    fn requires(&self) -> &[&str] {
        &["start"]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // Off Linux there is nothing to gate (no systemd, no venv): treat as OK
        // so a dev-host dry run does not record phantom failures.
        if ctx.env.os != "linux" {
            return StepOutcome::Ok;
        }

        let mut misses: Vec<String> = Vec::new();

        // 1. venv importable.
        if !venv_importable() {
            misses.push("venv-import".to_string());
        }

        // 2. supervisor active.
        if !supervisor_active() {
            misses.push("supervisor-active".to_string());
        }

        // 3. profile units enabled.
        for unit in expected_profile_units(&ctx.profile) {
            if !unit_enabled(unit) {
                misses.push(format!("unit-enabled:{unit}"));
            }
        }

        // 4. REST reachable.
        if !rest_reachable() {
            misses.push("api-reachable".to_string());
        }

        // 5. NEW: every Hard prebuilt binary is present + executable.
        for svc in missing_hard_binaries(&ctx.profile, |dest| is_executable(Path::new(dest))) {
            misses.push(format!("binary-missing:{svc}"));
        }

        // 6. The radio stack is on disk (both drone + ground station need it):
        // without the wfb-ng binaries, the bind artifacts, or the service
        // template a fresh rig cannot auto-pair, so each is a required miss.
        misses.extend(missing_radio_stack());

        // 7. mediamtx is best-effort (the video relay). A missing one degrades
        // streaming but must not abort an otherwise-working install, so record
        // it as a non-required failure rather than adding it to `misses`.
        if !mediamtx_present() {
            ctx.failures.record("mediamtx-missing", false);
            tracing::warn!("mediamtx not present; video streaming degraded");
        }

        if misses.is_empty() {
            tracing::info!("install health gate: OK");
            StepOutcome::Ok
        } else {
            // Record each miss as a Required failure so the result names them
            // individually (matching the bash gate's per-check record_failure).
            for m in &misses {
                ctx.failures.record(m, true);
            }
            tracing::error!(misses = ?misses, "install health gate FAILED");
            // The step itself also fails so the graph reports it; the per-miss
            // records above give the granular failedSteps list.
            StepOutcome::Failed(format!(
                "required components missing: {}",
                misses.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drone_expects_only_the_supervisor() {
        let units = expected_profile_units("drone");
        assert_eq!(units, &["ados-supervisor.service"]);
    }

    #[test]
    fn ground_station_expects_the_receive_and_ap_set() {
        let units = expected_profile_units("ground_station");
        assert!(units.contains(&"ados-supervisor.service"));
        assert!(units.contains(&"ados-wfb-rx.service"));
        assert!(units.contains(&"ados-hostapd.service"));
        assert!(units.contains(&"ados-setup-captive.service"));
    }

    #[test]
    fn missing_hard_binaries_reports_only_absent_hard_gates() {
        // Pretend every binary is present → no misses.
        let none = missing_hard_binaries("drone", |_| true);
        assert!(none.is_empty());

        // Pretend ados-video is absent (a Hard gate on the drone profile) but
        // everything else present → exactly that one is reported.
        let one = missing_hard_binaries("drone", |dest| dest != "/opt/ados/bin/ados-video");
        assert_eq!(one, vec!["ados-video"]);

        // A best-effort binary being absent must NOT be reported (e.g. ados-tui).
        let beste = missing_hard_binaries("drone", |dest| dest != "/opt/ados/bin/ados-tui");
        assert!(
            beste.is_empty(),
            "best-effort miss must not flag the health gate"
        );

        // All Hard gates absent → all of them reported (drone Hard set:
        // supervisor, video, cloud, vision).
        let all = missing_hard_binaries("drone", |_| false);
        for svc in ["ados-supervisor", "ados-video", "ados-cloud", "ados-vision"] {
            assert!(
                all.contains(&svc),
                "{svc} (Hard) must be flagged when absent"
            );
        }
        // ados-radio is best-effort on drone → must not appear.
        assert!(!all.contains(&"ados-radio"));
    }

    #[test]
    fn radio_stack_set_covers_the_bind_protocol() {
        // The bind FSM tools + artifacts the auto-pair path needs at boot.
        assert!(RADIO_BINS.contains(&"wfb_keygen"));
        assert!(RADIO_BINS.contains(&"wfb-server"));
        assert!(BIND_ARTIFACTS.contains(&"/etc/bind.key"));
        assert!(BIND_ARTIFACTS.contains(&"/etc/bind.yaml"));
    }

    #[test]
    fn wifibroadcast_template_checked_in_both_unit_dirs() {
        // Both the deployed drop-in dir and the packaged unit dir are searched
        // so a template in either location satisfies the gate.
        assert!(WIFIBROADCAST_UNIT_DIRS.contains(&"/etc/systemd/system"));
        assert!(WIFIBROADCAST_UNIT_DIRS.contains(&"/usr/lib/systemd/system"));
    }

    #[test]
    fn missing_hard_binaries_respects_profile() {
        // ados-groundlink is best-effort + ground-only; ados-cloud is Hard +
        // both. On the GS profile with everything absent, ados-cloud (Hard)
        // shows, ados-groundlink (best-effort) does not, ados-video (drone-only)
        // does not.
        let gs = missing_hard_binaries("ground_station", |_| false);
        assert!(gs.contains(&"ados-cloud"));
        assert!(gs.contains(&"ados-supervisor"));
        assert!(!gs.contains(&"ados-groundlink"));
        assert!(!gs.contains(&"ados-video"));
    }
}
