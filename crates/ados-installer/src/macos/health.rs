//! The post-install health gate: control-surface 2xx and launchd steady state.

use std::time::{Duration, Instant};

use ados_term::exec;

use super::launchd::label_for;
use super::{HealthReport, SteadyOutcome, CONTROL_PORT, DAEMONS};

/// Prove the whole workstation node came up, not merely that the control port
/// answers. Every daemon in [`DAEMONS`] must be loaded + running under launchd
/// AND stay running across a re-sample (a `ados-logd` that crash-loops on its DB —
/// the original silent-failure class — is caught by the pid changing / vanishing),
/// and `ados-control` must return a 2xx from `/api/status` (not just any HTTP
/// code).
pub(super) fn health_poll(uid: u32) -> HealthReport {
    // The control surface is the slowest to answer (it builds its full router), so
    // waiting for its 2xx also gives the other daemons time to reach steady state
    // before the launchd re-sample below.
    let control_ok = wait_control_2xx(CONTROL_PORT);

    // Wait for each launchd daemon to reach STEADY STATE — the same running pid
    // held across several consecutive samples. A fresh KeepAlive job commonly
    // restarts once or twice while it initializes (it waits on a socket, a
    // dependency, or its DB before it settles), so a single before/after snapshot
    // is too fragile: it false-fails a healthy node that merely settled during the
    // 2s window (a Rule-44 lying surface — a false "down" is as bad as a false
    // "up"). Instead each daemon gets a settle window to stop restarting. Only a
    // daemon that never holds a stable pid within the window — a genuine
    // crash-loop, e.g. `ados-logd` on an unwritable DB path, which exits on
    // startup and never accumulates the streak — or one that is never running
    // fails the gate.
    let labels: Vec<(&str, String)> = DAEMONS
        .iter()
        .map(|d| (d.name, label_for(d.name)))
        .collect();

    let mut failures = Vec::new();
    for (name, label) in &labels {
        match wait_daemon_steady(uid, label) {
            SteadyOutcome::Stable => {
                // Combine the launchd steady-state proof with the control
                // surface's functional proof.
                if *name == "ados-control" && !control_ok {
                    failures.push(format!(
                        "{name}: not returning 2xx on :{CONTROL_PORT}/api/status"
                    ));
                }
            }
            SteadyOutcome::CrashLooping => failures.push(format!(
                "{name}: still restarting after {}s, crash-looping",
                STEADY_DEADLINE.as_secs()
            )),
            SteadyOutcome::NeverRunning => {
                failures.push(format!("{name}: not running under launchd"))
            }
        }
    }

    if failures.is_empty() {
        println!(
            "  health: all {} daemons up (control 2xx, launchd running+stable)",
            DAEMONS.len()
        );
    } else {
        println!("  health: FAILED");
        for f in &failures {
            println!("    - {f}");
        }
    }
    HealthReport {
        up: failures.is_empty(),
        failures,
    }
}

/// Consecutive identical running-pid samples that prove a daemon has settled.
/// At the sample cadence below this is ~2.4s of no restart — long enough that an
/// immediately-crash-looping daemon (which exits within ms and is throttled by
/// launchd to a ≥10s respawn interval) can never accumulate the streak, while a
/// daemon that restarted once during init reaches it quickly.
const STEADY_SAMPLES: u32 = 4;

/// The gap between pid samples while waiting for steady state.
const STEADY_SAMPLE_GAP: Duration = Duration::from_millis(600);

/// The per-daemon settle deadline. Comfortably longer than launchd's default
/// ≥10s respawn throttle so a crash-looping daemon shows at least one not-running
/// gap (which resets the streak) within the window; a healthy daemon settles in
/// ~2.4s so the common case returns fast.
const STEADY_DEADLINE: Duration = Duration::from_secs(20);

/// Poll a daemon's launchd pid until it holds steady (`STEADY_SAMPLES` consecutive
/// identical running pids) or the settle deadline elapses. A changed or vanished
/// pid resets the streak, so only a daemon that stops restarting passes.
fn wait_daemon_steady(uid: u32, label: &str) -> SteadyOutcome {
    let start = Instant::now();
    let mut last: Option<i64> = None;
    let mut streak: u32 = 0;
    let mut ever_running = false;
    loop {
        match daemon_pid(uid, label) {
            Some(pid) => {
                ever_running = true;
                if last == Some(pid) {
                    streak += 1;
                } else {
                    last = Some(pid);
                    streak = 1;
                }
                if streak >= STEADY_SAMPLES {
                    return SteadyOutcome::Stable;
                }
            }
            None => {
                // Not currently running — between KeepAlive respawns, or never
                // started. Either way the settle streak breaks.
                last = None;
                streak = 0;
            }
        }
        if start.elapsed() >= STEADY_DEADLINE {
            return if ever_running {
                SteadyOutcome::CrashLooping
            } else {
                SteadyOutcome::NeverRunning
            };
        }
        std::thread::sleep(STEADY_SAMPLE_GAP);
    }
}

/// Poll `GET /api/status` until it returns a 2xx (bounded ~30s). A 2xx — not just
/// any completed HTTP code — is the proof the control surface is actually serving,
/// so a `500`/`502` from a control daemon that came up but cannot answer does not
/// pass the gate. Connection-refused (daemon not listening) exits curl non-zero.
fn wait_control_2xx(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/api/status");
    print!("  health: waiting for ados-control (2xx) on :{port} ");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    for _ in 0..40 {
        let res = exec::run(
            "curl",
            &[
                "-sS",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                "--max-time",
                "3",
                &url,
            ],
        );
        if res.success() {
            if let Ok(code) = res.stdout.trim().parse::<u16>() {
                if (200..300).contains(&code) {
                    println!("ok (HTTP {code})");
                    return true;
                }
            }
        }
        print!(".");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_millis(750));
    }
    println!(" (no 2xx)");
    false
}

/// The running pid of a launchd job, or `None` when it is not loaded or not
/// currently running. `launchctl print gui/<uid>/<label>` exits zero while the job
/// is loaded and prints the job's own `state = running` + `pid = N` in its header
/// block while its process is alive; a crash-looping job shows a changing pid (or a
/// not-running state) across samples, which is how the health gate catches it.
///
/// The job's `state`/`pid` are the FIRST such lines in the header. The rest of the
/// output nests sub-dictionaries (endpoints, sockets, spawn env) that carry their
/// OWN `state = active`/`state = inactive` lines — reading every `state =` line
/// would let a trailing nested `state = active` clobber the job's `running` flag to
/// `false` and make a healthy daemon read as down (a Rule-44 lying surface). So take
/// only the first occurrence of each.
pub(super) fn daemon_pid(uid: u32, label: &str) -> Option<i64> {
    let target = format!("gui/{uid}/{label}");
    let res = exec::run("launchctl", &["print", &target]);
    if !res.success() {
        return None;
    }
    parse_daemon_state(&res.stdout)
}

/// Parse the running pid out of `launchctl print` output: the first `state =`
/// (the job's own state) must be `running`, and the first `pid =` is the job pid.
/// Split out so the nested-`state`-clobber regression is unit-testable without a
/// live launchd.
pub(super) fn parse_daemon_state(stdout: &str) -> Option<i64> {
    let mut running: Option<bool> = None;
    let mut pid: Option<i64> = None;
    for line in stdout.lines() {
        let t = line.trim();
        if running.is_none() {
            if let Some(v) = t.strip_prefix("state = ") {
                running = Some(v.trim() == "running");
                continue;
            }
        }
        if pid.is_none() {
            if let Some(v) = t.strip_prefix("pid = ") {
                pid = v.trim().parse::<i64>().ok();
            }
        }
    }
    if running == Some(true) {
        pid
    } else {
        None
    }
}
