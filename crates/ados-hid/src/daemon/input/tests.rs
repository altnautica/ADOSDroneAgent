use super::primary_disconnect_request;
use serde_json::json;

#[test]
fn primary_removal_carries_the_holder() {
    let state = json!({"primary_gamepad_id": "evdev-0", "claimed_by": "hdmi-kiosk"});
    assert_eq!(
        primary_disconnect_request(&state, "evdev-0"),
        Some(json!({"op": "disconnect", "client_id": "hdmi-kiosk"}))
    );
}

#[test]
fn unbound_or_unclaimed_primary_sends_no_disconnect() {
    // The claim moved to an operator: the arbiter unbound the gamepad.
    let moved = json!({"primary_gamepad_id": null, "claimed_by": "op-b"});
    assert_eq!(primary_disconnect_request(&moved, "evdev-0"), None);
    let other = json!({"primary_gamepad_id": "evdev-1", "claimed_by": "hdmi-kiosk"});
    assert_eq!(primary_disconnect_request(&other, "evdev-0"), None);
    let free = json!({"primary_gamepad_id": "evdev-0", "claimed_by": null});
    assert_eq!(primary_disconnect_request(&free, "evdev-0"), None);
}
