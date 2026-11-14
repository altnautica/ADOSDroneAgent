//! The sensing surface: the host clock, the rig sensor table and its rig
//! writes, capture storage, and GNSS (fix, RTK base, RTCM injection).

use crate::routes::sensing::{clock, gnss, rig_write, sensors, storage};
use crate::routes::table::Routes;
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.get("/api/v1/clock", clock::get_clock)
        .get("/api/v1/sensors", sensors::get_sensors)
        .put("/api/v1/sensors/rig", rig_write::put_rig)
        .post("/api/v1/sensors/rig/lidar", rig_write::post_rig_lidar)
        .get("/api/v1/storage", storage::get_storage)
        .get("/api/v1/gnss", gnss::get_gnss)
        .put("/api/v1/gnss/base", gnss::put_base)
        .post("/api/v1/gnss/rtcm", gnss::post_rtcm)
}
