//! The operator API key: rotate and clear.

use crate::routes::security_api_key;
use crate::routes::table::Routes;
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // Both change who the node trusts: refused in the handler over the radio
    // relay and to a remote caller, and unmapped for MCP tokens.
    r.post(
        "/api/v1/security/api-key/rotate",
        security_api_key::rotate_api_key,
    )
    .delete("/api/v1/security/api-key", security_api_key::clear_api_key)
}
