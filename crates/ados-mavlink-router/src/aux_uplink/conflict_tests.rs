use super::*;
use ados_protocol::mavlink::ardupilotmega::{MavCmd, MavMessage, COMMAND_LONG_DATA};
use ados_protocol::mavlink::{serialize_v2, MavHeader};

fn arm_for(target_system: u8) -> Vec<u8> {
    let msg = MavMessage::COMMAND_LONG(COMMAND_LONG_DATA {
        target_system,
        target_component: 1,
        command: MavCmd::MAV_CMD_COMPONENT_ARM_DISARM,
        confirmation: 0,
        param1: 1.0,
        param2: 0.0,
        param3: 0.0,
        param4: 0.0,
        param5: 0.0,
        param6: 0.0,
        param7: 0.0,
    });
    serialize_v2(
        MavHeader {
            system_id: 255,
            component_id: 190,
            sequence: 0,
        },
        &msg,
    )
    .unwrap()
}

fn wall_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn sidecar(dir: &std::path::Path, written_at: f64, conflicts: &[u8]) -> PathBuf {
    let path = dir.join("relayed-status.json");
    let doc = serde_json::json!({
        "wall_time_unix": written_at,
        "conflicting_system_ids": conflicts,
    });
    std::fs::write(&path, doc.to_string()).unwrap();
    path
}

#[test]
fn a_command_to_a_shared_system_id_is_held_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let path = sidecar(dir.path(), wall_now(), &[1]);
    let blocked = Arc::new(AtomicU64::new(0));
    let mut filter = ConflictFilter::new(path, blocked.clone());

    let mut chunk = arm_for(1);
    let other = arm_for(2);
    chunk.extend_from_slice(&other);
    assert_eq!(filter.admit(chunk), other, "only system 2's frame leaves");
    assert_eq!(blocked.load(Ordering::Relaxed), 1);

    // Broadcast is not an address two aircraft can share.
    let broadcast = arm_for(0);
    assert_eq!(filter.admit(broadcast.clone()), broadcast);
}

#[test]
fn without_a_live_conflict_report_nothing_is_held() {
    let dir = tempfile::tempdir().unwrap();
    let frame = arm_for(1);

    let none = sidecar(dir.path(), wall_now(), &[]);
    let mut filter = ConflictFilter::new(none, Arc::new(AtomicU64::new(0)));
    assert_eq!(filter.admit(frame.clone()), frame);

    // A conflict written by a receive process that has since stopped.
    let stale = sidecar(
        dir.path(),
        wall_now() - CONFLICT_DOC_STALE_AFTER_S - 5.0,
        &[1],
    );
    let mut filter = ConflictFilter::new(stale, Arc::new(AtomicU64::new(0)));
    assert_eq!(filter.admit(frame.clone()), frame);

    let mut filter =
        ConflictFilter::new(dir.path().join("absent.json"), Arc::new(AtomicU64::new(0)));
    assert_eq!(filter.admit(frame.clone()), frame);
}
