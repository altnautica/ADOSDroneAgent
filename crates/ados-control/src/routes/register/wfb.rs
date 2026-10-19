//! The node's own WFB radio link: status, history, pairing and the writes.

use crate::routes::table::Routes;
use crate::routes::{radio_adapters, wfb, wfb_local_bind, wfb_pair_write, wfb_write};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // Link status, link-quality history, pair state and failover state.
    r.get("/api/wfb", wfb::get_wfb_status)
        .get("/api/wfb/history", wfb::get_wfb_history)
        .get("/api/wfb/pair", wfb::get_wfb_pair_status)
        .get("/api/wfb/pair/failover-status", wfb::get_failover_status)
        // The operator local-radio bind (through the supervisor socket) and the
        // unpair.
        .get("/api/wfb/pair/local-bind", wfb_local_bind::get_local_bind)
        .post("/api/wfb/pair/local-bind", wfb_local_bind::post_local_bind)
        .post("/api/wfb/pair/unpair", wfb_local_bind::post_unpair)
        // The coordinated channel hop and the runtime TX power (set + persist).
        .post("/api/wfb/channel", wfb_write::set_wfb_channel)
        .put("/api/wfb/tx-power", wfb_write::set_wfb_tx_power)
        // The auto-pair arm flag (a surgical config merge; a re-arm on a paired
        // rig is refused without a persist).
        .put("/api/wfb/pair/auto-pair", wfb_pair_write::put_auto_pair)
        // Every WFB adapter with its role, per-adapter receive counters and
        // the receive-diversity block.
        .get("/api/v1/radio/adapters", radio_adapters::get_radio_adapters)
}
