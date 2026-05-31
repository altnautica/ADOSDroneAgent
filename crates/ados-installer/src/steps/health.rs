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
fn rest_reachable() -> bool {
    exec::run_ok(
        "curl",
        &[
            "-fsS",
            "--max-time",
            "5",
            "http://127.0.0.1:8080/api/pairing/info",
            "-o",
            "/dev/null",
        ],
    )
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
    std::fs::metadata(path).map(|m| m.is_file() && m.len() > 0).unwrap_or(false)
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
            StepOutcome::Failed(format!("required components missing: {}", misses.join(", ")))
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
        assert!(beste.is_empty(), "best-effort miss must not flag the health gate");

        // All Hard gates absent → all of them reported (drone Hard set:
        // supervisor, video, cloud, vision).
        let all = missing_hard_binaries("drone", |_| false);
        for svc in ["ados-supervisor", "ados-video", "ados-cloud", "ados-vision"] {
            assert!(all.contains(&svc), "{svc} (Hard) must be flagged when absent");
        }
        // ados-radio is best-effort on drone → must not appear.
        assert!(!all.contains(&"ados-radio"));
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
