use super::*;

#[test]
fn status_serializes_to_wire_strings() {
    assert_eq!(CommandStatus::Completed.as_str(), "completed");
    assert_eq!(CommandStatus::Failed.as_str(), "failed");
    assert_eq!(
        serde_json::to_value(CommandStatus::Completed).unwrap(),
        serde_json::json!("completed")
    );
}

#[test]
fn result_builders_shape_the_ack() {
    let ok = CommandResult::completed("installed");
    assert_eq!(ok.result["success"], true);
    assert_eq!(ok.result["message"], "installed");
    let bad = CommandResult::failed("boom").with_data(serde_json::json!({"code": "x"}));
    assert_eq!(bad.result["success"], false);
    assert_eq!(bad.data.unwrap()["code"], "x");
}
