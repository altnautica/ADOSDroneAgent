//! MCP-token management for the AI-control surface.

use crate::routes::mcp;
use crate::routes::table::Routes;
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // status is a read; mint issues a credential (authorized in the handler to
    // on-box or a valid key); revoke needs the admin scope class. The edge
    // honours a minted token only when `mcp.token_accept_enabled` is set.
    r.get("/api/mcp/status", mcp::get_mcp_status)
        .post("/api/mcp/tokens", mcp::mint_mcp_token)
        .post("/api/mcp/revoke", mcp::revoke_mcp_token)
}
