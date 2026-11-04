//! `ados-supervisor switch-profile`: bring the node's units onto the profile its
//! config now names, the runtime half of a profile change.
//!
//! A profile change through the setup API persists `agent.profile`, then runs
//! this as a transient unit (`systemd-run`): the agent's own services are
//! `PartOf=` the supervisor, so the sequence has to run outside them to survive
//! stopping it, and outside the API's sandbox to write the unit links. It does
//! what an install of that profile does to the units, in the same order:
//!
//! 1. stop the supervisor, which stops every `PartOf=` unit with it. While the
//!    previous profile's supervisor runs it restarts any unit it supervises the
//!    moment that unit dies, so nothing below can stick before it is down;
//! 2. stop and disable the other profile's units;
//! 3. enable the profile's own units;
//! 4. start the supervisor (a start, never a restart: a restart restarts every
//!    still-active `PartOf=` unit and carries it into the new profile);
//! 5. kick the profile's start set with `--no-block`.
//!
//! Before any of that it points `profile.conf` at the profile, as an install
//! does: a later bare `--upgrade` takes its profile from that file, and a stale
//! one would reinstall the node as the profile it just left.
//!
//! Every list comes from the unit table (`ados_protocol::units`), the same lists
//! the installer acts on.

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use ados_protocol::units;

use crate::config::AgentConfig;

/// The supervisor's unit.
const SUPERVISOR: &str = "ados-supervisor.service";
/// Where the unit files are deployed.
const SYSTEMD_DIR: &str = "/etc/systemd/system";
/// Lets the API's reply to the request that dispatched the switch flush before
/// the supervisor (and so the API) goes down.
const REPLY_GRACE: Duration = Duration::from_secs(1);

/// Run the switch for `profile` (underscore form) through an injected
/// `systemctl` runner. `deployed` answers whether a unit file is present: a
/// unit this node does not ship is not enabled. Returns whether the
/// supervisor started.
pub fn switch_units(
    profile: &str,
    deployed: &dyn Fn(&str) -> bool,
    systemctl: &mut dyn FnMut(&[&str]) -> bool,
) -> bool {
    systemctl(&["stop", SUPERVISOR]);
    for unit in units::other_profile_units(profile) {
        systemctl(&["stop", unit]);
        systemctl(&["disable", unit]);
    }
    for unit in units::profile_enable_units(profile) {
        if deployed(unit) {
            systemctl(&["enable", unit]);
        }
    }
    let started = systemctl(&["start", SUPERVISOR]);
    for unit in units::start_units(profile) {
        systemctl(&["start", "--no-block", unit]);
    }
    started
}

/// `profile.conf` with its `profile:` line naming `profile`, every other line
/// (the install channel, a pinned version) kept. `None` when it already does.
pub fn profile_conf_with(body: &str, profile: &str) -> Option<String> {
    let line = format!("profile: {profile}");
    let mut found = false;
    let mut out: Vec<String> = Vec::new();
    for l in body.lines() {
        if l.trim_start().starts_with("profile:") || l.trim_start().starts_with("profile=") {
            if found {
                continue;
            }
            found = true;
            if l.trim() == line {
                out.push(l.to_string());
            } else {
                out.push(line.clone());
            }
        } else {
            out.push(l.to_string());
        }
    }
    if !found {
        out.insert(0, line);
    }
    let mut joined = out.join("\n");
    joined.push('\n');
    (joined != body).then_some(joined)
}

/// Point the node's `profile.conf` at `profile` (write-then-rename).
fn persist_profile_conf(profile: &str) {
    let path = ados_config::profile_conf_path();
    let body = std::fs::read_to_string(&path).unwrap_or_default();
    let Some(updated) = profile_conf_with(&body, profile) else {
        return;
    };
    let tmp = path.with_extension("conf.switch");
    let res = std::fs::write(&tmp, updated).and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        eprintln!(
            "ados-supervisor switch-profile: writing {} failed: {e}",
            path.display()
        );
    }
}

/// The `switch-profile` entry: resolve the profile exactly as the supervisor
/// will at its start, then run the switch against the host's systemd.
pub fn run() -> ExitCode {
    let profile = AgentConfig::load_quiet().profile_gate();
    eprintln!("ados-supervisor switch-profile: switching units to {profile}");
    std::thread::sleep(REPLY_GRACE);
    persist_profile_conf(&profile);
    let deployed = |unit: &str| Path::new(SYSTEMD_DIR).join(unit).exists();
    let mut systemctl = |args: &[&str]| {
        let ok = std::process::Command::new("systemctl")
            .args(args)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "ados-supervisor switch-profile: systemctl {} failed",
                args.join(" ")
            );
        }
        ok
    };
    if switch_units(&profile, &deployed, &mut systemctl) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILES: [&str; 5] = ["drone", "ground_station", "workstation", "compute", "visor"];

    fn calls_for(profile: &str) -> Vec<String> {
        let mut calls = Vec::new();
        assert!(switch_units(profile, &|_| true, &mut |args| {
            calls.push(args.join(" "));
            true
        }));
        calls
    }

    /// Regression: a runtime profile change only restarted the supervisor, so
    /// the previous profile's units stayed enabled, and any still running were
    /// carried over by the restart (a ground station switched to a drone kept
    /// its media server on the ports the drone's binds).
    #[test]
    fn the_switch_matches_an_install_of_the_profile() {
        for profile in PROFILES {
            let calls = calls_for(profile);
            let pos = |c: &str| calls.iter().position(|x| x == c);
            let stop_sup = pos("stop ados-supervisor.service").unwrap();
            let start_sup = pos("start ados-supervisor.service").unwrap();
            assert_eq!(stop_sup, 0, "{profile}");
            assert!(calls.iter().all(|c| !c.starts_with("restart")), "{profile}");
            for unit in units::other_profile_units(profile) {
                let stop = pos(&format!("stop {unit}")).unwrap();
                let disable = pos(&format!("disable {unit}")).unwrap();
                assert!(
                    stop_sup < stop && stop < disable && disable < start_sup,
                    "{unit}"
                );
            }
            for unit in units::profile_enable_units(profile) {
                assert!(
                    pos(&format!("enable {unit}")).unwrap() < start_sup,
                    "{unit}"
                );
            }
            for unit in units::start_units(profile) {
                assert!(
                    pos(&format!("start --no-block {unit}")).unwrap() > start_sup,
                    "{unit}"
                );
            }
        }
        let drone = calls_for("drone");
        assert!(drone.contains(&"disable ados-mediamtx-gs.service".to_string()));
        let gs = calls_for("ground_station");
        assert!(gs.contains(&"disable ados-video.service".to_string()));
        assert!(gs.contains(&"enable ados-mediamtx-gs.service".to_string()));
    }

    #[test]
    fn an_undeployed_unit_is_not_enabled_and_a_failed_start_is_reported() {
        let mut calls = Vec::new();
        let started = switch_units(
            "ground_station",
            &|u| u != "ados-kiosk.service",
            &mut |args| {
                calls.push(args.join(" "));
                args != ["start", SUPERVISOR]
            },
        );
        assert!(!started);
        assert!(!calls.contains(&"enable ados-kiosk.service".to_string()));
        assert!(calls.contains(&"enable ados-wfb-rx.service".to_string()));
    }

    /// A switch left `profile.conf` naming the old profile, which a later bare
    /// `--upgrade` would have reinstalled the node as.
    #[test]
    fn profile_conf_names_the_new_profile_and_keeps_the_rest() {
        let body = "profile: ground_station\nchannel: edge\nversion: 0.108.2\n";
        assert_eq!(
            profile_conf_with(body, "drone").as_deref(),
            Some("profile: drone\nchannel: edge\nversion: 0.108.2\n")
        );
        assert_eq!(
            profile_conf_with("profile: drone\nchannel: edge\n", "drone"),
            None
        );
        // The legacy `key=value` line is replaced; an absent file gains one.
        assert_eq!(
            profile_conf_with("profile=drone\n", "visor").as_deref(),
            Some("profile: visor\n")
        );
        assert_eq!(
            profile_conf_with("", "drone").as_deref(),
            Some("profile: drone\n")
        );
    }
}
