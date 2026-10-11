//! Ground-station mesh roles: which units each role owns, the boot apply, and
//! the operator role transition.
//!
//! The role unit sets come from the unit table's `roles` column
//! (`ados_protocol::units`), the same column the service registry's role gates
//! are built from. The boot apply and the transition here read them, the
//! hardware pass reads them, and the REST role route reports them.
//!
//! The transition executes in this process. Each role's units are the processes
//! a transition stops, so code running inside one of them (the data plane's
//! command socket used to host it) could be killed by its own first step and
//! leave the node with masks half applied and no plane running. The supervisor
//! is never a role unit. A request arrives on the control socket, is handed to
//! the supervisor loop, and runs to completion there whatever happens to the
//! requester.

use std::path::Path;

use ados_protocol::units::{role_unit_count, role_unit_names};
use serde_json::{json, Value};

use crate::config::VALID_ROLES;
use crate::lifecycle::Supervisor;
use crate::process_manager::ProcessManager;

static DIRECT_UNITS: [&str; role_unit_count(Some("direct"))] = role_unit_names(Some("direct"));
static RELAY_UNITS: [&str; role_unit_count(Some("relay"))] = role_unit_names(Some("relay"));
static RECEIVER_UNITS: [&str; role_unit_count(Some("receiver"))] =
    role_unit_names(Some("receiver"));

/// The units `role` owns, in start order (the unit table's `roles` column, in
/// table order). The direct role owns the single-node receive plane; relay and
/// receiver bring up batman before the WFB plane that binds to its interface.
/// Any other value reads as direct, like the sentinel.
pub fn role_units(role: &str) -> &'static [&'static str] {
    match role {
        "relay" => &RELAY_UNITS,
        "receiver" => &RECEIVER_UNITS,
        _ => &DIRECT_UNITS,
    }
}

/// The WFB plane `role` owns: the last unit of its set, the one whose receive
/// chain reads the radio key.
pub fn role_plane(role: &str) -> &'static str {
    let units = role_units(role);
    units[units.len() - 1]
}

/// Every role-owned unit: the union of the role unit sets.
pub const ALL_ROLE_UNITS: [&str; role_unit_count(None)] = role_unit_names(None);

/// The registry row name of a role unit (the unit name without `.service`).
pub fn service_name(unit: &str) -> &str {
    unit.trim_end_matches(".service")
}

/// Per-role state files under the run dir, cleared on a transition so a node
/// that left a role never serves that role's last snapshot as current.
const ROLE_STATE_FILES: [&str; 3] = ["mesh-state.json", "wfb-relay.json", "wfb-receiver.json"];

/// The mesh-event kind a transition publishes.
const KIND_ROLE_CHANGED: &str = "role_changed";

/// Atomically write the role sentinel (0o644).
fn write_role_file(path: &Path, role: &str) -> std::io::Result<()> {
    ados_protocol::sidecar::write_atomic(path, format!("{role}\n").as_bytes(), 0o644)
}

/// Apply `role` at supervisor boot: write the sentinel, stop every role unit
/// the role does not own, mask every role unit and unmask the role's own.
/// Starting the role's units is left to the hardware pass. Best-effort: a
/// sentinel write error is logged and the rest still runs.
///
/// The stop matters on a boot whose configured role differs from the sentinel
/// the last session left: a unit pulled up with the supervisor could have read
/// the old sentinel and be driving the adapter for a role this node no longer
/// runs.
pub async fn apply_role_on_boot(pm: &dyn ProcessManager, role: &str, role_path: &Path) {
    let role = if VALID_ROLES.contains(&role) {
        role
    } else {
        "direct"
    };

    if let Err(e) = write_role_file(role_path, role) {
        tracing::error!(error = %e, "role sentinel write failed");
    }

    let owned = role_units(role);
    for unit in ALL_ROLE_UNITS {
        if !owned.contains(&unit) {
            pm.stop(unit).await;
        }
    }
    for unit in ALL_ROLE_UNITS {
        pm.mask(unit).await;
    }
    for unit in owned {
        pm.unmask(unit).await;
    }
    tracing::info!(role, "ground-station role applied at boot");
}

/// A completed transition, in the shape the REST role route returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleResult {
    pub role: String,
    pub previous: String,
    pub units_started: Vec<String>,
    pub units_stopped: Vec<String>,
    pub ts_ms: i64,
    pub noop: bool,
}

/// Why a transition was refused. Each is refused before any side effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleError {
    /// The target is not one of [`VALID_ROLES`].
    InvalidRole(String),
    /// This node is not a ground station.
    NotGroundStation,
    /// A bind handshake owns the radio adapter; a role's receive plane would
    /// fight it.
    BindInProgress,
}

/// A role change the control socket hands to the supervisor loop. `reply`
/// carries the control-socket reply object back to the connection.
pub struct RoleRequest {
    pub target: String,
    pub reason: String,
    pub reply: tokio::sync::oneshot::Sender<Value>,
}

/// The control-socket reply for a transition outcome: the result fields with
/// `ok:true`, or `ok:false` with the refusal code.
pub fn role_reply(outcome: Result<RoleResult, RoleError>) -> Value {
    match outcome {
        Ok(r) => json!({
            "ok": true,
            "role": r.role,
            "previous": r.previous,
            "units_started": r.units_started,
            "units_stopped": r.units_stopped,
            "ts_ms": r.ts_ms,
            "noop": r.noop,
        }),
        Err(RoleError::InvalidRole(bad)) => json!({
            "ok": false,
            "error": "E_INVALID_ROLE",
            "message": format!("role must be one of {VALID_ROLES:?}, got {bad:?}"),
        }),
        Err(RoleError::NotGroundStation) => json!({"ok": false, "error": "E_PROFILE_MISMATCH"}),
        Err(RoleError::BindInProgress) => json!({"ok": false, "error": "E_BIND_IN_PROGRESS"}),
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Supervisor {
    /// Move the ground station to `target`: stop the current role's units
    /// (plane before batman), clear the per-role state files, mask every role
    /// unit and unmask the target's, flip the sentinel, start the target's
    /// units (batman before the plane), then publish `role_changed`.
    ///
    /// Stops and starts go through the service table, so the monitor sees a
    /// stopped unit as stopped rather than dead and supervises the new role's
    /// units from the moment they start. A unit that fails to stop or start is
    /// logged and the transition proceeds; `units_started` lists only the units
    /// that reached `active`.
    pub async fn apply_role(
        &mut self,
        target: &str,
        reason: &str,
    ) -> Result<RoleResult, RoleError> {
        if !VALID_ROLES.contains(&target) {
            return Err(RoleError::InvalidRole(target.to_string()));
        }
        if !self.config().is_ground_station() {
            return Err(RoleError::NotGroundStation);
        }
        if self.bind_session_active() {
            return Err(RoleError::BindInProgress);
        }

        let current = self.config().live_role();
        let ts_ms = now_ms();
        if current == target {
            tracing::info!(role = target, "role_apply_noop");
            return Ok(RoleResult {
                role: target.to_string(),
                previous: current,
                units_started: Vec::new(),
                units_stopped: Vec::new(),
                ts_ms,
                noop: true,
            });
        }
        tracing::info!(previous = %current, target, reason, "role_apply_start");

        let role_path = self.config().mesh_role_path.clone();
        let run_dir = self.config().run_dir.clone();
        let progress = self.progress();

        let mut units_stopped = Vec::new();
        for unit in role_units(&current).iter().rev() {
            if !self.stop_service(service_name(unit)).await {
                tracing::warn!(unit, "role unit stop failed");
            }
            units_stopped.push((*unit).to_string());
            progress.mark();
        }

        for name in ROLE_STATE_FILES {
            let p = run_dir.join(name);
            if p.is_file() {
                if let Err(e) = std::fs::remove_file(&p) {
                    tracing::debug!(path = %p.display(), error = %e, "role state clear failed");
                }
            }
        }

        let pm = self.process_manager();
        for unit in ALL_ROLE_UNITS {
            pm.mask(unit).await;
        }
        for unit in role_units(target) {
            pm.unmask(unit).await;
        }
        progress.mark();

        // The sentinel flips before any start: each role unit's start condition
        // and the monitor's role gate both read it.
        if let Err(e) = write_role_file(&role_path, target) {
            tracing::error!(error = %e, "role sentinel write failed");
        }

        let mut units_started = Vec::new();
        for unit in role_units(target) {
            if self.start_service(service_name(unit)).await {
                units_started.push((*unit).to_string());
            }
            progress.mark();
        }

        crate::mesh_journal::append(
            &crate::mesh_journal::journal_path(&run_dir),
            KIND_ROLE_CHANGED,
            json!({
                "previous": current,
                "role": target,
                "reason": reason,
                "units_started": units_started,
                "units_stopped": units_stopped,
            }),
            ts_ms,
        );
        tracing::info!(
            previous = %current,
            target,
            ?units_started,
            ?units_stopped,
            "role_apply_done"
        );

        Ok(RoleResult {
            role: target.to_string(),
            previous: current,
            units_started,
            units_stopped,
            ts_ms,
            noop: false,
        })
    }
}

#[cfg(test)]
mod tests;
