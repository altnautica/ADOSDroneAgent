//! Preflight: validate the host before any install work and resolve the
//! run's profile + channel. Required — a failed preflight aborts the install.
//!
//! What it checks / resolves, in order:
//!   1. **root** — every later step writes under `/opt/ados`, `/etc/ados`,
//!      `/etc/systemd/system`, so a non-root run cannot proceed. On a non-Linux
//!      dev host the check is inert (returns "root") so `cargo test` + a dry
//!      run on a Mac still exercise the chain.
//!   2. **arch** — the prebuilt service binaries target aarch64 only.
//!   3. **profile** — `--profile` wins, else the persisted
//!      `/etc/ados/profile.conf` (YAML `profile: X` or legacy `profile=X`),
//!      else the `drone` default. The resolved value lands in `ctx.profile`.
//!   4. **channel** — `--channel` wins, else `ctx.channel`'s existing default
//!      (`edge`). The resolved value lands back in `ctx.channel`.
//!   5. **armed** — an upgrade (or a reinstall over a present install) of a
//!      drone or ground-station node restarts the MAVLink, radio and video
//!      services, so it is refused while the vehicle is, or may be, armed,
//!      unless `--force-armed`. The verdict is the shared
//!      `ados_protocol::armed` rule: no fresh `armed: false` state fails closed.
//!   6. a best-effort connectivity note (logged, never fatal — `fetch_binaries`
//!      is the real network gate).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::ctx::Ctx;
use crate::env::{self, PROFILE_CONF};
use crate::exec;
use crate::graph::{Step, StepKind, StepOutcome};

/// Parse a `profile.conf` body into a normalized profile, accepting both the
/// YAML form (`profile: X`) and the legacy key=value form (`profile=X`). An
/// `auto` / unrecognized / empty value yields `None` (the caller defaults).
/// Pure — the heart of the profile resolution, unit-testable without a file.
pub fn parse_profile_conf(body: &str) -> Option<String> {
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip a leading `profile` key with either `:` or `=` separator.
        let rest = line
            .strip_prefix("profile")
            .map(|r| r.trim_start())
            .and_then(|r| r.strip_prefix([':', '=']))
            .map(|r| r.trim());
        let val = match rest {
            Some(v) => v.trim_matches('"').trim_matches('\'').trim(),
            None => continue,
        };
        let normalized = match val {
            "ground-station" | "ground_station" => "ground_station",
            "drone" => "drone",
            "workstation" => "workstation",
            "compute" => "compute",
            "visor" => "visor",
            // `auto` resolves at runtime; treat it as "no on-disk preference".
            _ => continue,
        };
        return Some(normalized.to_string());
    }
    None
}

/// Resolve the profile for this run (pure given the flag + the conf body):
/// the `--profile` flag wins; else a recognized `profile.conf` value; else
/// `drone`.
pub fn resolve_profile(flag: Option<&str>, profile_conf_body: Option<&str>) -> String {
    if let Some(p) = flag {
        // The CLI already normalized the flag spelling at the parse boundary.
        return p.to_string();
    }
    if let Some(body) = profile_conf_body {
        if let Some(p) = parse_profile_conf(body) {
            return p;
        }
    }
    "drone".to_string()
}

/// True when the process is running as root. On a non-Linux dev host there is
/// no install to do, so this is inert (returns true) — the OS-touching steps
/// are themselves Linux-gated.
#[cfg(target_os = "linux")]
fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

#[cfg(not(target_os = "linux"))]
fn is_root() -> bool {
    true
}

/// Host validation + profile/channel resolution gate.
pub struct Preflight;

impl Step for Preflight {
    fn id(&self) -> &str {
        "preflight"
    }
    fn requires(&self) -> &[&str] {
        &[]
    }
    fn checkpoint(&self) -> Option<&str> {
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Required
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        // 1. root.
        if !is_root() {
            return StepOutcome::Failed(
                "must run as root (re-run with sudo / curl ... | sudo bash)".to_string(),
            );
        }

        // 2. arch — aarch64 installs from the published release binaries;
        // x86_64 installs from locally built binaries (`--artifacts`, enforced
        // in fetch_binaries). macOS hosts take the separate build-from-source
        // install path instead.
        if !env::is_supported_arch() {
            return StepOutcome::Failed(format!(
                "unsupported architecture {}: Linux supports aarch64 (release \
                 binaries) and x86_64 (locally built, --channel edge --artifacts <dir>)",
                ctx.env.arch
            ));
        }

        // 3. profile: --profile wins, else profile.conf, else drone.
        let conf_body = std::fs::read_to_string(PROFILE_CONF).ok();
        let profile = resolve_profile(ctx.args.profile.as_deref(), conf_body.as_deref());
        tracing::info!(profile = %profile, "resolved install profile");
        ctx.profile = profile;

        // 4. channel: --channel wins, else the ctx default (edge). The ctx
        // already carries the flag-or-default value from `Ctx::from_args`; we
        // only re-affirm + log it here for the operator.
        if let Some(c) = ctx.args.channel.clone() {
            ctx.channel = c;
        }
        tracing::info!(channel = %ctx.channel, "resolved release channel");

        // 5. armed interlock — an upgrade restarts the link-carrying services.
        let upgrading = ctx.args.upgrade || env::probe_install_present();
        if let Some(why) = armed_refusal(
            upgrading,
            ctx.args.force_armed,
            &ctx.profile,
            &state_socket_path(),
        ) {
            return StepOutcome::Failed(why);
        }

        // 6. connectivity note — best-effort, never fatal. `fetch_binaries`
        // is the real network gate; this is just an early operator hint.
        note_connectivity();

        StepOutcome::Ok
    }
}

/// Log a best-effort reachability note for the GitHub release host. Never
/// fatal: a transient DNS hiccup here must not abort the install, and the real
/// fetch step retries IPv4-resilient. Skipped silently when `curl` is absent.
fn note_connectivity() {
    // A 5 s HEAD against the releases host. We only log the outcome.
    let res = exec::run(
        "curl",
        &[
            "-fsS",
            "-I",
            "--max-time",
            "5",
            "https://github.com",
            "-o",
            "/dev/null",
        ],
    );
    if !res.spawned {
        tracing::debug!("curl not present for the connectivity note; skipping");
    } else if res.success() {
        tracing::info!("connectivity to github.com OK");
    } else {
        tracing::warn!(
            "connectivity note: github.com not reachable right now (the fetch step retries)"
        );
    }
}

/// True when `/etc/ados/profile.conf` exists (used by status/diagnostics).
pub fn profile_conf_present() -> bool {
    Path::new(PROFILE_CONF).exists()
}

/// How long the one-shot vehicle-state read waits for a frame.
const STATE_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// The router's vehicle-state socket, under the agent's run directory
/// (`ADOS_RUN_DIR`, default `/run/ados`), the same resolution the services use.
fn state_socket_path() -> PathBuf {
    std::env::var_os("ADOS_RUN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/ados"))
        .join("state.sock")
}

/// Whether a profile carries a vehicle link an upgrade would drop. A visor's
/// MAVLink peer is a handheld sensor hub, never an aircraft in flight.
pub fn profile_carries_vehicle(profile: &str) -> bool {
    matches!(profile, "drone" | "ground_station")
}

/// Read one vehicle-state snapshot from `state_socket` and apply the shared
/// armed rule. Steps run synchronously on the installer's `#[tokio::main]`
/// thread, where a nested `block_on` panics, so the read runs on a thread of
/// its own with its own runtime. A runtime that cannot be built, or a read
/// thread that dies, proves nothing, so it fails closed like an absent socket.
fn vehicle_armed(state_socket: &Path) -> bool {
    let path = state_socket.to_path_buf();
    let read = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        Some(rt.block_on(ados_protocol::armed::read_state_snapshot(
            &path,
            STATE_READ_TIMEOUT,
        )))
    });
    match read.join() {
        Ok(Some(snapshot)) => ados_protocol::armed::vehicle_treated_as_armed(snapshot),
        _ => true,
    }
}

/// The refusal for an upgrade while the vehicle is, or may be, armed, or
/// `None` when the run may proceed. Only an upgrade of a vehicle-carrying
/// profile is checked; `--force-armed` overrides.
pub fn armed_refusal(
    upgrading: bool,
    force_armed: bool,
    profile: &str,
    state_socket: &Path,
) -> Option<String> {
    if !upgrading || !profile_carries_vehicle(profile) {
        return None;
    }
    if !vehicle_armed(state_socket) {
        return None;
    }
    if force_armed {
        tracing::warn!("vehicle is armed or its arm state is unknown; proceeding (--force-armed)");
        return None;
    }
    Some(
        "refusing to upgrade: the vehicle is armed or its arm state is unknown \
         (the upgrade restarts the MAVLink, radio and video services). Disarm \
         and retry, or pass --force-armed to proceed anyway"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_yaml_form() {
        assert_eq!(
            parse_profile_conf("profile: drone\n").as_deref(),
            Some("drone")
        );
        assert_eq!(
            parse_profile_conf("profile: ground_station\n").as_deref(),
            Some("ground_station")
        );
    }

    #[test]
    fn parse_legacy_kv_form() {
        assert_eq!(
            parse_profile_conf("profile=drone").as_deref(),
            Some("drone")
        );
        assert_eq!(
            parse_profile_conf("profile=ground-station").as_deref(),
            Some("ground_station")
        );
    }

    #[test]
    fn parse_strips_quotes_and_ignores_comments() {
        let body = "# a comment\nprofile: \"ground_station\"\n";
        assert_eq!(parse_profile_conf(body).as_deref(), Some("ground_station"));
    }

    #[test]
    fn parse_recognizes_workstation_compute_and_visor() {
        // A re-install of a workstation/compute/visor node must keep its
        // profile from profile.conf rather than collapsing to drone.
        assert_eq!(
            parse_profile_conf("profile: workstation\n").as_deref(),
            Some("workstation")
        );
        assert_eq!(
            parse_profile_conf("profile=compute").as_deref(),
            Some("compute")
        );
        assert_eq!(
            parse_profile_conf("profile: visor\n").as_deref(),
            Some("visor")
        );
        assert_eq!(resolve_profile(None, Some("profile=visor")), "visor");
    }

    #[test]
    fn parse_auto_and_garbage_are_none() {
        assert_eq!(parse_profile_conf("profile: auto\n"), None);
        assert_eq!(parse_profile_conf("profile: nonsense\n"), None);
        assert_eq!(parse_profile_conf("nothing here\n"), None);
        assert_eq!(parse_profile_conf(""), None);
    }

    #[test]
    fn resolve_prefers_flag_then_conf_then_default() {
        // Flag wins outright.
        assert_eq!(
            resolve_profile(Some("ground_station"), Some("profile: drone")),
            "ground_station"
        );
        // No flag → conf value.
        assert_eq!(
            resolve_profile(None, Some("profile: ground_station")),
            "ground_station"
        );
        // No flag, no recognizable conf → drone default.
        assert_eq!(resolve_profile(None, Some("profile: auto")), "drone");
        assert_eq!(resolve_profile(None, None), "drone");
    }

    #[test]
    fn preflight_refuses_armed() {
        let dir = tempfile::tempdir().unwrap();
        // No router state socket: the arm state is unknown and fails closed.
        let sock = dir.path().join("state.sock");
        for profile in ["drone", "ground_station"] {
            let why = armed_refusal(true, false, profile, &sock).expect("refused");
            assert!(why.contains("armed"), "{why}");
            assert!(why.contains("--force-armed"), "{why}");
        }
        // A fresh install, and a profile with no vehicle link, are not checked.
        assert_eq!(armed_refusal(false, false, "drone", &sock), None);
        assert_eq!(armed_refusal(true, false, "workstation", &sock), None);
        assert_eq!(armed_refusal(true, false, "compute", &sock), None);
        assert_eq!(armed_refusal(true, false, "visor", &sock), None);
    }

    #[test]
    fn force_armed_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("state.sock");
        assert_eq!(armed_refusal(true, true, "drone", &sock), None);
        assert_eq!(armed_refusal(true, true, "ground_station", &sock), None);
    }

    /// The installer drives its steps from `#[tokio::main]`: the armed check
    /// must not nest a runtime (a nested `block_on` panicked every upgrade).
    #[tokio::test]
    async fn armed_check_runs_inside_a_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("state.sock");
        assert!(armed_refusal(true, false, "drone", &sock).is_some());
        assert_eq!(armed_refusal(true, true, "drone", &sock), None);
    }
}
