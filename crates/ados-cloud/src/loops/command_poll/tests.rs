use super::*;
use crate::dispatch::CommandResult;

#[test]
fn ack_carries_command_id_device_status_result_and_data() {
    let result =
        CommandResult::completed("installed").with_data(serde_json::json!({"pluginId": "p"}));
    let ack = build_ack("cmd-1", "dev1", &result);
    assert_eq!(ack["commandId"], "cmd-1");
    assert_eq!(ack["deviceId"], "dev1");
    assert_eq!(ack["status"], "completed");
    assert_eq!(ack["result"]["success"], true);
    assert_eq!(ack["data"]["pluginId"], "p");
}

#[test]
fn ack_omits_data_when_absent() {
    let result = CommandResult::failed("boom");
    let ack = build_ack("cmd-2", "dev1", &result);
    assert_eq!(ack["status"], "failed");
    assert!(ack.as_object().unwrap().get("data").is_none());
}

#[test]
fn parse_commands_reads_the_array() {
    let body = serde_json::json!({
        "commands": [
            {"_id": "c1", "command": "get_services"},
            {"_id": "c2", "command": "plugin.enable", "args": {"pluginId": "p"}}
        ]
    });
    let cmds = parse_commands(&body);
    assert_eq!(cmds.len(), 2);
    assert_eq!(command_id(&cmds[0]), "c1");
    assert_eq!(command_name(&cmds[1]), "plugin.enable");
}

#[test]
fn parse_commands_empty_when_missing() {
    assert!(parse_commands(&serde_json::json!({})).is_empty());
}
