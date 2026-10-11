//! The durable log store: the query proxy, recent entries, the live tail and
//! the operator cloud-export trigger.

use crate::routes::table::Routes;
use crate::routes::{logs, logs_write, observability};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // The logging store's query API, forwarded over its local socket.
    r.get(
        "/api/v2/observability/{*upstream_path}",
        observability::observability_proxy,
    )
    // Recent log entries and their live SSE tail.
    .get("/api/logs", logs::get_logs)
    .get("/api/logs/stream", logs::get_logs_stream)
    // Writes a push-request file the cloud service consumes and reports the
    // outcome.
    .post("/api/logs/push", logs_write::push_logs)
}
