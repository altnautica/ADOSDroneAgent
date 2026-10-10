//! systemd backend: thin async wrappers over the `systemctl` binary.
//!
//! The supervisor orchestrates systemd and never spawns a service process
//! itself, so every lifecycle action funnels through here. A missing
//! `systemctl` (e.g. a non-Linux dev host) or a timeout is treated as a soft
//! failure: an action returns `false`, and a probe returns no verdict (`None`)
//! rather than a fabricated "inactive".

use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;
use tokio::time::timeout;

use super::ProcessManager;

const ACT_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

async fn run(args: &[&str], dur: Duration) -> Option<std::process::Output> {
    // `kill_on_drop`: a `systemctl` blocked past the ceiling (a start job queued
    // behind a slow unit) is reaped on the timeout path instead of surviving as
    // one leaked process per retry.
    let child = Command::new("systemctl")
        .args(args)
        .kill_on_drop(true)
        .output();
    match timeout(dur, child).await {
        Ok(Ok(out)) => Some(out),
        Ok(Err(_)) => None, // spawn error (systemctl missing)
        Err(_) => None,     // timed out
    }
}

/// Map `systemctl is-active` output to a verdict. `active` is running; any
/// other state word (`inactive`, `failed`, `activating`, …) is a definite
/// not-running answer; empty output is no answer at all. Pure for testing.
fn is_active_verdict(stdout: &str) -> Option<bool> {
    match stdout.trim() {
        "" => None,
        "active" => Some(true),
        _ => Some(false),
    }
}

fn ok(out: &Option<std::process::Output>) -> bool {
    out.as_ref().map(|o| o.status.success()).unwrap_or(false)
}

/// How `systemctl show` reports a stopped unit's last run.
#[derive(Debug, PartialEq, Eq)]
enum ShownEnd {
    /// The unit still holds its runtime record (it is referenced, failed, or
    /// waiting out `RestartSec=`): systemd's own verdict on the last run, clean
    /// only for `Result=success` with the main process exited
    /// (`ExecMainCode=1`, CLD_EXITED) with status 0.
    Live(bool),
    /// No runtime record: `InvocationID` is empty and the exec fields are the
    /// defaults of a never-run unit. systemd garbage-collects an unreferenced
    /// unit once it ends inactive with a success result and no restart
    /// pending, and every supervisor-managed unit is unreferenced (disabled),
    /// so this is the normal shape right after a clean exit.
    /// `exit_zero_only` holds when the restart policy leaves exit 0 as the
    /// only such end apart from a stop job: `Restart=always` restarts after
    /// every other exit status and every signal, clean ones included, and
    /// `RestartPreventExitStatus=0` exempts exactly status 0.
    Collected { exit_zero_only: bool },
}

/// The `systemctl show` properties [`shown_end`] reads.
const SHOW_END_PROPERTIES: [&str; 7] = [
    "--property=LoadState",
    "--property=InvocationID",
    "--property=Result",
    "--property=ExecMainCode",
    "--property=ExecMainStatus",
    "--property=Restart",
    "--property=RestartPreventExitStatus",
];

/// Map `systemctl show` output for [`SHOW_END_PROPERTIES`] to the unit's last
/// end. `None` for a unit that is not loaded (not found, masked) or output
/// missing a property. Pure for testing.
fn shown_end(stdout: &str) -> Option<ShownEnd> {
    let mut load = None;
    let mut invocation = None;
    let mut result = None;
    let mut code = None;
    let mut status = None;
    let mut restart = None;
    let mut prevent = None;
    for line in stdout.lines() {
        match line.trim().split_once('=') {
            Some(("LoadState", v)) => load = Some(v),
            Some(("InvocationID", v)) => invocation = Some(v),
            Some(("Result", v)) => result = Some(v),
            Some(("ExecMainCode", v)) => code = v.parse::<i32>().ok(),
            Some(("ExecMainStatus", v)) => status = v.parse::<i32>().ok(),
            Some(("Restart", v)) => restart = Some(v),
            Some(("RestartPreventExitStatus", v)) => prevent = Some(v),
            _ => {}
        }
    }
    if load? != "loaded" {
        return None;
    }
    if invocation?.is_empty() {
        let exit_zero_only =
            restart? == "always" && prevent?.split_whitespace().eq(std::iter::once("0"));
        return Some(ShownEnd::Collected { exit_zero_only });
    }
    Some(ShownEnd::Live(
        result? == "success" && code? == 1 && status? == 0,
    ))
}

/// systemd's catalog ids for the unit lifecycle records PID 1 writes to the
/// journal, each tagged with the unit's `INVOCATION_ID`.
mod catalog {
    pub const STARTING: &str = "7d4958e842da4a758f6c1cdc7b36dcc5";
    pub const STARTED: &str = "39f53479d3a045ac8e11786248231fbf";
    pub const STOPPING: &str = "de5b426a63be47a7b6ac3eaac82e2f6f";
    pub const STOPPED: &str = "9d1aaa27d60140bd96365438aad20286";
    pub const FAILED: &str = "be02cf6855d2428ba40df7e9d022f03d";
    /// "Deactivated successfully": the run ended with a success result.
    pub const SUCCESS: &str = "7ad2d189f7e94e70a38c781354912448";
    /// "Main process exited, code=…, status=…": logged only for an unclean end
    /// (a clean one is logged at debug, below the default level).
    pub const PROCESS_EXIT: &str = "98e322203f7a4ed290d09fe03c09fe15";
    pub const FAILURE_RESULT: &str = "d9b373ed55a64feb8242e02dbe79a49c";
    pub const RESTART_SCHEDULED: &str = "5eb03494b6584870a536b337290809b3";

    pub const LIFECYCLE: [&str; 9] = [
        STARTING,
        STARTED,
        STOPPING,
        STOPPED,
        FAILED,
        SUCCESS,
        PROCESS_EXIT,
        FAILURE_RESULT,
        RESTART_SCHEDULED,
    ];
}

/// How many of the unit's newest lifecycle records to read. One run writes at
/// most five (starting, started, stopping, end, stopped).
const JOURNAL_WINDOW: &str = "16";

/// Judge the newest run from PID 1's lifecycle records for the unit, given
/// newest first as `journalctl -o json` lines. The newest record names the
/// run: an empty invocation there (a start skipped on a failed condition) has
/// no run to judge. That run is clean only when it logged a success end and
/// no stop job, unclean end or scheduled restart. `None` when it has no end
/// recorded yet or a line does not parse. Pure for testing.
fn journal_end_verdict(stdout: &str) -> Option<bool> {
    fn field<'a>(r: &'a serde_json::Value, k: &str) -> Option<&'a str> {
        r.get(k).and_then(serde_json::Value::as_str)
    }
    let records = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let run = field(records.first()?, "INVOCATION_ID").filter(|id| !id.is_empty())?;
    let mut ended_clean = false;
    for r in records
        .iter()
        .filter(|r| field(r, "INVOCATION_ID") == Some(run))
    {
        if field(r, "JOB_TYPE") == Some("stop") {
            return Some(false);
        }
        match field(r, "MESSAGE_ID") {
            Some(
                catalog::PROCESS_EXIT
                | catalog::FAILURE_RESULT
                | catalog::FAILED
                | catalog::RESTART_SCHEDULED,
            ) => return Some(false),
            Some(catalog::SUCCESS) => ended_clean = true,
            _ => {}
        }
    }
    ended_clean.then_some(true)
}

/// The journal's `UNIT=` value for a unit the catalog names without a suffix.
fn service_unit_name(unit: &str) -> String {
    if unit.ends_with(".service") {
        unit.to_owned()
    } else {
        format!("{unit}.service")
    }
}

/// The end of the unit's newest run in this boot, from PID 1's journal records.
/// `journalctl --sync` first: PID 1 logged the end before the unit went
/// inactive, and the sync guarantees journald has stored everything sent
/// before it, so a record still in journald's queue is never missed.
async fn journal_end(unit: &str) -> Option<bool> {
    let _ = journalctl(&["--sync"]).await;
    let unit_match = format!("UNIT={}", service_unit_name(unit));
    let ids: Vec<String> = catalog::LIFECYCLE
        .iter()
        .map(|id| format!("MESSAGE_ID={id}"))
        .collect();
    let mut args = vec![
        "-o",
        "json",
        "-q",
        "--no-pager",
        "-b",
        "-r",
        "-n",
        JOURNAL_WINDOW,
        "--output-fields=MESSAGE_ID,INVOCATION_ID,JOB_TYPE",
        "_PID=1",
        &unit_match,
    ];
    args.extend(ids.iter().map(String::as_str));
    let out = journalctl(&args).await?;
    if !out.status.success() {
        return None;
    }
    journal_end_verdict(&String::from_utf8_lossy(&out.stdout))
}

async fn journalctl(args: &[&str]) -> Option<std::process::Output> {
    let child = Command::new("journalctl")
        .args(args)
        .kill_on_drop(true)
        .output();
    match timeout(PROBE_TIMEOUT, child).await {
        Ok(Ok(out)) => Some(out),
        _ => None,
    }
}

/// Drives service units via the `systemctl` binary.
pub struct SystemdManager;

#[async_trait]
impl ProcessManager for SystemdManager {
    /// `systemctl start <unit>`.
    async fn start(&self, unit: &str) -> bool {
        ok(&run(&["start", unit], ACT_TIMEOUT).await)
    }

    /// `systemctl stop <unit>`.
    async fn stop(&self, unit: &str) -> bool {
        ok(&run(&["stop", unit], ACT_TIMEOUT).await)
    }

    /// `systemctl restart <unit>` — the prompt path to a fresh spawn cycle (used
    /// after a key write so the wfb unit reloads the new key).
    async fn restart(&self, unit: &str) -> bool {
        ok(&run(&["restart", unit], ACT_TIMEOUT).await)
    }

    /// `systemctl try-restart <unit>` — restarts a running unit, leaves a
    /// stopped one stopped (exit 0 either way).
    async fn try_restart(&self, unit: &str) -> bool {
        ok(&run(&["try-restart", unit], ACT_TIMEOUT).await)
    }

    /// `systemctl reset-failed <unit>` — clears a `failed (start-limit-hit)`
    /// state + the burst counter so a following `start` is not a no-op.
    async fn reset_failed(&self, unit: &str) {
        let _ = run(&["reset-failed", unit], PROBE_TIMEOUT).await;
    }

    /// `systemctl is-active <unit>`: `Some(true)` only for exactly `active`,
    /// `None` when the probe timed out or could not be spawned.
    async fn is_active(&self, unit: &str) -> Option<bool> {
        let out = run(&["is-active", unit], PROBE_TIMEOUT).await?;
        is_active_verdict(&String::from_utf8_lossy(&out.stdout))
    }

    /// The work counter each judged unit publishes about its own lane (see
    /// [`crate::work_proof::read_work_counter`]). Never the process's
    /// `/proc/<pid>/io`: that counts only the `read`/`write` file path, which
    /// misses socket traffic and is kept moving by timer-driven sidecar writes.
    async fn work_counter(&self, unit: &str) -> Option<u64> {
        crate::work_proof::read_work_counter(unit).await
    }

    /// `systemctl show <unit> --property=Job --value`: the job id while a
    /// start, stop or restart is queued or running on the unit, empty when
    /// there is none. `is-active` cannot answer this: a unit whose restart job
    /// is still waiting on an ordering dependency reads `active` throughout.
    async fn job_pending(&self, unit: &str) -> Option<bool> {
        let out = run(&["show", unit, "--property=Job", "--value"], PROBE_TIMEOUT).await?;
        out.status
            .success()
            .then(|| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
    }

    /// The unit's last run as `systemctl show` keeps it; when systemd has
    /// already collected the unit (the normal case for a clean exit of a
    /// disabled unit), PID 1's journal records for that run. A collected unit
    /// whose restart policy leaves another success end possible gets no
    /// verdict, never a guessed clean exit.
    async fn exited_cleanly(&self, unit: &str) -> Option<bool> {
        let mut args = vec!["show", unit];
        args.extend(SHOW_END_PROPERTIES);
        let out = run(&args, PROBE_TIMEOUT).await?;
        if !out.status.success() {
            return None;
        }
        match shown_end(&String::from_utf8_lossy(&out.stdout))? {
            ShownEnd::Live(clean) => Some(clean),
            ShownEnd::Collected {
                exit_zero_only: true,
            } => journal_end(unit).await,
            ShownEnd::Collected {
                exit_zero_only: false,
            } => None,
        }
    }

    /// `systemctl show <unit> -p ConditionResult,ActiveState`, read through
    /// [`condition_verdict`].
    async fn condition_result(&self, unit: &str) -> Option<bool> {
        let out = run(
            &["show", unit, "-p", "ConditionResult,ActiveState"],
            PROBE_TIMEOUT,
        )
        .await?;
        if !out.status.success() {
            return None;
        }
        condition_verdict(&String::from_utf8_lossy(&out.stdout))
    }

    /// `systemctl mask <unit>` (idempotent).
    async fn mask(&self, unit: &str) {
        let _ = run(&["mask", unit], PROBE_TIMEOUT).await;
    }

    /// `systemctl unmask <unit>` (idempotent).
    async fn unmask(&self, unit: &str) {
        let _ = run(&["unmask", unit], PROBE_TIMEOUT).await;
    }
}

/// Map `systemctl show -p ConditionResult,ActiveState` output to whether the
/// unit's start conditions held. A unit that is running (or on its way) held
/// them whatever the record says; otherwise `ConditionResult=no` is a start
/// systemd skipped. Either property missing is no verdict. Pure for testing.
fn condition_verdict(stdout: &str) -> Option<bool> {
    let mut condition = None;
    let mut active = None;
    for line in stdout.lines() {
        match line.split_once('=') {
            Some(("ConditionResult", v)) => condition = Some(v.trim()),
            Some(("ActiveState", v)) => active = Some(v.trim()),
            _ => {}
        }
    }
    match (condition?, active?) {
        (_, "active" | "activating" | "reloading") => Some(true),
        ("no", _) => Some(false),
        ("yes", _) => Some(true),
        _ => None,
    }
}

#[cfg(test)]
#[path = "systemd_tests.rs"]
mod tests;
