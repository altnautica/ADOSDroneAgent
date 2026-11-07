//! The command-authority stream.

use crate::routes::pilot;
use crate::routes::table::Routes;
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    r.ws("/api/v1/pilot/stream", pilot::ws_pilot_stream)
}
