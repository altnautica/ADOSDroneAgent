use super::*;
use serde_json::json;

#[test]
fn each_event_is_one_enveloped_line_and_appends_never_truncate() {
    let dir = tempfile::tempdir().unwrap();
    let p = journal_path(&dir.path().join("nested"));
    append(&p, "relay_connected", json!({"relay_mac": "aa:bb"}), 100);
    append(&p, "role_changed", json!({"role": "relay"}), 200);

    let text = std::fs::read_to_string(&p).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0],
        json!({"bus": "mesh", "kind": "relay_connected", "timestamp_ms": 100,
               "payload": {"relay_mac": "aa:bb"}})
    );
    assert_eq!(lines[1]["kind"], "role_changed");
    assert_eq!(lines[1]["timestamp_ms"], 200);
}
