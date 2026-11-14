use super::*;
use ados_protocol::vision_status::SourceError;

/// A fake device tree: a `dev` dir standing in for `/dev`, a sysfs class dir
/// and a USB device the camera's V4L2 node hangs off.
struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        let f = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(f.path("dev")).unwrap();
        std::fs::create_dir_all(f.path("sys_v4l")).unwrap();
        f
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn sources(&self) -> PresenceSources {
        PresenceSources {
            sys_v4l: self.path("sys_v4l"),
            vision_status: self.path("vision-status.json"),
        }
    }

    /// A device node file at `dev/<name>`; returns its path.
    fn node(&self, name: &str) -> String {
        let p = self.path(&format!("dev/{name}"));
        std::fs::write(&p, b"").unwrap();
        p.to_string_lossy().into_owned()
    }

    /// Enumerate `videoN` under a USB device with these ids.
    fn usb_camera(&self, video: &str, vid: &str, pid: &str, serial: Option<&str>) {
        self.usb_camera_at("1-1", video, vid, pid, serial);
    }

    /// Enumerate `videoN` under the USB device on `port` with these ids.
    fn usb_camera_at(&self, port: &str, video: &str, vid: &str, pid: &str, serial: Option<&str>) {
        let usb = self.path(&format!("devices/usb1/{port}"));
        let intf = usb.join(format!("{port}:1.0"));
        std::fs::create_dir_all(intf.join("video4linux").join(video)).unwrap();
        std::fs::write(usb.join("idVendor"), format!("{vid}\n")).unwrap();
        std::fs::write(usb.join("idProduct"), format!("{pid}\n")).unwrap();
        if let Some(s) = serial {
            std::fs::write(usb.join("serial"), format!("{s}\n")).unwrap();
        }
        let class = self.path("sys_v4l").join(video);
        std::fs::create_dir_all(&class).unwrap();
        std::os::unix::fs::symlink(&intf, class.join("device")).unwrap();
    }

    fn unplug_usb(&self) {
        std::fs::remove_dir_all(self.path("sys_v4l")).unwrap();
        std::fs::create_dir_all(self.path("sys_v4l")).unwrap();
        std::fs::remove_dir_all(self.path("devices")).unwrap();
    }

    /// Unplug the USB device on `port`, which enumerated `videoN`.
    fn unplug_port(&self, port: &str, video: &str) {
        std::fs::remove_dir_all(self.path("sys_v4l").join(video)).unwrap();
        std::fs::remove_dir_all(self.path(&format!("devices/usb1/{port}"))).unwrap();
    }

    fn vision(&self, errors: &[(&str, &str)]) {
        let status = VisionStatus {
            source_errors: errors
                .iter()
                .map(|(camera, error)| SourceError {
                    camera: camera.to_string(),
                    error: error.to_string(),
                })
                .collect(),
            ..Default::default()
        };
        std::fs::write(
            self.path("vision-status.json"),
            serde_json::to_vec(&status).unwrap(),
        )
        .unwrap();
    }
}

fn legs(yaml: &str) -> Vec<CameraLeg> {
    serde_norway::from_str(yaml).unwrap()
}

fn discovered_ready(path: &str) -> DiscoveredPrimary {
    DiscoveredPrimary {
        state: CameraState::Ready,
        device_path: Some(path.to_string()),
    }
}

fn by_id<'a>(all: &'a [CameraPresence], id: &str) -> &'a CameraPresence {
    all.iter().find(|p| p.id == id).expect(id)
}

#[test]
fn a_captured_camera_reads_missing_once_its_node_is_gone() {
    let f = Fake::new();
    let node = f.node("video0");
    let cams = legs(&format!(
        "- {{id: front, role: primary, source: {node}, capture: true}}\n"
    ));
    let found = assess(&cams, &discovered_ready("framebus://front"), &f.sources());
    let front = by_id(&found, "front");
    assert!(front.primary && front.captured);
    assert_eq!(front.state, CameraState::Ready);
    assert_eq!(front.device_path.as_deref(), Some(node.as_str()));

    std::fs::remove_file(&node).unwrap();
    let found = assess(&cams, &discovered_ready("framebus://front"), &f.sources());
    let front = by_id(&found, "front");
    assert_eq!(front.state, CameraState::Missing);
    assert!(front.reason.as_deref().unwrap().contains("is gone"));
    // The locator stays named, so recovery knows what it is looking for.
    assert_eq!(front.device_path.as_deref(), Some(node.as_str()));
}

#[test]
fn a_usb_fingerprint_finds_the_camera_wherever_its_node_is() {
    let f = Fake::new();
    f.usb_camera("video4", "1BCF", "2C99", Some("SN1"));
    let cams = legs(
        "- {id: front, role: primary, source: /dev/video0, capture: true, match: {usb: '1bcf:2c99:sn1'}}\n\
         - {id: belly, role: down, source: /dev/video2, capture: true, match: {usb: '1bcf:2c99:other'}}\n",
    );
    let found = assess(&cams, &discovered_ready("framebus://front"), &f.sources());
    assert_eq!(by_id(&found, "front").state, CameraState::Ready);
    assert_eq!(
        by_id(&found, "front").device_path.as_deref(),
        Some("/dev/video4")
    );
    // A different serial is a different camera: the belly one is not there.
    assert_eq!(by_id(&found, "belly").state, CameraState::Missing);
    assert!(!by_id(&found, "belly").primary);

    f.unplug_usb();
    let found = assess(&cams, &discovered_ready("framebus://front"), &f.sources());
    assert_eq!(by_id(&found, "front").state, CameraState::Missing);
}

#[test]
fn the_vision_engine_decides_whether_a_present_captured_camera_is_live() {
    let f = Fake::new();
    let node = f.node("video0");
    let cams = legs(&format!(
        "- {{id: front, role: primary, source: {node}, capture: true}}\n"
    ));
    let d = discovered_ready("framebus://front");

    // Enumerated but silent: a wedged device, which a USB rebind recovers.
    f.vision(&[("front", "delivered no frame for 10 s")]);
    let front = assess(&cams, &d, &f.sources()).remove(0);
    assert_eq!(front.state, CameraState::Missing);
    assert_eq!(front.reason.as_deref(), Some("delivered no frame for 10 s"));

    // A failure to open or negotiate is surfaced, not read as a lost device.
    f.vision(&[("front", "no usable capture format")]);
    let front = assess(&cams, &d, &f.sources()).remove(0);
    assert_eq!(front.state, CameraState::Error);

    // Another camera's failure is not this one's.
    f.vision(&[("belly", "delivered no frame for 10 s")]);
    assert_eq!(assess(&cams, &d, &f.sources())[0].state, CameraState::Ready);

    // A sidecar the engine stopped writing no longer speaks for the camera.
    f.vision(&[("front", "delivered no frame for 10 s")]);
    let old = SystemTime::now() - Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(f.path("vision-status.json"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    assert_eq!(assess(&cams, &d, &f.sources())[0].state, CameraState::Ready);
}

#[test]
fn a_secondary_leg_is_judged_by_its_device() {
    let f = Fake::new();
    let belly = f.node("video2");
    let cams = legs(&format!(
        "- {{id: main, role: primary, source: /dev/video0}}\n\
         - {{id: belly, role: down, source: {belly}}}\n\
         - {{id: spare, source: /dev/video9, enabled: false}}\n\
         - {{id: pod, source: 'rtsp://pod.local/live'}}\n\
         - {{id: rib, source: csi}}\n"
    ));
    let d = discovered_ready("/dev/video0");
    let found = assess(&cams, &d, &f.sources());
    let ids: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    // Disabled, network and device-less legs have nothing to watch.
    assert_eq!(ids, vec!["main", "belly"]);
    // The direct primary mirrors discovery.
    assert_eq!(by_id(&found, "main").state, CameraState::Ready);
    assert_eq!(
        by_id(&found, "main").device_path.as_deref(),
        Some("/dev/video0")
    );
    assert_eq!(by_id(&found, "belly").state, CameraState::Ready);
    // The engine's verdict applies to captured cameras only.
    f.vision(&[("belly", "delivered no frame for 10 s")]);
    assert_eq!(
        by_id(&assess(&cams, &d, &f.sources()), "belly").state,
        CameraState::Ready
    );

    std::fs::remove_file(&belly).unwrap();
    let found = assess(&cams, &d, &f.sources());
    assert_eq!(by_id(&found, "belly").state, CameraState::Missing);
    assert_eq!(by_id(&found, "main").state, CameraState::Ready);
}

#[test]
fn a_node_with_no_roster_mirrors_discovery_as_main() {
    let f = Fake::new();
    let missing = DiscoveredPrimary {
        state: CameraState::Missing,
        device_path: None,
    };
    let found = assess(&[], &missing, &f.sources());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "main");
    assert!(found[0].primary && !found[0].captured);
    assert_eq!(found[0].state, CameraState::Missing);
}

#[test]
fn a_camera_whose_frames_do_not_decode_is_an_error_not_a_lost_device() {
    let f = Fake::new();
    let node = f.node("video0");
    let cams = legs(&format!(
        "- {{id: front, role: primary, source: {node}, capture: true}}\n"
    ));
    // The source reports frames that arrive and fail to decode: the device is
    // there and talking, so it reads `error`, which recovery does not rebind.
    let reason = "frames arrive but none decoded for 3 s: no end-of-image marker";
    f.vision(&[("front", reason)]);
    let d = discovered_ready("framebus://front");
    let front = assess(&cams, &d, &f.sources()).remove(0);
    assert_eq!(front.state, CameraState::Error);
    assert_eq!(front.device_path.as_deref(), Some(node.as_str()));
}

#[test]
fn identical_cameras_are_told_apart_by_their_source_nodes() {
    let f = Fake::new();
    f.usb_camera_at("1-1", "video0", "046d", "0825", None);
    f.usb_camera_at("1-2", "video2", "046d", "0825", None);
    let front_node = f.node("video0");
    let belly_node = f.node("video2");
    let cams = legs(&format!(
        "- {{id: front, role: primary, source: {front_node}, capture: true, match: {{usb: '046d:0825'}}}}\n\
         - {{id: belly, role: down, source: {belly_node}, capture: true, match: {{usb: '046d:0825'}}}}\n"
    ));
    let d = discovered_ready("framebus://front");
    let found = assess(&cams, &d, &f.sources());
    assert_eq!(
        by_id(&found, "front").device_path.as_deref(),
        Some(front_node.as_str())
    );
    assert_eq!(
        by_id(&found, "belly").device_path.as_deref(),
        Some(belly_node.as_str())
    );

    // The belly camera drops off the bus and its capture goes silent.
    f.unplug_port("1-2", "video2");
    std::fs::remove_file(&belly_node).unwrap();
    f.vision(&[("belly", "delivered no frame for 10 s")]);
    let found = assess(&cams, &d, &f.sources());
    let front = by_id(&found, "front");
    assert_eq!(front.state, CameraState::Ready);
    assert_eq!(front.device_path.as_deref(), Some(front_node.as_str()));
    let belly = by_id(&found, "belly");
    assert_eq!(belly.state, CameraState::Missing);
    // Recovery is pointed at the belly camera's own locator, never at the
    // healthy front camera's node.
    assert_eq!(belly.device_path.as_deref(), Some(belly_node.as_str()));
}

#[test]
fn a_fingerprint_two_cameras_share_names_no_node() {
    let f = Fake::new();
    f.usb_camera_at("1-1", "video0", "046d", "0825", None);
    f.usb_camera_at("1-2", "video2", "046d", "0825", None);
    let cams = legs(
        "- {id: front, role: primary, source: usb, capture: true, match: {usb: '046d:0825'}}\n\
         - {id: belly, role: down, source: usb, capture: true, match: {usb: '046d:0825'}}\n",
    );
    let d = discovered_ready("framebus://front");
    f.vision(&[("belly", "delivered no frame for 10 s")]);
    let found = assess(&cams, &d, &f.sources());
    // Either device could be either camera: both are present, and neither
    // names a node recovery could act on.
    assert_eq!(by_id(&found, "front").state, CameraState::Ready);
    assert_eq!(by_id(&found, "belly").state, CameraState::Missing);
    for id in ["front", "belly"] {
        assert_eq!(by_id(&found, id).device_path, None, "{id}");
    }

    // One camera left on the bus is still either of them.
    f.unplug_port("1-2", "video2");
    let found = assess(&cams, &d, &f.sources());
    for id in ["front", "belly"] {
        assert_eq!(by_id(&found, id).device_path, None, "{id}");
    }
}

#[test]
fn a_secondary_never_takes_the_node_discovery_opened_for_the_primary() {
    let f = Fake::new();
    f.usb_camera_at("1-1", "video0", "046d", "0825", None);
    f.usb_camera_at("1-2", "video2", "046d", "0825", None);
    let main_node = f.node("video0");
    let cams = legs(
        "- {id: main, role: primary, source: usb, match: {usb: '046d:0825'}}\n\
         - {id: belly, role: down, source: usb, match: {usb: '046d:0825'}}\n",
    );
    let d = discovered_ready(&main_node);
    let belly = by_id(&assess(&cams, &d, &f.sources()), "belly").clone();
    assert_eq!(belly.state, CameraState::Ready);
    assert_eq!(belly.device_path.as_deref(), Some("/dev/video2"));

    // With the belly camera unplugged, the primary's device is not it.
    f.unplug_port("1-2", "video2");
    let found = assess(&cams, &d, &f.sources());
    let belly = by_id(&found, "belly");
    assert_eq!(belly.state, CameraState::Missing);
    assert_eq!(belly.device_path, None);
    assert_eq!(
        by_id(&found, "main").device_path.as_deref(),
        Some(main_node.as_str())
    );
}
