//! Profile-agnostic network routes: the Wi-Fi client station and MAC pinning.

use crate::routes::table::Routes;
use crate::routes::{mac_adapters, mac_pin, network_client_read, network_write};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // The saved-profile listing (read-only nmcli) and the nearby-network scan.
    r.get(
        "/api/v1/network/client/configured",
        network_client_read::get_client_configured,
    )
    .get(
        "/api/v1/network/client/scan",
        network_client_read::get_client_scan,
    )
    // The station status and writes go through the Wi-Fi command socket on
    // every profile: a ground station's uplink daemon serves it, any other node
    // serves it from this process.
    .get(
        "/api/v1/network/client/status",
        network_client_read::get_client_status,
    )
    .put(
        "/api/v1/network/client/join",
        network_write::put_client_join,
    )
    .delete("/api/v1/network/client", network_write::delete_client)
    .delete(
        "/api/v1/network/client/configured/{name}",
        network_write::delete_client_configured,
    )
    .put(
        "/api/v1/network/client/configured/{name}/autoconnect",
        network_write::put_client_autoconnect,
    )
    // MAC pinning: the per-adapter verdicts, and the pin (POST) and clear
    // (DELETE) that merge the mac_pin config the supervisor reconciler reads.
    .get(
        "/api/v1/network/mac/adapters",
        mac_adapters::get_mac_adapters,
    )
    .post("/api/v1/network/mac/pin", mac_pin::post_mac_pin)
    .delete("/api/v1/network/mac/{iface}", mac_pin::delete_mac_pin)
}
