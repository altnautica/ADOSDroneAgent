//! The agent config surface: its JSON Schema and the effective values.

use crate::routes::table::Routes;
use crate::routes::{config_rw, config_schema};
use crate::state::AppState;

pub(super) fn register(r: Routes<AppState>) -> Routes<AppState> {
    // The build-time-embedded shape of the config surface (types, enums,
    // defaults, secret markers), so a schema-driven settings UI renders without
    // hand-typed forms. Shape only; the values stay on /api/config.
    r.get("/api/config/schema", config_schema::get_config_schema)
        // The effective values (secrets redacted) and the validated write.
        .get("/api/config", config_rw::get_config)
        .put("/api/config", config_rw::put_config)
}
