//! The PIC arbiter, gamepad and Bluetooth writes. They reach the ados-pic and
//! ados-input daemons over sockets that exist only when hid-rust is enabled, so
//! they are registered (and native) only then.

use crate::routes::table::Routes;
use crate::routes::{gs_bluetooth, gs_gamepad_write, gs_pic};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.post("/api/v1/ground-station/pic/claim", gs_pic::post_pic_claim)
        .post(
            "/api/v1/ground-station/pic/release",
            gs_pic::post_pic_release,
        )
        .post(
            "/api/v1/ground-station/pic/confirm-token",
            gs_pic::post_pic_confirm_token,
        )
        .post(
            "/api/v1/ground-station/pic/heartbeat",
            gs_pic::post_pic_heartbeat,
        )
        .put(
            "/api/v1/ground-station/gamepads/primary",
            gs_gamepad_write::put_gamepad_primary,
        )
        .post(
            "/api/v1/ground-station/bluetooth/scan",
            gs_bluetooth::post_bluetooth_scan,
        )
        .post(
            "/api/v1/ground-station/bluetooth/pair",
            gs_bluetooth::post_bluetooth_pair,
        )
        .delete(
            "/api/v1/ground-station/bluetooth/{mac}",
            gs_bluetooth::delete_bluetooth,
        )
}
