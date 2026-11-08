//! Aircraft setup: the flight-controller setup and tuning wizards.

use crate::routes::fc_setup;
use crate::routes::table::Routes;
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.get("/api/v1/fc-setup", fc_setup::get_fc_setup)
        .post("/api/v1/fc-setup/revert", fc_setup::post_revert)
        .post("/api/v1/fc-setup/{wizard}/start", fc_setup::post_start)
        .post("/api/v1/fc-setup/{wizard}/abort", fc_setup::post_abort)
        .post(
            "/api/v1/fc-setup/{wizard}/steps/{step}/run",
            fc_setup::post_run_step,
        )
        .post(
            "/api/v1/fc-setup/{wizard}/steps/{step}/actions/{action}",
            fc_setup::post_action,
        )
        .ws("/api/v1/fc-setup/stream", fc_setup::ws_fc_setup_stream)
}
