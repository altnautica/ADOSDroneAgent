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
            crate::bind::control::serve(Arc::new(BindOrchestrator::new()), roles_tx, &sock).await
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
