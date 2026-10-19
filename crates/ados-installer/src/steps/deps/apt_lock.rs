//! Apt invocation that waits out a contended dpkg lock and finishes an
//! interrupted dpkg run once.

use std::time::{Duration, Instant};

use crate::exec;
use crate::ui::ProgressSink;

use super::apt;

/// dpkg options that answer a conffile question without asking: keep a file
/// the operator (or this installer) changed, take the maintainer's version of
/// one nobody touched. `DEBIAN_FRONTEND=noninteractive` silences debconf only;
/// a conffile conflict is dpkg's own prompt, and it waits on the terminal the
/// install is running in, forever.
const DPKG_NO_PROMPT: [&str; 2] = ["--force-confdef", "--force-confold"];

/// The full `env … apt-get …` argv for `args` (pure).
pub(super) fn apt_argv<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut argv: Vec<&str> = vec![
        "DEBIAN_FRONTEND=noninteractive",
        "apt-get",
        "-o",
        "Dpkg::Options::=--force-confdef",
        "-o",
        "Dpkg::Options::=--force-confold",
    ];
    argv.extend_from_slice(args);
    argv
}

/// Whether apt refused because a previous dpkg run was interrupted (a power
/// cut during first-boot unattended-upgrades, or during an earlier install).
/// apt's own advice is a manual `dpkg --configure -a`; the installer runs it.
fn is_dpkg_interrupted(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("dpkg was interrupted")
}

/// Finish the interrupted dpkg run, non-interactively.
fn repair_interrupted_dpkg(sink: &ProgressSink) {
    sink.activity(
        "deps",
        "finishing an interrupted package install".to_string(),
    );
    let mut argv = vec![
        "DEBIAN_FRONTEND=noninteractive",
        "dpkg",
        "--configure",
        "-a",
    ];
    argv.extend_from_slice(&DPKG_NO_PROMPT);
    let res = exec::run_streamed("env", &argv, |line| sink.sub_log("deps", line));
    tracing::info!(code = ?res.code, "dpkg --configure -a after an interrupted run");
}

/// How long to wait for another package manager to release the dpkg lock.
///
/// A freshly flashed board runs `unattended-upgrades` on first boot, which
/// holds this lock for anywhere from seconds to minutes. Failing the moment it
/// is contended aborts the whole install on a condition that resolves itself,
/// and the operator's only recourse is to run the one-liner again and hope for
/// better timing.
const APT_LOCK_WAIT: Duration = Duration::from_secs(180);

/// How often to retry while the lock is held.
const APT_LOCK_POLL: Duration = Duration::from_secs(3);

/// Whether this failure is the dpkg lock being held by somebody else, rather
/// than a real packaging problem.
///
/// Matched on the message because apt does not give it a distinct exit code —
/// every one of these is exit 100, the same as a genuinely broken index.
fn is_lock_contention(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("could not get lock")
        || s.contains("unable to acquire the dpkg frontend lock")
        || s.contains("temporarily unavailable")
        || (s.contains("is held by process") && s.contains("lock"))
}

/// Run an apt invocation, waiting out a contended dpkg lock and finishing an
/// interrupted dpkg run once.
///
/// Only those two are retried. A broken index, an unknown package or a failing
/// post-install script returns immediately, because those do not get better by
/// trying again and a silent retry loop would only delay the report.
pub(super) fn apt_waiting_for_lock(argv: &[&str], sink: &ProgressSink) -> crate::exec::CmdResult {
    let deadline = Instant::now() + APT_LOCK_WAIT;
    let mut announced = false;
    let mut repaired = false;
    loop {
        let res = apt(argv, sink);
        if res.success() || !res.spawned {
            return res;
        }
        if !repaired && is_dpkg_interrupted(&res.stderr) {
            repaired = true;
            repair_interrupted_dpkg(sink);
            continue;
        }
        if !is_lock_contention(&res.stderr) {
            return res;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                waited_s = APT_LOCK_WAIT.as_secs(),
                "dpkg lock still held; giving up"
            );
            return res;
        }
        if !announced {
            // Say what is happening. An install that looks hung for two minutes
            // with no explanation is indistinguishable from one that has died.
            sink.activity(
                "deps",
                "waiting for another package manager to finish".to_string(),
            );
            tracing::info!("dpkg lock held by another process; waiting");
            announced = true;
        }
        std::thread::sleep(APT_LOCK_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dpkg_lock_message_is_recognised_as_contention() {
        // The exact text a freshly flashed board produces while
        // unattended-upgrades still holds the lock. Getting this wrong aborts
        // the whole install on a condition that clears itself.
        assert!(is_lock_contention(
            "E: Could not get lock /var/lib/dpkg/lock-frontend. It is held by process 2341 (apt-get)"
        ));
        assert!(is_lock_contention(
            "E: Unable to acquire the dpkg frontend lock (/var/lib/dpkg/lock-frontend), is another process using it?"
        ));
        assert!(is_lock_contention(
            "E: Could not get lock /var/lib/apt/lists/lock - open (11: Resource temporarily unavailable)"
        ));
    }

    #[test]
    fn a_real_packaging_failure_is_not_treated_as_contention() {
        // These do not get better by waiting, and retrying them would only
        // delay the report by the whole lock timeout.
        assert!(!is_lock_contention(
            "E: Unable to locate package ados-nonesuch"
        ));
        assert!(!is_lock_contention(
            "E: The repository 'http://example.invalid stable Release' does not have a Release file."
        ));
        assert!(!is_lock_contention(
            "dpkg: error processing package foo (--configure)"
        ));
        assert!(!is_lock_contention(""));
    }

    #[test]
    fn apt_never_stops_to_ask_about_a_conffile() {
        // debconf is silenced by the frontend variable; the conffile question
        // is dpkg's own, and it would wait on the install's terminal forever.
        let argv = apt_argv(&["install", "-y", "curl"]);
        let joined = argv.join(" ");
        assert!(joined.contains("DEBIAN_FRONTEND=noninteractive"));
        assert!(joined.contains("-o Dpkg::Options::=--force-confdef"));
        assert!(joined.contains("-o Dpkg::Options::=--force-confold"));
        assert_eq!(&argv[argv.len() - 3..], &["install", "-y", "curl"]);
    }

    #[test]
    fn an_interrupted_dpkg_is_recognised_and_is_not_lock_contention() {
        let msg = "E: dpkg was interrupted, you must manually run 'sudo dpkg --configure -a' to correct the problem.";
        assert!(is_dpkg_interrupted(msg));
        assert!(!is_lock_contention(msg));
        assert!(!is_dpkg_interrupted("E: Unable to locate package foo"));
    }

    #[test]
    fn the_wait_is_bounded_and_long_enough_to_outlast_first_boot_upgrades() {
        // Unbounded would hang an install forever behind a stuck package
        // manager; too short and it does not outlast the thing it waits for.
        assert!(APT_LOCK_WAIT >= Duration::from_secs(60));
        assert!(APT_LOCK_WAIT <= Duration::from_secs(600));
        assert!(APT_LOCK_POLL < APT_LOCK_WAIT);
    }
}
