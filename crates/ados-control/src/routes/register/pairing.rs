//! Node pairing and the dashboard-access PIN.

use crate::routes::table::Routes;
use crate::routes::{dashboard_pin, pairing};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // The node-identity probe and the local pairing handshake. info, code and
    // claim are public (the auth-exempt set); unpair requires the key.
    r.get("/api/pairing/info", pairing::get_pairing_info)
        .get("/api/pairing/code", pairing::get_pairing_code)
        .post("/api/pairing/claim", pairing::claim_pairing)
        .post("/api/pairing/unpair", pairing::unpair)
        // External-code accept: registers against a Mission Control code at the
        // pairing backend, then claims locally. Requires the key when paired.
        .post("/api/pairing/accept", pairing::accept_pairing_code)
        // Dashboard-access PIN: the off-box browser gate. status, verify and
        // set are public at the edge (set authorizes in the handler); clear
        // stays behind the normal gate. A correct PIN mints a session the front
        // accepts as an alternative data-plane credential to X-ADOS-Key.
        .get("/api/dashboard/pin/status", dashboard_pin::get_pin_status)
        .post("/api/dashboard/pin/verify", dashboard_pin::verify_pin)
        .post("/api/dashboard/pin/set", dashboard_pin::set_pin)
        .post("/api/dashboard/pin/clear", dashboard_pin::clear_pin)
}
