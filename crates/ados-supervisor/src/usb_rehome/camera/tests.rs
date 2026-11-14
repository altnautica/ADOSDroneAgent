//! The reconciler against a fake sysfs and clock: a captured or secondary
//! camera that drops off the bus is recovered, one USB action at a time.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::os::{CameraOs, LastGood};
use super::*;
use crate::usb_rehome::topo::{ControlPath, UsbTopo};

/// A fake OS: the camera-state document, the enumerated V4L2 nodes and their
/// USB topology, hubs with per-port power, and the records written back.
#[derive(Default)]
struct FakeOs {
    cfg: CameraRecoveryConfig,
    state: RefCell<Value>,
    /// V4L2 node → USB topology of the enumerated cameras.
    nodes: RefCell<BTreeMap<String, UsbTopo>>,
    /// USB device nodes present in sysfs.
    present: RefCell<BTreeSet<String>>,
    /// `(hub, port)` pairs with per-port power.
    ppps: BTreeSet<(String, u32)>,
    last_good: RefCell<BTreeMap<String, LastGood>>,
    sidecar: RefCell<Value>,
    now_unix: u64,
}

fn topo(bind_id: &str) -> UsbTopo {
    UsbTopo {
        bind_id: bind_id.to_string(),
        ancestors: super::os::name_ancestors(bind_id),
    }
}

impl FakeOs {
    fn new() -> Self {
        Self {
            cfg: CameraRecoveryConfig::default(),
            now_unix: 1_000_000,
            ..Default::default()
        }
    }

    fn plug(&self, node: &str, bind_id: &str) {
        self.nodes
            .borrow_mut()
            .insert(node.to_string(), topo(bind_id));
        self.present.borrow_mut().insert(bind_id.to_string());
    }

    fn unplug(&self, node: &str) {
        if let Some(t) = self.nodes.borrow_mut().remove(node) {
            self.present.borrow_mut().remove(&t.bind_id);
        }
    }

    fn cameras(&self, cameras: Value) {
        *self.state.borrow_mut() = json!({
            "version": 2,
            "state": "ready",
            "updated_at_unix": self.now_unix as f64,
            "cameras": cameras,
        });
    }
}

impl CameraOs for FakeOs {
    fn config(&self) -> CameraRecoveryConfig {
        self.cfg.clone()
    }
    async fn camera_state(&self) -> Option<Value> {
        Some(self.state.borrow().clone())
    }
    fn now_unix(&self) -> u64 {
        self.now_unix
    }
    async fn read_last_good(&self, primary: bool, id: &str) -> Option<LastGood> {
        let key = super::os::last_good_name(primary, id);
        self.last_good.borrow().get(&key).cloned()
    }
    async fn write_last_good(&self, primary: bool, id: &str, lg: &LastGood) {
        let key = super::os::last_good_name(primary, id);
        self.last_good.borrow_mut().insert(key, lg.clone());
    }
    async fn video_topo(&self, dev_path: &str) -> Option<UsbTopo> {
        self.nodes.borrow().get(dev_path).cloned()
    }
    fn device_present(&self, bind_id: &str) -> bool {
        self.present.borrow().contains(bind_id)
    }
    async fn usb_ids(&self, _bind_id: &str) -> (String, String) {
        ("1bcf".into(), "2c99".into())
    }
    async fn protected_set(&self) -> (Vec<ControlPath>, Vec<UsbTopo>) {
        let radio = topo("1-1.1");
        (vec![ControlPath::Usb(radio.clone())], vec![radio])
    }
    fn ppps_capable(&self, hub: &str, port: u32) -> bool {
        self.ppps.contains(&(hub.to_string(), port))
    }
    fn within_boot_window(&self, _window: Duration) -> bool {
        false
    }
    async fn fc_armed(&self) -> Option<bool> {
        None
    }
    async fn radio_topo(&self) -> Option<UsbTopo> {
        Some(topo("1-1.1"))
    }
    fn write_sidecar(&self, body: &Value) {
        *self.sidecar.borrow_mut() = body.clone();
    }
}

fn recovery() -> CameraUsbRecovery {
    let dir = std::env::temp_dir().join("ados-camera-recovery-test-no-socket");
    CameraUsbRecovery::new(EventEmitter::with_socket("test", dir))
}

fn camera(id: &str, primary: bool, state: &str, node: &str) -> Value {
    json!({"id": id, "primary": primary, "captured": true, "state": state, "device_path": node})
}

/// Tick every 5 s from `start` for `secs`, collecting the plans.
async fn run(
    rec: &mut CameraUsbRecovery,
    os: &FakeOs,
    start: Instant,
    from_s: u64,
    to_s: u64,
) -> Vec<(u64, CameraRecoveryPlan)> {
    let mut plans = Vec::new();
    let mut t = from_s;
    while t <= to_s {
        if let Some(p) = rec.decide_with(os, start + Duration::from_secs(t)).await {
            plans.push((t, p));
        }
        t += 5;
    }
    plans
}

#[tokio::test]
async fn a_captured_camera_that_drops_off_the_bus_has_its_port_cycled() {
    let mut os = FakeOs::new();
    os.ppps.insert(("1-1".into(), 2));
    os.plug("/dev/video0", "1-1.2");
    os.cameras(json!([camera("front", true, "ready", "/dev/video0")]));
    let mut rec = recovery();
    let t0 = Instant::now();

    // Seen healthy: its location is recorded, nothing to do.
    assert!(run(&mut rec, &os, t0, 0, 0).await.is_empty());
    assert_eq!(
        os.last_good.borrow()["camera-last-good.json"].bind_id,
        "1-1.2"
    );
    assert_eq!(os.sidecar.borrow()["camera_usb_recovery_state"], "idle");

    // The device vanishes: camera-state reads it missing.
    os.unplug("/dev/video0");
    os.cameras(json!([camera("front", true, "missing", "/dev/video0")]));
    // Inside the confirm window: monitoring, no action.
    assert!(run(&mut rec, &os, t0, 5, 20).await.is_empty());
    assert_eq!(
        os.sidecar.borrow()["camera_usb_recovery_state"],
        "monitoring"
    );
    // Past the 20 s debounce: the camera's own port is re-enabled.
    let plans = run(&mut rec, &os, t0, 25, 25).await;
    assert_eq!(plans.len(), 1, "{plans:?}");
    assert_eq!(
        plans[0].1,
        CameraRecoveryPlan {
            camera_id: "front".into(),
            action: CameraRecoveryAction::CyclePort {
                hub: "1-1".into(),
                port: 2
            },
            attempt: 1,
        }
    );
    let side = os.sidecar.borrow().clone();
    assert_eq!(side["camera_usb_recovery_state"], "port_cycling");
    assert_eq!(side["camera_id"], "front");
    assert_eq!(side["cameras"][0]["camera_id"], "front");
    // The cooldown holds the next attempt off.
    assert!(run(&mut rec, &os, t0, 30, 80).await.is_empty());
    assert_eq!(run(&mut rec, &os, t0, 85, 85).await.len(), 1);
}

#[tokio::test]
async fn a_wedged_secondary_camera_is_rebound_in_place() {
    let os = FakeOs::new();
    os.plug("/dev/video0", "1-1.2");
    os.plug("/dev/video2", "1-1.3");
    os.cameras(json!([
        camera("front", true, "ready", "/dev/video0"),
        camera("belly", false, "ready", "/dev/video2"),
    ]));
    let mut rec = recovery();
    let t0 = Instant::now();
    run(&mut rec, &os, t0, 0, 0).await;
    // Each camera keeps its own location record.
    assert!(os
        .last_good
        .borrow()
        .contains_key("camera-last-good-belly.json"));

    // The belly camera stays enumerated but its source went silent.
    os.cameras(json!([
        camera("front", true, "ready", "/dev/video0"),
        camera("belly", false, "missing", "/dev/video2"),
    ]));
    let plans = run(&mut rec, &os, t0, 5, 25).await;
    assert_eq!(plans.len(), 1, "{plans:?}");
    assert_eq!(plans[0].1.camera_id, "belly");
    assert_eq!(
        plans[0].1.action,
        CameraRecoveryAction::RebindDevice {
            bind_id: "1-1.3".into()
        }
    );
    // The top level shows the camera in recovery, not the healthy primary.
    assert_eq!(os.sidecar.borrow()["camera_id"], "belly");
    assert_eq!(
        os.sidecar.borrow()["camera_usb_recovery_state"],
        "rebinding"
    );
    // Still missing on the next tick: the belly retries under its cooldown
    // while the primary's own entry stays idle.
    assert!(run(&mut rec, &os, t0, 30, 30).await.is_empty());
    assert_eq!(os.sidecar.borrow()["camera_usb_recovery_state"], "retrying");
    assert_eq!(os.sidecar.borrow()["cameras"][0]["camera_id"], "front");
    assert_eq!(
        os.sidecar.borrow()["cameras"][0]["camera_usb_recovery_state"],
        "idle"
    );
}

#[tokio::test]
async fn two_cameras_lost_together_take_turns_on_the_bus() {
    let mut os = FakeOs::new();
    os.ppps.insert(("1-1".into(), 2));
    os.ppps.insert(("1-1".into(), 3));
    os.plug("/dev/video0", "1-1.2");
    os.plug("/dev/video2", "1-1.3");
    os.cameras(json!([
        camera("front", true, "ready", "/dev/video0"),
        camera("belly", false, "ready", "/dev/video2"),
    ]));
    let mut rec = recovery();
    let t0 = Instant::now();
    run(&mut rec, &os, t0, 0, 0).await;

    // A brown-out drops both off the hub at once.
    os.unplug("/dev/video0");
    os.unplug("/dev/video2");
    os.cameras(json!([
        camera("front", true, "missing", "/dev/video0"),
        camera("belly", false, "missing", "/dev/video2"),
    ]));
    let plans = run(&mut rec, &os, t0, 5, 200).await;
    let order: Vec<(u64, &str)> = plans
        .iter()
        .map(|(t, p)| (*t, p.camera_id.as_str()))
        .collect();
    // One action per cooldown across the bus, alternating, neither starved.
    assert_eq!(
        order,
        vec![(25, "front"), (85, "belly"), (145, "front")],
        "{plans:?}"
    );
}

#[tokio::test]
async fn a_camera_never_seen_on_this_rig_is_not_expected() {
    let os = FakeOs::new();
    os.cameras(json!([camera("belly", false, "missing", "/dev/video2")]));
    let mut rec = recovery();
    assert!(run(&mut rec, &os, Instant::now(), 0, 120).await.is_empty());
    assert_eq!(os.sidecar.borrow()["camera_usb_recovery_state"], "idle");
    assert_eq!(os.sidecar.borrow()["expected"], false);
}

#[tokio::test]
async fn a_stale_camera_state_is_not_acted_on() {
    let os = FakeOs::new();
    os.cameras(json!([camera("front", true, "missing", "/dev/video0")]));
    os.state.borrow_mut()["updated_at_unix"] = json!((os.now_unix - 600) as f64);
    let mut rec = recovery();
    assert!(run(&mut rec, &os, Instant::now(), 0, 120).await.is_empty());
    assert_eq!(*os.sidecar.borrow(), Value::Null);
}
