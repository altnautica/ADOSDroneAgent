//! Service-lifecycle write routes: restart one `ados-*` unit + restart the
//! supervisor.
//!
//! `POST /api/services/{name}/restart` restarts a single named agent unit, and
//! `POST /api/v1/system/restart-supervisor` restarts the supervisor unit that
//! owns the whole agent process tree. Both shell `systemctl` through a bounded
//! `tokio::process` spawn. The property reads and the supervisor's
//! fire-and-forget restart go through [`crate::probe`]; the single-unit restart
//! calls `ados_proc::run::run` directly because it needs a non-zero exit, a
//! timeout and a spawn fault kept apart (the `RestartRun` outcomes), which the
//! probe seam deliberately folds into one degraded value.
//!
//! ## Why these answer HTTP 200 with a status field, not a 4xx
//!
//! Every outcome — success, an unknown service, a failed restart, a timeout —
//! is a plain JSON body at HTTP 200, with the verdict in a `status` (`restart`)
//! or `ok` (`restart-supervisor`) field. An unknown unit name is a
//! `{"status":"error", ...}` body at 200, NOT a 400. The GCS parses the body
//! field, so the envelope is the wire contract, not the status code. (These two
//! routes therefore do not use the crate's `{"detail"}` 4xx helper.)
//!
//! ## The restart-service name guard
//!
//! Only an `ados-*` unit can be restarted. A request name is normalized to
//! `ados-<name>` when it lacks the prefix, then checked against the unit
//! table's `api_restartable` column (`ados_protocol::units`); anything outside
//! it is rejected with an `Unknown service` error before any `systemctl` runs,
//! so an arbitrary unit (e.g. `sshd`, `nginx`) can never be restarted through
//! this route. The table is held to the shipped unit files, so a unit that is
//! not shipped cannot be allowed.
//!
//! The caller names the real unit. Each profile lists its own units on
//! `/api/services`, so a ground station's receive stack is `ados-wfb-rx`, never
//! the drone-side `ados-wfb`.
//!
//! ## The restart confirmation
//!
//! `systemctl restart` returning 0 is necessary but not sufficient — polkit on
//! Debian can silently no-op a restart. So after a 0 return the route samples the
//! unit's restart signal: a fresh `MainPID` for simple/notify/dbus/exec units, or
//! a changed `ActiveEnterTimestamp` for forking/oneshot/idle units, polling for
//! up to ~5 s. Only a confirmed signal yields `status:ok`.
//!
//! ## The supervisor-restart background fire
//!
//! Restarting the supervisor kills the very process serving the request, so the
//! route spawns the `systemctl restart ados-supervisor` on the runtime (after a
//! short delay that lets the reply flush) and returns `{"ok":true}` at once.
//! The route reports `ok:false` only when it cannot even schedule the restart
//! (no `systemctl` on PATH).

use std::time::Duration;

use axum::extract::Path;
use axum::Json;
use serde_json::{json, Value};

use ados_proc::run::RunOutcome;

/// Whether the restart route may touch `unit` (an `ados-<name>` stem): the
/// unit table's `api_restartable` column. The column refuses this process
/// itself (the reply could never be delivered), the supervisor (it has its
/// own route), and the boot oneshots that have nothing to restart.
pub fn restart_allowed(unit: &str) -> bool {
    ados_protocol::units::find(unit).is_some_and(|u| u.api_restartable)
}

/// Every unit the restart route may touch, as `ados-<name>` stems.
pub fn allowed_units() -> impl Iterator<Item = &'static str> {
    ados_protocol::units::UNITS
        .iter()
        .filter(|u| u.api_restartable)
        .map(|u| u.stem())
}

/// The supervisor unit `POST /api/v1/system/restart-supervisor` restarts. It owns
/// the agent process tree, so restarting it cycles every child through the same
/// lifecycle the install set up.
const SUPERVISOR_UNIT: &str = "ados-supervisor";

/// How long the restart-confirmation poll runs: 50 iterations × 100 ms ≈ 5 s.
/// The window is ~5 s because a slow Pi 4B under load (or a Pi Zero 2 W) takes
/// longer than 2 s to spawn a fresh MainPID after a restart.
const CONFIRM_ITERATIONS: u32 = 50;
const CONFIRM_SLEEP: Duration = Duration::from_millis(100);

/// The `systemctl restart` subprocess timeout.
/// It also bounds the supervisor restart: `probe::PROBE_TIMEOUT` is sized for a
/// read-only probe and would kill a healthy supervisor restart mid-flight.
const RESTART_TIMEOUT: Duration = Duration::from_secs(30);

/// `POST /api/services/{name}/restart` → `{"status": ...}` at HTTP 200.
///
/// Validates the unit name against the `ados-*` allowlist, then restarts the
/// unit and confirms the restart actually executed before reporting
/// `status:ok`. Every outcome is a 200 with the verdict in the body: an unknown
/// name, a failed restart, an unconfirmed restart, and a timeout are all
/// `{"status":"error", ...}` bodies, never an HTTP error.
pub async fn restart_service(Path(name): Path<String>) -> Json<Value> {
    Json(restart_service_result(&name).await)
}

/// Restart one allowlisted `ados-*` unit and return the same verdict body the
/// service-restart route produces, for the routes that need a restart as a side
/// effect of another write (e.g. the vision-detector selection restarting
/// `ados-vision`). The name flows through the identical allowlist guard and
/// restart-confirmation path, so a unit outside the fixed set is refused here
/// too; the caller folds the returned `{status, ...}` into its own response
/// without trusting an arbitrary unit name. `async` because the systemd
/// shell-outs underneath are bounded `tokio::process` spawns; a caller already
/// inside an axum handler awaits it.
pub async fn restart_unit(name: &str) -> Value {
    restart_service_result(name).await
}

/// The pure restart logic + the systemd shell-outs, factored out of the axum
/// handler so the name guard and the body shapes are testable without the HTTP
/// layer.
async fn restart_service_result(name: &str) -> Value {
    // Normalize to the `ados-<name>` form, then gate on the allowlist. The error
    // body echoes the *original* request name.
    let svc_name = if name.starts_with("ados-") {
        name.to_string()
    } else {
        format!("ados-{name}")
    };
    if !restart_allowed(&svc_name) {
        return json!({
            "status": "error",
            "message": format!("Unknown service: {name}"),
        });
    }
    perform_restart(&svc_name).await
}

/// Run the restart for an already-validated unit and confirm it executed.
/// Returns the `status:ok` body on a confirmed restart, an error body on a
/// non-zero `systemctl` return, an unconfirmed restart, or a subprocess fault.
async fn perform_restart(svc_name: &str) -> Value {
    let unit_type = show_value(svc_name, "Type").await;
    let unit_type = if unit_type.is_empty() {
        "simple".to_string()
    } else {
        unit_type
    };
    let pid_before = main_pid(svc_name).await;
    let ts_before = active_enter_ts(svc_name).await;

    // `systemctl restart` with the 30 s timeout. A spawn failure (no systemctl)
    // or a timeout each map to a `{"status":"error"}` body.
    match run_with_timeout("systemctl", &["restart", svc_name], RESTART_TIMEOUT).await {
        RestartRun::Completed { code, stderr } => {
            if code != 0 {
                let msg = if stderr.trim().is_empty() {
                    format!("Failed to restart {svc_name}")
                } else {
                    stderr.trim().to_string()
                };
                return json!({"status": "error", "message": msg});
            }
        }
        RestartRun::TimedOut => {
            return json!({
                "status": "error",
                "message": format!("Restart timed out for {svc_name}"),
            });
        }
        RestartRun::SpawnError(err) => {
            // A spawn fault (no systemctl binary) surfaces its error string.
            return json!({"status": "error", "message": err});
        }
    }

    confirm_restart(svc_name, &unit_type, pid_before, &ts_before).await
}

/// Poll for the unit's restart signal after a `systemctl restart` returned 0.
///
/// For simple/notify/dbus/exec units the signal is a fresh, non-zero `MainPID`
/// that differs from `pid_before`, with `ActiveState=active`, held unchanged for
/// [`PID_SETTLE_SAMPLES`] consecutive polls (see [`PidSettle`]); for every other
/// unit type (forking, oneshot, idle — whose MainPID is transient or zero) a
/// changed `ActiveEnterTimestamp` is the signal. Polls up to
/// [`CONFIRM_ITERATIONS`] × [`CONFIRM_SLEEP`]. A confirmed signal yields the
/// `status:ok` body (with the type-appropriate before/after fields); exhausting
/// the poll yields the "did not show a restart signal" error body. The inter-poll
/// wait is `tokio::time::sleep`, not a thread sleep: the loop can occupy the full
/// ~5 s window, which a thread sleep would take out of a reactor worker.
async fn confirm_restart(
    svc_name: &str,
    unit_type: &str,
    pid_before: i64,
    ts_before: &str,
) -> Value {
    let pid_based = matches!(unit_type, "simple" | "notify" | "dbus" | "exec");
    let mut settle = PidSettle::new(pid_before);

    for _ in 0..CONFIRM_ITERATIONS {
        tokio::time::sleep(CONFIRM_SLEEP).await;

        if pid_based {
            let pid_after = main_pid(svc_name).await;
            let active_state = show_value(svc_name, "ActiveState").await;
            if settle.observe(pid_after, &active_state) {
                return json!({
                    "status": "ok",
                    "message": format!("Restarted {svc_name}"),
                    "unit": svc_name,
                    "pid_before": pid_before,
                    "pid_after": pid_after,
                });
            }
            continue;
        }
        let ts_after = active_enter_ts(svc_name).await;
        if !ts_after.is_empty() && ts_after != ts_before {
            return json!({
                "status": "ok",
                "message": format!("Restarted {svc_name}"),
                "unit": svc_name,
                "active_enter_before": ts_before,
                "active_enter_after": ts_after,
            });
        }
    }

    json!({
        "status": "error",
        "message": format!(
            "systemctl returned 0 but {svc_name} did not show a restart signal \
             within {window}s (type={unit_type}, pid_before={pid_before}). \
             Likely a polkit/permission issue, a unit that crashes on start, or \
             one that takes longer than the polling window to spawn.",
            window = CONFIRM_ITERATIONS / 10
        ),
        "unit": svc_name,
    })
}

/// How many consecutive polls a new `MainPID` must hold, active, before a restart
/// counts as confirmed. A unit with `Restart=always` that crashes on start gets a
/// fresh PID on every attempt, so a single sighting proves nothing; three polls
/// ([`CONFIRM_SLEEP`] apart) outlast systemd's default 100 ms restart delay.
const PID_SETTLE_SAMPLES: u32 = 3;

/// The PID-based restart confirmation, one poll at a time. Pure, so the
/// crash-loop case is testable without systemd.
struct PidSettle {
    pid_before: i64,
    candidate: i64,
    seen: u32,
}

impl PidSettle {
    fn new(pid_before: i64) -> Self {
        Self {
            pid_before,
            candidate: 0,
            seen: 0,
        }
    }

    /// Feed one poll; `true` once the same fresh PID has been seen active for
    /// [`PID_SETTLE_SAMPLES`] polls in a row.
    fn observe(&mut self, pid_after: i64, active_state: &str) -> bool {
        let fresh = pid_after != 0 && pid_after != self.pid_before && active_state == "active";
        if !fresh {
            self.candidate = 0;
            self.seen = 0;
            return false;
        }
        if pid_after == self.candidate {
            self.seen += 1;
        } else {
            self.candidate = pid_after;
            self.seen = 1;
        }
        self.seen >= PID_SETTLE_SAMPLES
    }
}

/// Read one `systemctl show <unit> -p <prop> --value` property, trimmed. An empty
/// string on any spawn / non-UTF-8 / read error (a missing systemctl yields `""`
/// too).
///
/// Routed through [`crate::probe::capture_systemctl`], which additionally folds a
/// non-zero exit into `""`. That is the same answer the blocking version gave in
/// both reachable cases: `systemctl show -p <prop> --value` exits 0 even for a
/// unit systemd has never heard of (printing the property default), and when it
/// does exit non-zero — a host with systemctl present but systemd not on PID 1 —
/// it writes the reason to stderr and nothing to stdout, so the old
/// `stdout.trim()` was `""` as well.
///
/// The probe forces `LANG=C`, which pins the `ActiveEnterTimestamp` wording. The
/// confirmation only ever compares two reads taken through this same function,
/// so the comparison is unaffected; the before/after strings echoed in the
/// response body are simply locale-independent now.
async fn show_value(unit: &str, prop: &str) -> String {
    crate::probe::capture_systemctl(
        &["show", unit, "-p", prop, "--value"],
        crate::probe::PROBE_TIMEOUT,
    )
    .await
    .text()
    .trim()
    .to_string()
}

/// The unit's `MainPID` as an integer, or `0` when the property is empty or not a
/// number.
async fn main_pid(unit: &str) -> i64 {
    let raw = show_value(unit, "MainPID").await;
    raw.parse::<i64>().unwrap_or(0)
}

/// The unit's `ActiveEnterTimestamp` string (the systemd-formatted last-active time),
/// empty on any read error.
async fn active_enter_ts(unit: &str) -> String {
    show_value(unit, "ActiveEnterTimestamp").await
}

/// The outcome of a bounded `systemctl restart` run.
enum RestartRun {
    /// The process finished within the timeout: its exit code + captured stderr.
    Completed { code: i32, stderr: String },
    /// The process did not finish within the timeout (it was killed).
    TimedOut,
    /// The process could not be spawned (no `systemctl` on PATH): the error
    /// string.
    SpawnError(String),
}

/// Run `program args…`, capture its output, and bound it to a wall-clock
/// timeout. On the agent's boards `systemctl restart` settles well within the
/// timeout; this kills a wedged restart so the route returns the timeout body
/// rather than hanging the connection. A spawn failure (binary absent) is
/// reported distinctly so the caller can surface its error string.
async fn run_with_timeout(program: &str, args: &[&str], timeout: Duration) -> RestartRun {
    match ados_proc::run::run(program, args, &[], timeout).await {
        RunOutcome::Exited { code, stderr, .. } => RestartRun::Completed {
            code: code.unwrap_or(-1),
            stderr,
        },
        RunOutcome::SpawnFailed(e) | RunOutcome::WaitFailed(e) => RestartRun::SpawnError(e),
        RunOutcome::TimedOut => RestartRun::TimedOut,
    }
}

/// `POST /api/v1/system/restart-supervisor` → `{"ok": ..., "message": ...}` at
/// HTTP 200.
///
/// Schedules `systemctl restart ados-supervisor` and returns immediately, so the
/// HTTP response flushes before the unit kills the serving process. Reports
/// `ok:false` only when the restart cannot even be scheduled (no `systemctl` on
/// PATH).
pub async fn restart_supervisor() -> Json<Value> {
    if which_systemctl().is_none() {
        return Json(json!({
            "ok": false,
            "message": "systemctl binary not found on PATH",
        }));
    }

    // Fire the restart on the runtime after a short delay that lets this reply
    // flush, then return at once. The restart kills the agent process the
    // supervisor owns, so any post-spawn error is benign (the unit IS
    // restarting).
    tokio::spawn(async move {
        // The 200 ms delay is load-bearing: it is what lets the response above
        // reach the client before the supervisor restarts the process serving it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Bounded at RESTART_TIMEOUT, not `probe::PROBE_TIMEOUT`: a supervisor
        // restart cycles the whole agent process tree and legitimately runs far
        // longer than a 3 s read-only probe, so the shorter bound would kill a
        // healthy restart. The bound exists only so a wedged systemctl cannot
        // leave a child behind; the exit status is ignored either way, because
        // the restart kills the process that would read it.
        let argv = ["restart", SUPERVISOR_UNIT];
        let _ = crate::probe::status_only("systemctl", &argv, RESTART_TIMEOUT).await;
    });

    Json(json!({
        "ok": true,
        "message": "ados-supervisor restart scheduled",
    }))
}

/// Resolve a `systemctl` executable on `PATH`. Returns
/// the first `PATH` entry that holds an existing `systemctl` file, or `None` when
/// no entry does (a non-systemd dev host). Used by the supervisor route to pick
/// `ok:false` when it cannot schedule the restart at all.
fn which_systemctl() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("systemctl");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests;
