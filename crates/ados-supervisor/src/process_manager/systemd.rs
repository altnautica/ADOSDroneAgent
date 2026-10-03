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

    /// `systemctl mask <unit>` (idempotent).
    async fn mask(&self, unit: &str) {
        let _ = run(&["mask", unit], PROBE_TIMEOUT).await;
    }

    /// `systemctl unmask <unit>` (idempotent).
    async fn unmask(&self, unit: &str) {
        let _ = run(&["unmask", unit], PROBE_TIMEOUT).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_reported_state_is_a_verdict() {
        assert_eq!(is_active_verdict("active\n"), Some(true));
        assert_eq!(is_active_verdict("inactive\n"), Some(false));
        assert_eq!(is_active_verdict("failed"), Some(false));
        assert_eq!(is_active_verdict("activating"), Some(false));
        assert_eq!(is_active_verdict(""), None);
        assert_eq!(is_active_verdict("  \n"), None);
    }

    /// `systemctl show` for a unit still holding its runtime record. The clean
    /// shape is a run that exited 0 while something referenced the unit.
    #[test]
    fn a_loaded_runtime_record_is_judged_by_its_exit_status() {
        let show = |result: &str, code: i32, status: i32| {
            format!(
                "LoadState=loaded\nInvocationID=05b17701423747b2b5f9080bdd332062\n\
                 Restart=always\nRestartPreventExitStatus=0\nResult={result}\n\
                 ExecMainCode={code}\nExecMainStatus={status}\n"
            )
        };
        assert_eq!(
            shown_end(&show("success", 1, 0)),
            Some(ShownEnd::Live(true))
        );
        assert_eq!(
            shown_end(&show("exit-code", 1, 3)),
            Some(ShownEnd::Live(false))
        );
        // SIGKILL: CLD_KILLED (2), status 9, waiting out RestartSec=.
        assert_eq!(
            shown_end(&show("signal", 2, 9)),
            Some(ShownEnd::Live(false))
        );
        // A stray SIGTERM is a success result, but not an exit: inside the
        // RestartSec= window the record still shows CLD_KILLED/15.
        assert_eq!(
            shown_end(&show("success", 2, 15)),
            Some(ShownEnd::Live(false))
        );
    }

    /// After a clean exit of an unreferenced unit systemd collects it, and
    /// `show` reloads the unit file and reports never-run defaults. Those
    /// defaults must not be read as a crash (`ExecMainCode=0`).
    #[test]
    fn a_collected_unit_defers_to_the_journal_only_under_an_exact_policy() {
        let collected = "LoadState=loaded\nInvocationID=\nRestart=always\n\
                         RestartPreventExitStatus=0\nResult=success\n\
                         ExecMainCode=0\nExecMainStatus=0\n";
        assert_eq!(
            shown_end(collected),
            Some(ShownEnd::Collected {
                exit_zero_only: true
            })
        );
        // Restart=on-failure leaves a clean signal as another success end.
        let on_failure = collected.replace("Restart=always", "Restart=on-failure");
        assert_eq!(
            shown_end(&on_failure),
            Some(ShownEnd::Collected {
                exit_zero_only: false
            })
        );
        // Preventing a restart on SIGTERM too makes a SIGTERM end collectable.
        let wider = collected.replace(
            "RestartPreventExitStatus=0",
            "RestartPreventExitStatus=0 SIGTERM",
        );
        assert_eq!(
            shown_end(&wider),
            Some(ShownEnd::Collected {
                exit_zero_only: false
            })
        );
        let not_found = collected.replace("LoadState=loaded", "LoadState=not-found");
        assert_eq!(shown_end(&not_found), None);
        assert_eq!(shown_end("LoadState=loaded\nResult=success\n"), None);
    }

    /// One `journalctl -o json --output-fields=…` line, as PID 1 writes it.
    fn rec(message_id: &str, invocation: &str, job: Option<&str>) -> String {
        let mut r = serde_json::json!({
            "__CURSOR": "s=12a29ddbd993414aa0e9a0fce957fe02;i=2cf;b=3016f124e3774212ad9b6101da1e4af8",
            "__REALTIME_TIMESTAMP": "1791024914306281",
            "__MONOTONIC_TIMESTAMP": "186249870743",
            "_BOOT_ID": "3016f124e3774212ad9b6101da1e4af8",
            "MESSAGE_ID": message_id,
            "INVOCATION_ID": invocation,
        });
        if let Some(job) = job {
            r["JOB_TYPE"] = job.into();
        }
        r.to_string()
    }

    const RUN: &str = "44888f9da2e7454dadceb9a036e27ed7";
    const EARLIER: &str = "05b17701423747b2b5f9080bdd332062";

    fn newest_first(lines: &[String]) -> String {
        lines.join("\n") + "\n"
    }

    #[test]
    fn a_success_end_with_no_stop_job_is_a_clean_exit() {
        let out = newest_first(&[
            rec(catalog::SUCCESS, RUN, None),
            rec(catalog::STARTED, RUN, Some("start")),
            rec(catalog::SUCCESS, EARLIER, None),
            rec(catalog::STARTED, EARLIER, Some("start")),
        ]);
        assert_eq!(journal_end_verdict(&out), Some(true));
    }

    #[test]
    fn a_stop_job_or_an_unclean_end_is_not_a_clean_exit() {
        // `systemctl stop`: the end is a success result, but a job ended it.
        let stopped = newest_first(&[
            rec(catalog::STOPPED, RUN, Some("stop")),
            rec(catalog::SUCCESS, RUN, None),
            rec(catalog::STOPPING, RUN, Some("stop")),
            rec(catalog::STARTED, RUN, Some("start")),
        ]);
        assert_eq!(journal_end_verdict(&stopped), Some(false));
        let exit_3 = newest_first(&[
            rec(catalog::FAILURE_RESULT, RUN, None),
            rec(catalog::PROCESS_EXIT, RUN, None),
            rec(catalog::STARTED, RUN, Some("start")),
        ]);
        assert_eq!(journal_end_verdict(&exit_3), Some(false));
        let restarted = newest_first(&[
            rec(catalog::RESTART_SCHEDULED, RUN, None),
            rec(catalog::SUCCESS, RUN, None),
            rec(catalog::STARTED, RUN, Some("start")),
        ]);
        assert_eq!(journal_end_verdict(&restarted), Some(false));
    }

    #[test]
    fn only_the_newest_run_is_judged() {
        // The newest run has no end on record: an earlier clean exit does not
        // stand in for it.
        let running = newest_first(&[
            rec(catalog::STARTED, RUN, Some("start")),
            rec(catalog::SUCCESS, EARLIER, None),
        ]);
        assert_eq!(journal_end_verdict(&running), None);
        // A start skipped on an unmet condition logs no invocation: no run.
        let skipped = newest_first(&[
            rec(catalog::STARTED, "", Some("start")),
            rec(catalog::SUCCESS, EARLIER, None),
        ]);
        assert_eq!(journal_end_verdict(&skipped), None);
        // An earlier run's crash does not taint a newer clean exit.
        let recovered = newest_first(&[
            rec(catalog::SUCCESS, RUN, None),
            rec(catalog::STARTED, RUN, Some("start")),
            rec(catalog::FAILURE_RESULT, EARLIER, None),
        ]);
        assert_eq!(journal_end_verdict(&recovered), Some(true));
        assert_eq!(journal_end_verdict(""), None);
        let garbled = format!("{{\"MESSAGE_ID\":\n{}", rec(catalog::SUCCESS, RUN, None));
        assert_eq!(journal_end_verdict(&garbled), None);
    }

    #[test]
    fn catalog_units_are_matched_by_their_journal_name() {
        assert_eq!(service_unit_name("ados-swarmbus"), "ados-swarmbus.service");
        assert_eq!(
            service_unit_name("ados-swarmbus.service"),
            "ados-swarmbus.service"
        );
    }
}
