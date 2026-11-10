//! The command-authority stream, the operator's gimbal rate pad and the
//! vehicle's MAVLink camera (photo and zoom).

use crate::routes::table::Routes;
use crate::routes::{camera_mavlink, gimbal_rate, pilot};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.ws("/api/v1/pilot/stream", pilot::ws_pilot_stream)
        .get("/api/v1/gimbal/rate", gimbal_rate::get_gimbal_rate)
        .post("/api/v1/gimbal/rate", gimbal_rate::post_gimbal_rate)
        .get("/api/v1/camera/mavlink", camera_mavlink::list_cameras)
        .post(
            "/api/v1/camera/mavlink/capture",
            camera_mavlink::post_capture,
        )
        .post("/api/v1/camera/mavlink/zoom", camera_mavlink::post_zoom)
}
