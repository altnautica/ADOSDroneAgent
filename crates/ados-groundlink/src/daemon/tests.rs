use super::direct::{aux_slots_to_bind, ifaces_to_restore};
use super::*;

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn ground_video_flag_selects_media_server() {
    assert!(wants_ground_video(&args(&["--ground-video"])));
    assert!(!wants_ground_video(&args(&["--role", "direct"])));
}

#[test]
fn a_slot_registered_after_start_still_gets_a_consumer() {
    use std::collections::BTreeSet;

    // The service started with one drone paired.
    let bound: BTreeSet<u8> = [1u8].into_iter().collect();

    // A second drone pairs. Its slot is issued and the receive plane spawns
    // a wfb_rx for it, but the pair route does not restart this unit when
    // the fleet key is unchanged, so nothing else would notice. Before the
    // reconcile, slot 2 decoded onto a port with no reader.
    assert_eq!(
        aux_slots_to_bind(&[1, 2], &bound),
        vec![2],
        "a slot issued after start must be picked up"
    );

    // A whole fleet joining at once is bound in one pass.
    assert_eq!(aux_slots_to_bind(&[1, 2, 3, 4], &bound), vec![2, 3, 4]);

    // Steady state does nothing: no rebinding of a live port.
    let all: BTreeSet<u8> = [1u8, 2, 3, 4].into_iter().collect();
    assert!(aux_slots_to_bind(&[1, 2, 3, 4], &all).is_empty());

    // A released slot is NOT torn down, and does not come back as work.
    assert!(
        aux_slots_to_bind(&[1], &all).is_empty(),
        "a deregistered slot must not be rebound or reaped here"
    );
}

#[test]
fn the_role_comes_from_the_unit_argument() {
    assert_eq!(
        resolve_role_from(&args(&["--role", "relay"])),
        Ok(Role::Relay)
    );
    assert_eq!(
        resolve_role_from(&args(&["--role=receiver"])),
        Ok(Role::Receiver)
    );
    assert_eq!(
        resolve_role_from(&args(&["--role", "direct"])),
        Ok(Role::Direct)
    );
}

#[test]
fn a_missing_or_unknown_role_is_an_error_not_a_guess() {
    assert!(resolve_role_from(&[]).is_err());
    assert!(resolve_role_from(&args(&["--role", "bogus"])).is_err());
    assert!(resolve_role_from(&args(&["--role"])).is_err());
}

#[test]
fn a_role_unit_runs_only_under_its_own_role() {
    // After a reboot into relay the direct receive unit must stand down, or
    // it drives the adapter beside the relay plane.
    assert!(!sentinel_admits(Role::Direct, "relay"));
    assert!(!sentinel_admits(Role::Direct, "receiver"));
    assert!(sentinel_admits(Role::Direct, "direct"));
    assert!(sentinel_admits(Role::Relay, "relay"));
    assert!(!sentinel_admits(Role::Relay, "direct"));
    assert!(sentinel_admits(Role::Receiver, "receiver"));
    assert!(!sentinel_admits(Role::Receiver, "relay"));
}

#[tokio::test]
async fn the_group_cell_holds_every_iface_after_capture() {
    // Mirror what the receive loop does after it resolves the adapters: write
    // every WFB iface into the shared cell. The shutdown side reads them back
    // via the same helper it uses to decide what to restore.
    let cell: wfb_rx::RadioGroup = Arc::new(Mutex::new(Vec::new()));
    // Nothing resolved yet → nothing to restore.
    assert!(ifaces_to_restore(&cell).await.is_empty());

    // Receive-loop capture point.
    *cell.lock().await = vec!["wlan1".to_string(), "wlan2".to_string()];

    // Shutdown side reads the captured ifaces and would restore exactly them.
    assert_eq!(ifaces_to_restore(&cell).await, vec!["wlan1", "wlan2"]);
}
