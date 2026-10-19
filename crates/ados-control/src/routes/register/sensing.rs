//! The sensing surface: the host clock, the rig sensor table and its rig
//! write, capture storage, and GNSS (fix, RTK base, RTCM injection).

use crate::routes::sensing::{clock, gnss, sensors, storage};
use crate::routes::table::Routes;
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.get("/api/v1/clock", clock::get_clock)
        .get("/api/v1/sensors", sensors::get_sensors)
        .put("/api/v1/sensors/rig", sensors::put_rig)
        .get("/api/v1/storage", storage::get_storage)
        .get("/api/v1/gnss", gnss::get_gnss)
        .put("/api/v1/gnss/base", gnss::put_base)
        .post("/api/v1/gnss/rtcm", gnss::post_rtcm)
}
