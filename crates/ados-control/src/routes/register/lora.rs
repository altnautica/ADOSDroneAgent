//! The LoRa beacon lane: state, the compiled region table, beacons heard and
//! the region / channel / power write.

use crate::routes::lora;
use crate::routes::table::Routes;
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.get("/api/v1/lora", lora::get_lora)
        .put("/api/v1/lora", lora::put_lora)
        .get("/api/v1/lora/regions", lora::get_lora_regions)
        .get("/api/v1/lora/beacons", lora::get_lora_beacons)
}
