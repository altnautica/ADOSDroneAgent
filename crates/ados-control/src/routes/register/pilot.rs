//! The command-authority stream and the operator's gimbal rate pad.

use crate::routes::table::Routes;
use crate::routes::{gimbal_rate, pilot};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.ws("/api/v1/pilot/stream", pilot::ws_pilot_stream)
        .get("/api/v1/gimbal/rate", gimbal_rate::get_gimbal_rate)
        .post("/api/v1/gimbal/rate", gimbal_rate::post_gimbal_rate)
}
