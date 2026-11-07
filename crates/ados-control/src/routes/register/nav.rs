//! The navigation surface: the nav status read, the live occupancy map, the
//! mission mirror, and the nav stream.

use crate::routes::nav;
use crate::routes::table::Routes;
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.get("/api/v1/nav/status", nav::get_nav_status)
        .get("/api/v1/map/live", nav::get_map_live)
        .get("/api/v1/mission", nav::get_mission)
        .ws("/api/v1/nav/stream", nav::ws_nav_stream)
}
