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
//!   6. **NEW** — the native WFB binary the profile's units exec by DEFAULT
//!      (`ados-radio` on a drone, `ados-groundlink` on a ground station) is
//!      present + executable. The WFB units run the native service on a clean
//!      boot and keep Python only as a flag-guarded fallback, so a missing
//!      native binary would crash-loop the unit; its on-disk presence is a
//!      required precondition even though its prebuilt fetch gate is best-effort.
//!      Also: the MAVLink router binary the Core MAVLink unit execs on both
//!      profiles is present + executable. It is the sole C2 path with no
//!      packaged fallback; its fetch gate is Hard, so this re-verification only
//!      catches a binary that vanished/was truncated between fetch and health,
//!      where the unit would crash-loop with no FC link while the install
//!      otherwise looked healthy.
//!   7. **NEW** — the native display binaries (`ados-display` +
//!      `ados-display-probe`) the display units exec by DEFAULT are present +
//!      executable, but ONLY when a panel was recognized (`display.enabled`
//!      marker). On a display-less rig the display units skip clean, so the
//!      binaries are not required there; once a panel is bound the unit runs the
//!      native binary on a clean boot, so a missing one would crash-loop the
//!      display and must FAIL the gate. Their prebuilt fetch gate stays
//!      best-effort so the install never aborts on a board with no display.
//!   8. **NEW** — the radio stack is on disk, for BOTH the drone and the ground
//!      station profile: the wfb-ng userspace binaries on PATH, the bind
//!      artifacts (`/etc/bind.key` + `/etc/bind.yaml`), and the `wifibroadcast@`
//!      service template. Without these a fresh rig cannot auto-pair, so their
//!      absence must FAIL the gate rather than report a misleading `ok` (the
//!      bash gate never asserted them here).
//!   9. **NEW** — `mediamtx` (the video relay) is present. Best-effort: a miss
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

/// The MAVLink router binary the Core MAVLink unit execs on both profiles. It
/// is the sole command-and-control path to the flight controller and has no
/// packaged fallback, so its on-disk presence is a required precondition: a
/// missing binary crash-loops the Core unit, leaving the drone with no FC
/// telemetry, arming, or GCS link. Profile-independent — both a drone and a
/// ground station run the router.
const C2_ROUTER_BINARY: &str = "/opt/ados/bin/ados-mavlink-router";

/// The MAVLink router binary if it is MISSING (absent or not executable), else
/// `None`. Pure given a `present` predicate so a unit test can exercise it
/// without touching the filesystem.
pub fn missing_c2_router_binary<F>(present: F) -> Option<&'static str>
where
    F: Fn(&str) -> bool,
{
    if present(C2_ROUTER_BINARY) {
        None
    } else {
        Some(C2_ROUTER_BINARY)
    }
}

/// The native service binary each profile's WFB units now exec by DEFAULT (the
/// Python service is kept on disk only as an emergency fallback behind a flag).
/// Because the unit runs the native binary unconditionally on a clean boot, its
/// absence would crash-loop the unit; so the install must verify it is present
/// and executable here, even though the prebuilt catalog gate for these two is
/// best-effort (a fetch miss degrades fetch but must not leave a unit pointed at
/// a binary that is not there). `ados-radio` is the drone-side transmitter,
/// `ados-groundlink` the ground-side receive plane.
fn default_radio_binary(profile: &str) -> Option<&'static str> {
    match profile {
        "ground_station" => Some("/opt/ados/bin/ados-groundlink"),
        "drone" => Some("/opt/ados/bin/ados-radio"),
        _ => None,
    }
}

/// The name of the default native WFB binary that is MISSING (absent or not
/// executable) for the profile, if any. Pure given a `present` predicate so a
/// unit test can exercise it without touching the filesystem.
pub fn missing_default_radio_binary<F>(profile: &str, present: F) -> Option<&'static str>
where
    F: Fn(&str) -> bool,
{
    default_radio_binary(profile).filter(|dest| !present(dest))
}

/// Marker the display install writes once an SPI-LCD / HDMI / OLED panel is
/// recognized. When it is absent the display services skip clean (their unit
/// `ConditionPathExists` is unmet), so the display binaries are not needed and
/// must not gate the install on a panel-less rig.
const DISPLAY_ENABLED_MARKER: &str = "/etc/ados/display.enabled";

/// The native display binaries the `ados-oled` / `ados-display-probe` units now
/// exec by DEFAULT. Both profiles can host a panel, so the set is
/// profile-independent. The Python render service stays on disk only as the
/// flag-pinned fallback; on a clean boot the unit runs the native binary, so a
/// missing one would crash-loop the display. Their prebuilt fetch gate is
/// best-effort (a display-less rig must still install), so the cutover makes
/// their on-disk presence a required precondition ONLY when a panel was
/// recognized.
const DEFAULT_DISPLAY_BINARIES: &[&str] = &[
    "/opt/ados/bin/ados-display",
    "/opt/ados/bin/ados-display-probe",
];

/// The default native display binaries that are MISSING (absent or not
/// executable) when a display is enabled. Pure given a `display_enabled`
/// predicate and a `present` predicate so a unit test can exercise the
/// gap-detection without touching the filesystem. Returns an empty vec when no
/// panel is bound (the units are a clean no-op there).
pub fn missing_default_display_binaries<G, F>(display_enabled: G, present: F) -> Vec<&'static str>
where
    G: Fn() -> bool,
    F: Fn(&str) -> bool,
{
    if !display_enabled() {
        return Vec::new();
    }
    DEFAULT_DISPLAY_BINARIES
        .iter()
        .copied()
        .filter(|dest| !present(dest))
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

        // 6. The native WFB binary the profile's units now exec by default must
        // be present + executable, or the unit would crash-loop. Its catalog gate
        // is best-effort for the fetch step, but the cutover makes its on-disk
        // presence a required precondition for a working install.
        if let Some(dest) =
            missing_default_radio_binary(&ctx.profile, |d| is_executable(Path::new(d)))
        {
            let svc = Path::new(dest)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(dest);
            misses.push(format!("binary-missing:{svc}"));
        }

        // 6b. The MAVLink router (the sole C2 path, no packaged fallback) must be
        // present + executable on both profiles. Its fetch gate is Hard so a
        // fetch miss already aborts the install before this point; re-verifying
        // its on-disk presence here catches a binary that vanished or was
        // truncated between fetch and health time, when the Core MAVLink unit
        // would otherwise crash-loop with no FC link reported as a healthy
        // install.
        if let Some(dest) = missing_c2_router_binary(|d| is_executable(Path::new(d))) {
            let svc = Path::new(dest)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(dest);
            misses.push(format!("binary-missing:{svc}"));
        }

        // 7. The native display binaries the display units now exec by DEFAULT
        // must be present + executable WHEN a panel was recognized. Their
        // prebuilt fetch gate is best-effort so a display-less rig still
        // installs, but once `display.enabled` is written the unit runs the
        // native binary on a clean boot, so a missing one would crash-loop the
        // panel — a required miss, gated on the panel marker.
        for dest in missing_default_display_binaries(
            || Path::new(DISPLAY_ENABLED_MARKER).exists(),
            |d| is_executable(Path::new(d)),
        ) {
            let svc = Path::new(dest)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(dest);
            misses.push(format!("binary-missing:{svc}"));
        }

        // 8. The radio stack is on disk (both drone + ground station need it):
        // without the wfb-ng binaries, the bind artifacts, or the service
        // template a fresh rig cannot auto-pair, so each is a required miss.
        misses.extend(missing_radio_stack());

        // 9. mediamtx is best-effort (the video relay). A missing one degrades
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
    fn default_radio_binary_is_profile_specific_and_gated() {
        // All present → no miss on either profile.
        assert!(missing_default_radio_binary("drone", |_| true).is_none());
        assert!(missing_default_radio_binary("ground_station", |_| true).is_none());

        // Drone default unit binary missing → ados-radio reported.
        let drone =
            missing_default_radio_binary("drone", |dest| dest != "/opt/ados/bin/ados-radio");
        assert_eq!(drone, Some("/opt/ados/bin/ados-radio"));

        // Ground-station default unit binary missing → ados-groundlink reported.
        let gs = missing_default_radio_binary("ground_station", |dest| {
            dest != "/opt/ados/bin/ados-groundlink"
        });
        assert_eq!(gs, Some("/opt/ados/bin/ados-groundlink"));

        // The drone never gates on the groundlink binary, and the ground station
        // never gates on the radio binary (each profile checks only its own unit).
        assert!(missing_default_radio_binary("drone", |dest| dest
            != "/opt/ados/bin/ados-groundlink")
        .is_none());
        assert!(missing_default_radio_binary("ground_station", |dest| dest
            != "/opt/ados/bin/ados-radio")
        .is_none());

        // An unknown profile has no default WFB unit binary to gate on.
        assert!(missing_default_radio_binary("compute", |_| false).is_none());
    }

    #[test]
    fn c2_router_binary_is_required_on_both_profiles() {
        // Present → no miss.
        assert!(missing_c2_router_binary(|_| true).is_none());

        // Absent → the router path is reported (the sole C2 path, no fallback).
        let missing = missing_c2_router_binary(|dest| dest != "/opt/ados/bin/ados-mavlink-router");
        assert_eq!(missing, Some("/opt/ados/bin/ados-mavlink-router"));

        // The check is profile-independent: the predicate alone decides it, so
        // a missing router fails the gate regardless of which profile is being
        // installed (both a drone and a ground station run the router).
        assert_eq!(
            missing_c2_router_binary(|_| false),
            Some("/opt/ados/bin/ados-mavlink-router")
        );
    }

    #[test]
    fn display_binaries_gated_on_the_panel_marker() {
        // No panel bound → the display binaries never gate the install, even
        // when both are absent (the units are a clean no-op there).
        assert!(missing_default_display_binaries(|| false, |_| false).is_empty());

        // Panel bound + both present → no miss.
        assert!(missing_default_display_binaries(|| true, |_| true).is_empty());

        // Panel bound + the render daemon missing → exactly that one reported.
        let one =
            missing_default_display_binaries(|| true, |dest| dest != "/opt/ados/bin/ados-display");
        assert_eq!(one, vec!["/opt/ados/bin/ados-display"]);

        // Panel bound + both missing → both reported.
        let both = missing_default_display_binaries(|| true, |_| false);
        assert_eq!(
            both,
            vec![
                "/opt/ados/bin/ados-display",
                "/opt/ados/bin/ados-display-probe"
            ]
        );
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
