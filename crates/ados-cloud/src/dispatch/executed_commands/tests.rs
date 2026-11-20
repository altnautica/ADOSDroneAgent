use super::*;
use serde_json::json;

fn record_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cloud-executed-commands.json");
    (dir, path)
}

/// An executed command id is answered from the stored ack on redelivery,
/// including after a restart reloads the record from disk.
#[test]
fn a_repeat_replays_the_stored_ack_across_a_restart() {
    let (_dir, path) = record_path();
    let row = json!({"_id": "cmd-1", "command": "send_command", "attempts": 1});
    let mut record = ExecutedCommands::load(&path);
    assert_eq!(record.precheck(&row), Precheck::Execute);
    let ack = json!({"commandId": "cmd-1", "status": "completed"});
    record.record("cmd-1", ack.clone()).unwrap();
    assert_eq!(record.precheck(&row), Precheck::Replay(ack.clone()));

    let reloaded = ExecutedCommands::load(&path);
    let redelivered = json!({"_id": "cmd-1", "command": "send_command", "attempts": 2});
    assert_eq!(reloaded.precheck(&redelivered), Precheck::Replay(ack));
}

/// A redelivered flight command the node never recorded is refused; other
/// commands and first deliveries execute.
#[test]
fn an_unrecorded_redelivered_flight_command_is_refused() {
    let (_dir, path) = record_path();
    let record = ExecutedCommands::load(&path);
    let redelivered = json!({"_id": "cmd-2", "command": "send_command", "attempts": 2});
    assert!(matches!(
        record.precheck(&redelivered),
        Precheck::Refuse(r) if r.status == super::super::CommandStatus::Failed
    ));
    let first = json!({"_id": "cmd-2", "command": "send_command", "attempts": 1});
    assert_eq!(record.precheck(&first), Precheck::Execute);
    let read = json!({"_id": "cmd-3", "command": "get_services", "attempts": 3});
    assert_eq!(record.precheck(&read), Precheck::Execute);
}

/// The record keeps only the newest ids.
#[test]
fn the_record_evicts_the_oldest_past_the_cap() {
    let (_dir, path) = record_path();
    let mut record = ExecutedCommands::load(&path);
    for i in 0..=EXECUTED_COMMANDS_MAX {
        record.record(&format!("c{i}"), json!({"n": i})).unwrap();
    }
    let reloaded = ExecutedCommands::load(&path);
    assert!(reloaded.stored_ack("c0").is_none(), "oldest evicted");
    assert!(reloaded.stored_ack("c1").is_some());
    assert!(reloaded
        .stored_ack(&format!("c{EXECUTED_COMMANDS_MAX}"))
        .is_some());
}

#[test]
fn a_corrupt_record_loads_empty() {
    let (_dir, path) = record_path();
    std::fs::write(&path, b"{not json").unwrap();
    let record = ExecutedCommands::load(&path);
    assert!(record.stored_ack("anything").is_none());
}

/// A command a restart cut short is answered failed and never run again.
#[test]
fn a_command_cut_short_by_a_restart_is_refused_not_run_again() {
    let (_dir, path) = record_path();
    let mut record = ExecutedCommands::load(&path);
    record.begin("cmd-4").unwrap();
    let reloaded = ExecutedCommands::load(&path);
    assert_eq!(reloaded.interrupted(), vec!["cmd-4".to_string()]);
    let redelivered = json!({"_id": "cmd-4", "command": "restart_service", "attempts": 2});
    assert!(matches!(
        reloaded.precheck(&redelivered),
        Precheck::Refuse(r) if r.result["message"] == INTERRUPTED
    ));
    assert!(reloaded.undelivered().is_empty(), "no ack yet to send");
}

/// An ack stays in the outbox, across a restart, until it is delivered.
#[test]
fn an_ack_stays_undelivered_until_the_queue_has_it() {
    let (_dir, path) = record_path();
    let mut record = ExecutedCommands::load(&path);
    record.begin("cmd-5").unwrap();
    let ack = json!({"commandId": "cmd-5", "status": "completed"});
    record.record("cmd-5", ack.clone()).unwrap();
    assert!(record.interrupted().is_empty());
    let mut reloaded = ExecutedCommands::load(&path);
    assert_eq!(reloaded.undelivered(), vec![ack]);
    reloaded.delivered("cmd-5").unwrap();
    assert!(ExecutedCommands::load(&path).undelivered().is_empty());
}

/// A record written before the outbox holds only acks already sent.
#[test]
fn a_record_from_before_the_outbox_has_nothing_to_resend() {
    let (_dir, path) = record_path();
    std::fs::write(&path, br#"[{"id":"cmd-6","ack":{"commandId":"cmd-6"}}]"#).unwrap();
    let record = ExecutedCommands::load(&path);
    assert!(record.undelivered().is_empty());
    assert!(record.interrupted().is_empty());
    assert!(record.stored_ack("cmd-6").is_some());
}
