//! Route registration, one file per domain. Each `register` adds its routes to
//! the shared [`Routes`] builder, which records every one in the native table.

mod config;
mod ground_station;
mod hid;
mod logs;
mod lora;
mod mcp;
mod network;
mod pairing;
mod plugins;
mod system;
mod video;
mod wfb;

use super::table::Routes;
use crate::state::AppState;

/// Every native route. The HID writes reach daemons whose sockets exist only
/// when hid-rust is enabled, so they are registered only then.
pub(super) fn register_all(r: Routes<AppState>, hid_native: bool) -> Routes<AppState> {
    let r = system::register(r);
    let r = config::register(r);
    let r = pairing::register(r);
    let r = plugins::register(r);
    let r = video::register(r);
    let r = wfb::register(r);
    let r = ground_station::register(r);
    let r = network::register(r);
    let r = mcp::register(r);
    let r = logs::register(r);
    let r = lora::register(r);
    if hid_native {
        hid::register(r)
    } else {
        r
    }
}
