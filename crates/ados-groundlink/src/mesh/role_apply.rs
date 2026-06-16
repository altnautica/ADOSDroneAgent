//! Mesh role transition: the active apply path (stop/start + mask/unmask).
//!
//! The sibling [`super::manager::get_current_role`] only reads the on-disk role
//! sentinel; this module owns the full transition the operator drives. It ports
//! `role_manager.apply_role`:
//!
//! - stop the previous role's units (reverse dependency order so the wfb side
//!   quiesces before batman tears down),
//! - clear the stale runtime mesh-state snapshots so a freshly-`direct` node
//!   cannot serve old relay/receiver data,
//! - mask every mesh unit, then unmask the ones the target role owns,
//! - flip the on-disk sentinel BEFORE starting the new units so their
//!   `ConditionPathExists` gates pass,
//! - start the new role's units (dependency order: batman first, then wfb),
//! - publish a `role_changed` event onto the cross-process mesh-event journal so
//!   the GCS Hardware tab + OLED + logs all see the transition.
//!
//! Systemctl actions never fail the transition: a wedged unit is logged and the
//! apply proceeds (matching the Python "never raises on systemctl failures"
//! contract), so a partial transition still completes on a node where one unit is
//! temporarily stuck.

use std::path::PathBuf;

use ados_supervisor::systemctl;

use crate::paths::{MESH_ROLE_PATH, MESH_STATE_JSON, WFB_RECEIVER_JSON, WFB_RELAY_JSON};

/// The valid mesh roles. Mirrors `role_manager.VALID_ROLES`.
pub const VALID_ROLES: [&str; 3] = ["direct", "relay", "receiver"];

/// The systemd units each role owns, in start order. Mirrors
/// `role_manager._ROLE_UNITS`: `direct` owns none; `relay`/`receiver` both bring
/// up `ados-batman` before their wfb unit (the wfb side binds to the batman-adv
/// interface). Start order is load-bearing for the `units_started` response.
pub fn role_units(role: &str) -> &'static [&'static str] {
    match role {
        "relay" => &["ados-batman.service", "ados-wfb-relay.service"],
        "receiver" => &["ados-batman.service", "ados-wfb-receiver.service"],
        _ => &[],
    }
}

/// The full mesh-unit set, in the `role_manager._ALL_MESH_UNITS` order.
pub const ALL_MESH_UNITS: [&str; 3] = [
    "ados-batman.service",
    "ados-wfb-relay.service",
    "ados-wfb-receiver.service",
];

/// The runtime mesh-state snapshots cleared on a transition so a stale snapshot
/// can never mislead a reader on the next start. Mirrors
/// `role_manager._MESH_STATE_FILES`.
fn mesh_state_files() -> [PathBuf; 3] {
    [
        PathBuf::from(MESH_STATE_JSON),
        PathBuf::from(WFB_RELAY_JSON),
        PathBuf::from(WFB_RECEIVER_JSON),
    ]
}

/// The role sentinel path. Honours `ADOS_MESH_ROLE` for tests so a transition
/// can be exercised without touching `/etc/ados/mesh/role` on the host.
fn role_file() -> PathBuf {
    std::env::var("ADOS_MESH_ROLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(MESH_ROLE_PATH))
}

/// Read the on-disk role sentinel, defaulting to `direct` when the file is
/// missing, unreadable, or carries an unknown value. Mirrors
/// `role_manager.get_current_role` (which the sibling reader also implements; a
/// local copy keeps this module self-contained against its own sentinel seam).
pub fn current_role() -> String {
    if let Ok(text) = std::fs::read_to_string(role_file()) {
        let value = text.trim();
        if VALID_ROLES.contains(&value) {
            return value.to_string();
        }
    }
    "direct".to_string()
}

/// Atomically write the role sentinel (0o644, owner-writable). Mirrors
/// `role_manager._write_role_file`: write a `.tmp` sibling, chmod 0644, rename.
fn write_role_file(role: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = role_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&tmp)?;
        f.write_all(format!("{role}\n").as_bytes())?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&tmp, &path)
}

/// Wall-clock unix milliseconds (the transition timestamp basis), matching the
/// Python `int(time.time() * 1000)`.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The result of a role transition, the body the REST role route returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleResult {
    pub role: String,
    pub previous: String,
    pub units_started: Vec<String>,
    pub units_stopped: Vec<String>,
    pub ts_ms: i64,
    pub noop: bool,
}

/// Apply the target role. Stops old units, clears stale state, masks the rest,
/// flips the sentinel, starts the new units, publishes the `role_changed` event.
///
/// Returns the transition metadata. An unknown target role is the only error
/// (`Err(target)`), mirroring the Python `ValueError`; systemctl failures never
/// fail the transition.
pub async fn apply_role(target: &str, reason: &str) -> Result<RoleResult, String> {
    if !VALID_ROLES.contains(&target) {
        return Err(target.to_string());
    }

    let current = current_role();
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

    let mut units_stopped: Vec<String> = Vec::new();
    let mut units_started: Vec<String> = Vec::new();

    // Stop the old role's units in reverse dependency order (wfb before batman).
    for unit in role_units(&current).iter().rev() {
        if !systemctl::stop(unit).await {
            tracing::debug!(unit, "stop_unit_noop");
        }
        units_stopped.push((*unit).to_string());
    }

    // Clear stale runtime mesh snapshots so a freshly-direct node cannot serve
    // old mesh data from a previous relay/receiver session.
    for p in mesh_state_files() {
        if p.is_file() {
            if let Err(e) = std::fs::remove_file(&p) {
                tracing::debug!(path = %p.display(), error = %e, "mesh_state_unlink_failed");
            }
        }
    }

    // Mask every mesh unit, then unmask the ones for the target role. Masking is
    // idempotent.
    for unit in ALL_MESH_UNITS {
        systemctl::mask(unit).await;
    }
    for unit in role_units(target) {
        systemctl::unmask(unit).await;
    }

    // Flip the sentinel BEFORE starting new units so their ConditionPathExists
    // checks pass.
    if let Err(e) = write_role_file(target) {
        tracing::error!(error = %e, "role_file_write_failed");
    }

    // Start new units in dependency order (batman first, then wfb).
    for unit in role_units(target) {
        if systemctl::start(unit).await {
            units_started.push((*unit).to_string());
        }
    }

    // Publish the transition event last so subscribers see a consistent state.
    let payload = serde_json::json!({
        "previous": current,
        "role": target,
        "reason": reason,
        "units_started": units_started,
        "units_stopped": units_stopped,
    });
    crate::mesh_events::emit_role_changed(payload, ts_ms);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `ADOS_MESH_ROLE` is process-global; the env-mutating tests serialize on
    // this lock so the multi-threaded test runner cannot interleave their
    // set/remove of the var.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn role_units_match_the_python_mapping() {
        assert!(role_units("direct").is_empty());
        assert_eq!(
            role_units("relay"),
            &["ados-batman.service", "ados-wfb-relay.service"]
        );
        assert_eq!(
            role_units("receiver"),
            &["ados-batman.service", "ados-wfb-receiver.service"]
        );
        // An unknown role owns no units (matches the dict `.get(role, [])`).
        assert!(role_units("bogus").is_empty());
    }

    #[test]
    fn all_mesh_units_is_the_full_set_in_order() {
        assert_eq!(
            ALL_MESH_UNITS,
            [
                "ados-batman.service",
                "ados-wfb-relay.service",
                "ados-wfb-receiver.service",
            ]
        );
    }

    #[test]
    fn current_role_reads_the_sentinel_and_defaults_direct() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let role_path = dir.path().join("role");
        std::fs::write(&role_path, "relay\n").unwrap();
        // SAFETY: the env lock is held.
        unsafe { std::env::set_var("ADOS_MESH_ROLE", &role_path) };
        assert_eq!(current_role(), "relay");
        // An absent sentinel defaults to direct.
        unsafe { std::env::set_var("ADOS_MESH_ROLE", dir.path().join("absent")) };
        assert_eq!(current_role(), "direct");
        // An unknown value also defaults to direct.
        let bad = dir.path().join("bad");
        std::fs::write(&bad, "bogus\n").unwrap();
        unsafe { std::env::set_var("ADOS_MESH_ROLE", &bad) };
        assert_eq!(current_role(), "direct");
        unsafe { std::env::remove_var("ADOS_MESH_ROLE") };
    }

    #[test]
    fn write_role_file_round_trips_with_0644() {
        use std::os::unix::fs::PermissionsExt;
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let role_path = dir.path().join("mesh/role");
        // SAFETY: the env lock is held.
        unsafe { std::env::set_var("ADOS_MESH_ROLE", &role_path) };
        write_role_file("receiver").unwrap();
        assert_eq!(std::fs::read_to_string(&role_path).unwrap(), "receiver\n");
        let mode = std::fs::metadata(&role_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        unsafe { std::env::remove_var("ADOS_MESH_ROLE") };
    }

    #[tokio::test]
    async fn unknown_target_is_rejected_before_any_side_effect() {
        // An unknown role returns Err(target) and touches nothing — the Python
        // ValueError path. (No env redirect needed: the guard fires first.)
        let err = apply_role("bogus", "test").await;
        assert_eq!(err, Err("bogus".to_string()));
    }

    // The env lock is held across the single `apply_role` await so the
    // `ADOS_MESH_ROLE` override the test set cannot be cleared by a sibling test
    // mid-call. The awaited future never yields to another task that takes this
    // lock (a noop transition does no I/O), so holding it is safe here.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn noop_when_already_in_the_target_role() {
        // current == target → a noop transition with empty unit lists and the
        // noop flag set, never touching systemd.
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let role_path = dir.path().join("role");
        std::fs::write(&role_path, "direct\n").unwrap();
        // SAFETY: the env lock is held.
        unsafe { std::env::set_var("ADOS_MESH_ROLE", &role_path) };
        let res = apply_role("direct", "test").await.unwrap();
        assert!(res.noop);
        assert_eq!(res.role, "direct");
        assert_eq!(res.previous, "direct");
        assert!(res.units_started.is_empty());
        assert!(res.units_stopped.is_empty());
        unsafe { std::env::remove_var("ADOS_MESH_ROLE") };
    }
}
