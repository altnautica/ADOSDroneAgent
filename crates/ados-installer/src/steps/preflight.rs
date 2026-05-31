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
//!   5. a best-effort connectivity note (logged, never fatal — `fetch_binaries`
//!      is the real network gate).

use std::path::Path;

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

        // 2. arch — the prebuilt binaries target aarch64 only.
        if !env::is_supported_arch() {
            return StepOutcome::Failed(format!(
                "unsupported architecture {}; the prebuilt agent binaries target aarch64",
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

        // 5. connectivity note — best-effort, never fatal. `fetch_binaries`
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
}
