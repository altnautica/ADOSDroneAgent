//! Unit tests for the systemd backend's pure parsers.

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
    // A nonzero status beside 0 (ados-video's terminal 78) ends `failed`,
    // which systemd never collects, so exit 0 is still the only collected end.
    let with_status = collected.replace(
        "RestartPreventExitStatus=0",
        "RestartPreventExitStatus=0 78",
    );
    assert_eq!(
        shown_end(&with_status),
        Some(ShownEnd::Collected {
            exit_zero_only: true
        })
    );
    let without_zero =
        collected.replace("RestartPreventExitStatus=0", "RestartPreventExitStatus=78");
    assert_eq!(
        shown_end(&without_zero),
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

#[test]
fn an_unmet_condition_on_a_stopped_unit_is_a_skipped_start() {
    assert_eq!(
        condition_verdict("ConditionResult=no\nActiveState=inactive\n"),
        Some(false)
    );
    assert_eq!(
        condition_verdict("ConditionResult=yes\nActiveState=inactive\n"),
        Some(true)
    );
    // A running unit held its conditions whatever the record says.
    assert_eq!(
        condition_verdict("ConditionResult=no\nActiveState=active\n"),
        Some(true)
    );
    // Missing properties are no verdict.
    assert_eq!(condition_verdict("ActiveState=inactive\n"), None);
    assert_eq!(condition_verdict(""), None);
}
