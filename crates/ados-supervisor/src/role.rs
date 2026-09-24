//! Ground-station mesh roles: which units each role owns, the boot apply, and
//! the operator role transition.
//!
//! This is the one definition of the role unit sets. The boot apply and the
//! transition here read it, the hardware pass reads it, the REST role route
//! reports it, and a test holds the service registry's role gates to it.
//!
//! The transition executes in this process. Each role's units are the processes
//! a transition stops, so code running inside one of them (the data plane's
//! command socket used to host it) could be killed by its own first step and
//! leave the node with masks half applied and no plane running. The supervisor
//! is never a role unit. A request arrives on the control socket, is handed to
//! the supervisor loop, and runs to completion there whatever happens to the
//! requester.

use std::path::Path;

use serde_json::{json, Value};

use crate::config::VALID_ROLES;
use crate::lifecycle::Supervisor;
use crate::process_manager::ProcessManager;

/// The units `role` owns, in start order. The direct role owns the single-node
/// receive plane; relay and receiver bring up batman before the WFB plane that
/// binds to its interface. Any other value reads as direct, like the sentinel.
pub fn role_units(role: &str) -> &'static [&'static str] {
    match role {
        "relay" => &["ados-batman.service", "ados-wfb-relay.service"],
        "receiver" => &["ados-batman.service", "ados-wfb-receiver.service"],
        _ => &["ados-wfb-rx.service"],
    }
}

/// The WFB plane `role` owns: the last unit of its set, the one whose receive
/// chain reads the radio key.
pub fn role_plane(role: &str) -> &'static str {
    let units = role_units(role);
    units[units.len() - 1]
}

/// Every role-owned unit: the union of the role unit sets.
pub const ALL_ROLE_UNITS: [&str; 4] = [
    "ados-wfb-rx.service",
    "ados-batman.service",
    "ados-wfb-relay.service",
    "ados-wfb-receiver.service",
];

/// The registry row name of a role unit (the unit name without `.service`).
pub fn service_name(unit: &str) -> &str {
    unit.trim_end_matches(".service")
}

/// Per-role state files under the run dir, cleared on a transition so a node
/// that left a role never serves that role's last snapshot as current.
const ROLE_STATE_FILES: [&str; 3] = ["mesh-state.json", "wfb-relay.json", "wfb-receiver.json"];

/// The mesh-event kind a transition publishes.
const KIND_ROLE_CHANGED: &str = "role_changed";

/// Atomically write the role sentinel (`temp` + rename, 0o644).
fn write_role_file(path: &Path, role: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("{role}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&tmp, path)
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
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    use parking_lot::Mutex;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::bind::orchestrator::BindOrchestrator;
    use crate::config::AgentConfig;

    /// The WFB planes that can drive the ground station's RTL adapter.
    const WFB_PLANES: [&str; 3] = ["ados-wfb-rx", "ados-wfb-relay", "ados-wfb-receiver"];

    /// A systemd stand-in that tracks which units are active and, like
    /// systemd, kills whatever runs inside a unit when that unit stops: a task
    /// registered as hosted by a unit is aborted by the unit's stop.
    #[derive(Default)]
    struct FakeSystemd {
        active: Mutex<BTreeSet<String>>,
        hosted: Mutex<BTreeMap<String, tokio::task::AbortHandle>>,
    }

    impl FakeSystemd {
        fn with_active(units: &[&str]) -> Self {
            let fake = Self::default();
            for u in units {
                fake.active.lock().insert((*u).to_string());
            }
            fake
        }
        fn host(&self, unit: &str, task: tokio::task::AbortHandle) {
            self.hosted.lock().insert(unit.to_string(), task);
        }
        fn planes(&self) -> Vec<String> {
            let active = self.active.lock();
            WFB_PLANES
                .iter()
                .filter(|p| active.contains(**p))
                .map(|p| p.to_string())
                .collect()
        }
        fn is_up(&self, unit: &str) -> bool {
            self.active.lock().contains(service_name(unit))
        }
    }

    #[async_trait::async_trait]
    impl ProcessManager for FakeSystemd {
        async fn start(&self, unit: &str) -> bool {
            self.active.lock().insert(service_name(unit).to_string());
            true
        }
        async fn stop(&self, unit: &str) -> bool {
            let name = service_name(unit);
            self.active.lock().remove(name);
            if let Some(task) = self.hosted.lock().remove(name) {
                task.abort();
            }
            true
        }
        async fn restart(&self, unit: &str) -> bool {
            self.start(unit).await
        }
        async fn try_restart(&self, _unit: &str) -> bool {
            true
        }
        async fn reset_failed(&self, _unit: &str) {}
        async fn is_active(&self, unit: &str) -> Option<bool> {
            Some(self.is_up(unit))
        }
        async fn mask(&self, _unit: &str) {}
        async fn unmask(&self, _unit: &str) {}
    }

    /// A ground-station supervisor whose sentinel and run dir live in `dir`,
    /// with the sentinel reading `role`.
    fn ground_station(dir: &Path, role: &str, pm: Arc<FakeSystemd>) -> Supervisor {
        let config_yaml = dir.join("config.yaml");
        std::fs::write(&config_yaml, "agent:\n  profile: ground_station\n").unwrap();
        let role_path = dir.join("mesh/role");
        write_role_file(&role_path, role).unwrap();
        let mut config = AgentConfig::load_from(&config_yaml, &dir.join("absent.conf"), &role_path);
        config.run_dir = dir.join("run");
        Supervisor::with_process_manager(config, Arc::new(BindOrchestrator::new()), pm)
    }

    #[test]
    fn the_registry_role_gates_match_the_role_table() {
        // The monitor gates on the registry's role_gate while transitions and
        // the boot pass read the role table; a unit the two disagree on is one
        // the monitor restarts behind a transition, or never supervises.
        let specs = crate::registry::build_specs();
        for unit in ALL_ROLE_UNITS {
            let spec = specs
                .iter()
                .find(|s| s.name == service_name(unit))
                .unwrap_or_else(|| panic!("{unit} has no registry row"));
            let mut gated: Vec<&str> = spec.role_gate.unwrap().split('|').collect();
            gated.sort_unstable();
            let mut owners: Vec<&str> = VALID_ROLES
                .iter()
                .copied()
                .filter(|r| role_units(r).contains(&unit))
                .collect();
            owners.sort_unstable();
            assert_eq!(gated, owners, "{unit}");
        }
        for role in VALID_ROLES {
            for unit in role_units(role) {
                assert!(ALL_ROLE_UNITS.contains(unit), "{unit}");
            }
            assert!(role_plane(role).starts_with("ados-wfb-"), "{role}");
        }
    }

    #[tokio::test]
    async fn a_direct_to_relay_transition_leaves_one_wfb_plane() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeSystemd::with_active(&["ados-wfb-rx"]));
        let mut sup = ground_station(dir.path(), "direct", fake.clone());

        let res = sup.apply_role("relay", "test").await.unwrap();

        assert_eq!(fake.planes(), vec!["ados-wfb-relay"]);
        assert!(fake.is_up("ados-batman"));
        assert_eq!(res.units_stopped, vec!["ados-wfb-rx.service"]);
        assert_eq!(
            res.units_started,
            vec!["ados-batman.service", "ados-wfb-relay.service"]
        );
        assert_eq!(sup.config().live_role(), "relay");

        // And back: the relay role's units stop and the receive plane returns.
        let back = sup.apply_role("direct", "test").await.unwrap();
        assert_eq!(fake.planes(), vec!["ados-wfb-rx"]);
        assert!(!fake.is_up("ados-batman"));
        assert_eq!(
            back.units_stopped,
            vec!["ados-wfb-relay.service", "ados-batman.service"]
        );
    }

    #[tokio::test]
    async fn a_transition_survives_the_stop_of_the_unit_that_requested_it() {
        // The requester lives in the receive-plane unit the transition stops
        // first, so the transition kills it mid-request. The transition must
        // still run to completion.
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeSystemd::with_active(&["ados-wfb-rx"]));
        let mut sup = ground_station(dir.path(), "direct", fake.clone());

        let sock = dir.path().join("supervisor.sock");
        let (roles_tx, mut roles_rx) = tokio::sync::mpsc::channel(4);
        let server = tokio::spawn({
            let sock = sock.clone();
            async move {
                crate::bind::control::serve(Arc::new(BindOrchestrator::new()), roles_tx, &sock)
                    .await
            }
        });
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let requester = tokio::spawn({
            let sock = sock.clone();
            async move {
                let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
                s.write_all(b"{\"op\":\"set_role\",\"role\":\"relay\",\"reason\":\"rest\"}\n")
                    .await
                    .unwrap();
                let mut reply = Vec::new();
                let _ = s.read_to_end(&mut reply).await;
                reply
            }
        });
        fake.host("ados-wfb-rx", requester.abort_handle());

        let req = tokio::time::timeout(Duration::from_secs(5), roles_rx.recv())
            .await
            .expect("the control socket must hand set_role to the supervisor loop")
            .expect("request");
        let reply = role_reply(sup.apply_role(&req.target, &req.reason).await);
        let _ = req.reply.send(reply);

        assert!(
            requester.await.unwrap_err().is_cancelled(),
            "the requester was inside the stopped unit"
        );
        assert_eq!(sup.config().live_role(), "relay");
        assert_eq!(fake.planes(), vec!["ados-wfb-relay"]);
        assert!(fake.is_up("ados-batman"));
        server.abort();
    }

    #[tokio::test]
    async fn refusals_have_no_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeSystemd::with_active(&["ados-wfb-rx"]));
        let mut sup = ground_station(dir.path(), "direct", fake.clone());

        assert_eq!(
            sup.apply_role("bogus", "test").await,
            Err(RoleError::InvalidRole("bogus".to_string()))
        );
        let noop = sup.apply_role("direct", "test").await.unwrap();
        assert!(noop.noop && noop.units_started.is_empty() && noop.units_stopped.is_empty());
        assert_eq!(fake.planes(), vec!["ados-wfb-rx"]);
        assert_eq!(sup.config().live_role(), "direct");
    }

    #[tokio::test]
    async fn the_boot_apply_stops_the_units_the_role_does_not_own() {
        // A unit pulled up with the supervisor may have read a stale sentinel;
        // the boot apply must leave only the configured role's units able to run.
        let dir = tempfile::tempdir().unwrap();
        let fake = FakeSystemd::with_active(&["ados-wfb-rx"]);
        let role_path = dir.path().join("mesh/role");
        apply_role_on_boot(&fake, "relay", &role_path).await;
        assert!(fake.planes().is_empty());
        assert_eq!(std::fs::read_to_string(&role_path).unwrap(), "relay\n");
    }

    #[test]
    fn write_role_file_is_atomic_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mesh/role");
        write_role_file(&p, "relay").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "relay\n");
        write_role_file(&p, "direct").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "direct\n");
    }
}
