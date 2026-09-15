//! Boot-time camera presence probe with apply-verify-auto-revert.
//!
//! The camera-overlay provisioner writes a boot-critical device-tree edit
//! BEFORE the sensor can be confirmed present — it has to, because on most
//! boards the CSI sensor does not enumerate until the overlay is in the DT and
//! the box has rebooted. That leaves a window where a board with no camera
//! attached (or the wrong sensor attached) carries a boot-config edit forever
//! with nothing to undo it. That is the failure class that has already cost a
//! board on the bench.
//!
//! So the provisioner arms `/etc/ados/camera.probation` and this probe is the
//! thing that reads it, once, on the next boot:
//!
//! * CONFIRM: the sensor bound this boot (the expected `/dev/videoN` node
//!   exists, or some `/sys/class/video4linux/videoN/name` matches the declared
//!   sensor). Clear probation and mark the camera confirmed in `camera.conf`.
//! * AUTO-REVERT: the sensor never bound. Restore the boot config from the
//!   install-time snapshot (only when it is at least
//!   [`MIN_SNAPSHOT_BYTES`] — the truncation guard), rewrite `camera.conf` to a
//!   camera-absent block, and clear the marker, so the next boot is the
//!   known-good one.
//!
//! This is the camera counterpart of the display probe
//! (`ados_display::probe`) and deliberately mirrors its shape: injectable
//! roots, a pure `apply_decision` so both branches are testable against temp
//! trees, and a poll window because the sensor binds late.
//!
//! Pure filesystem IO — nothing here is `cfg(target_os = "linux")`, so the
//! confirm/revert decision is compiled and tested on every host. Only the
//! meaning of the paths is Linux-specific.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How long to wait for the CSI sensor to enumerate. The sensor driver binds
/// through the media/V4L2 stack well after userspace starts on several boards,
/// so poll past the slowest observed board before judging it absent. Matches
/// the display probe's window.
pub const BIND_POLL_SECONDS: f64 = 20.0;
/// Cadence of the bind poll.
pub const BIND_POLL_INTERVAL_SECONDS: f64 = 0.5;

/// A valid boot-config snapshot is larger than this. A smaller one is treated
/// as truncated and is NEVER restored over a working boot config: an empty
/// snapshot written over `extlinux.conf` is the unbootable board this probe
/// exists to prevent.
pub const MIN_SNAPSHOT_BYTES: usize = 100;

/// Filesystem roots the probe reads + writes. Defaulted to the real paths;
/// overridden to temp trees in tests.
#[derive(Debug, Clone)]
pub struct CameraProbePaths {
    /// Where `/dev/video*` nodes appear.
    pub dev_dir: PathBuf,
    /// `/sys/class/video4linux`, whose `videoN/name` carries the driver-reported
    /// sensor/ISP name.
    pub sys_video4linux_dir: PathBuf,
    /// The camera state file the camera service, the heartbeat assembler and the
    /// setup hardware-check all read.
    pub camera_conf: PathBuf,
    /// The probation marker the overlay provisioner arms.
    pub camera_probation: PathBuf,
}

impl Default for CameraProbePaths {
    fn default() -> Self {
        Self {
            dev_dir: PathBuf::from("/dev"),
            sys_video4linux_dir: PathBuf::from("/sys/class/video4linux"),
            camera_conf: PathBuf::from("/etc/ados/camera.conf"),
            camera_probation: PathBuf::from("/etc/ados/camera.probation"),
        }
    }
}

/// The terminal outcome of a probe run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraProbeOutcome {
    /// No probation marker present — nothing to do. The overwhelmingly common
    /// case, which is why the unit is `ConditionPathExists`-gated on the marker.
    NoProbation,
    /// The sensor bound; probation cleared, overlay retained. Carries the node
    /// or sysfs name that proved it.
    Confirmed { evidence: String },
    /// The sensor never bound; the boot config was restored (when a valid
    /// snapshot existed) and the camera marked absent.
    Reverted { boot_config_restored: bool },
}

/// Parse a `key=value` marker file, ignoring blanks and `#` comments. Empty map
/// on a missing/unreadable file.
pub fn parse_marker(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

/// Whether `name` is a `videoN` entry with an all-digit suffix.
fn is_video_entry(name: &str) -> bool {
    name.strip_prefix("video")
        .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

/// Normalise a sensor string to the tokens worth matching against a
/// driver-reported V4L2 name: lowercase alphanumeric words of 3+ characters.
///
/// `"Sony IMX214"` yields `["sony", "imx214"]`, and the driver name
/// `"sunxi-vin: imx214_mipi"` matches on `imx214`. Words shorter than three
/// characters are dropped so a stray `"a"`/`"hd"` cannot match everything.
fn sensor_tokens(sensor: &str) -> Vec<String> {
    sensor
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_ascii_lowercase())
        .collect()
}

/// The V4L2 node whose driver-reported name matches the declared `sensor`, or
/// `None`. Walks `<sys_video4linux>/video*/name`.
pub fn sensor_node_by_name(sys_video4linux_dir: &Path, sensor: &str) -> Option<String> {
    let tokens = sensor_tokens(sensor);
    if tokens.is_empty() {
        return None;
    }
    let entries = std::fs::read_dir(sys_video4linux_dir).ok()?;
    let mut matched: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let node = entry.file_name().to_string_lossy().to_string();
        if !is_video_entry(&node) {
            continue;
        }
        let Ok(reported) = std::fs::read_to_string(entry.path().join("name")) else {
            continue;
        };
        let reported = reported.trim().to_ascii_lowercase();
        if tokens.iter().any(|t| reported.contains(t.as_str())) {
            matched.push(node);
        }
    }
    // Deterministic across readdir order.
    matched.sort();
    matched.into_iter().next()
}

/// Confirm the camera: the expected node exists, else a V4L2 node reports the
/// declared sensor. Returns the evidence string that proved it.
///
/// Both halves are needed. The node path from the board profile is the precise
/// answer but a board can enumerate the sensor on a different index than the
/// profile's first mode declares; the sysfs name match catches that without
/// accepting "some camera exists" as proof of THIS camera — an unrelated UVC
/// webcam reports its own name and does not match the sensor tokens.
pub fn camera_present(
    dev_dir: &Path,
    sys_video4linux_dir: &Path,
    expected_node: &str,
    sensor: &str,
) -> Option<String> {
    let expected = expected_node.trim();
    if !expected.is_empty() {
        // The marker carries an absolute `/dev/videoN`; resolve its basename
        // under the injected dev root so a test tree works unchanged.
        let base = Path::new(expected)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        if !base.is_empty() && dev_dir.join(&base).exists() {
            return Some(expected.to_string());
        }
    }
    sensor_node_by_name(sys_video4linux_dir, sensor).map(|node| format!("{node} ({sensor})"))
}

/// Render the `camera.conf` body for a confirmed camera, preserving every key
/// the provisioner wrote and only advancing the state.
///
/// A rewrite-from-scratch would drop `overlay_ref` / `vendor_isp` /
/// `default_mode`, which the camera service needs to actually open the sensor,
/// so the confirm path is a merge and not a replace.
fn confirmed_conf(existing: &BTreeMap<String, String>, evidence: &str) -> String {
    let mut body = String::from(
        "# Written by the camera probe after the CSI sensor was CONFIRMED present\n\
         # on the boot following the overlay apply. The overlay is retained.\n",
    );
    let mut merged = existing.clone();
    merged.insert("camera_present".to_string(), "true".to_string());
    merged.insert("overlay_state".to_string(), "confirmed".to_string());
    merged.insert("camera_evidence".to_string(), evidence.to_string());
    for (k, v) in &merged {
        body.push_str(k);
        body.push('=');
        body.push_str(v);
        body.push('\n');
    }
    body
}

/// Camera confirmed: advance `camera.conf` and clear probation.
pub fn confirm(paths: &CameraProbePaths, evidence: &str) -> std::io::Result<()> {
    if let Some(parent) = paths.camera_conf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = parse_marker(&paths.camera_conf);
    std::fs::write(
        &paths.camera_conf,
        confirmed_conf(&existing, evidence).as_bytes(),
    )?;
    let _ = std::fs::remove_file(&paths.camera_probation);
    Ok(())
}

/// Camera never bound: restore the boot-config snapshot (only when it clears the
/// truncation floor), rewrite `camera.conf` to a camera-absent block, and clear
/// the marker. Returns whether the boot config was actually restored.
pub fn revert(
    paths: &CameraProbePaths,
    marker: &BTreeMap<String, String>,
) -> std::io::Result<bool> {
    let snapshot = marker.get("snapshot").map(|s| s.trim()).unwrap_or("");
    let boot_config = marker.get("boot_config").map(|s| s.trim()).unwrap_or("");
    let mut restored = false;
    if !snapshot.is_empty() && !boot_config.is_empty() {
        let snap_path = Path::new(snapshot);
        let boot_path = Path::new(boot_config);
        if snap_path.is_file() {
            let data = std::fs::read(snap_path)?;
            if data.len() >= MIN_SNAPSHOT_BYTES {
                std::fs::write(boot_path, &data)?;
                restored = true;
            } else {
                tracing::warn!(
                    bytes = data.len(),
                    snapshot = snapshot,
                    "camera probe snapshot too small to restore"
                );
            }
        }
    }

    if let Some(parent) = paths.camera_conf.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let camera_id = marker.get("camera_id").map(|s| s.as_str()).unwrap_or("none");
    let board = marker.get("board").map(|s| s.as_str()).unwrap_or("");
    let body = format!(
        "# Written by the camera probe after an unconfirmed CSI overlay failed to\n\
         # bind a sensor. The boot config was restored from the install-time\n\
         # snapshot and the camera marked absent.\n\
         camera_id={camera_id}\n\
         board={board}\n\
         camera_present=false\n\
         overlay_state=reverted\n"
    );
    std::fs::write(&paths.camera_conf, body)?;
    let _ = std::fs::remove_file(&paths.camera_probation);
    Ok(restored)
}

/// Decide confirm vs revert from an already-taken presence reading (no
/// polling). Pure over the reading so both branches are testable without
/// sleeping. Performs the side effects and returns the outcome.
pub fn apply_decision(
    paths: &CameraProbePaths,
    marker: &BTreeMap<String, String>,
    present: Option<String>,
) -> std::io::Result<CameraProbeOutcome> {
    match present {
        Some(evidence) => {
            confirm(paths, &evidence)?;
            Ok(CameraProbeOutcome::Confirmed { evidence })
        }
        None => {
            let restored = revert(paths, marker)?;
            Ok(CameraProbeOutcome::Reverted {
                boot_config_restored: restored,
            })
        }
    }
}

/// Run the probe: a no-op when no probation marker exists; otherwise poll for
/// the sensor up to the late-bind window, then confirm or revert.
pub fn run(paths: &CameraProbePaths) -> std::io::Result<CameraProbeOutcome> {
    run_with_window(
        paths,
        std::time::Duration::from_secs_f64(BIND_POLL_SECONDS),
    )
}

/// [`run`] with an explicit late-bind window, so a test drives the whole
/// marker-read → poll → confirm/revert path without waiting 20 s.
pub fn run_with_window(
    paths: &CameraProbePaths,
    window: std::time::Duration,
) -> std::io::Result<CameraProbeOutcome> {
    if !paths.camera_probation.exists() {
        tracing::info!("camera probe no-op: no probation marker");
        return Ok(CameraProbeOutcome::NoProbation);
    }
    let marker = parse_marker(&paths.camera_probation);
    let expected_node = marker
        .get("expected_node")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let sensor = marker
        .get("sensor")
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let present = wait_for_sensor(paths, &expected_node, &sensor, window);
    let outcome = apply_decision(paths, &marker, present)?;
    match &outcome {
        CameraProbeOutcome::Confirmed { evidence } => tracing::info!(
            evidence = %evidence,
            camera = marker.get("camera_id").map(|s| s.as_str()).unwrap_or(""),
            "camera overlay confirmed"
        ),
        CameraProbeOutcome::Reverted {
            boot_config_restored,
        } => tracing::warn!(
            boot_config_restored,
            camera = marker.get("camera_id").map(|s| s.as_str()).unwrap_or(""),
            overlay = marker.get("overlay").map(|s| s.as_str()).unwrap_or(""),
            "camera overlay auto-reverted: no sensor bound"
        ),
        CameraProbeOutcome::NoProbation => {}
    }
    Ok(outcome)
}

/// Poll for the sensor to enumerate, up to the late-bind window.
fn wait_for_sensor(
    paths: &CameraProbePaths,
    expected_node: &str,
    sensor: &str,
    window: std::time::Duration,
) -> Option<String> {
    let deadline = std::time::Instant::now() + window;
    loop {
        if let Some(found) = camera_present(
            &paths.dev_dir,
            &paths.sys_video4linux_dir,
            expected_node,
            sensor,
        ) {
            return Some(found);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_secs_f64(
            BIND_POLL_INTERVAL_SECONDS,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probation marker + boot config + snapshot in a temp tree, shaped
    /// exactly as `scripts/drivers/install-camera-overlay.sh` writes them.
    struct Tree {
        _dir: tempfile::TempDir,
        paths: CameraProbePaths,
        boot_config: PathBuf,
        snapshot: PathBuf,
    }

    const GOOD_BOOT_CONFIG: &str = "# extlinux.conf (known good)\nlabel ados\n  kernel /Image\n  fdt /dtb\n  append root=/dev/mmcblk0p2 rootwait console=ttyS0,115200\n";

    fn tree(snapshot_body: &str) -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dev = root.join("dev");
        let sys = root.join("sys/class/video4linux");
        let etc = root.join("etc/ados");
        let boot = root.join("boot");
        for d in [&dev, &sys, &etc, &boot] {
            std::fs::create_dir_all(d).unwrap();
        }
        let boot_config = boot.join("extlinux.conf");
        let snapshot = boot.join("extlinux.conf.ados-camera.bak");
        // The live boot config carries the overlay edit; the snapshot is the
        // pre-edit known-good copy.
        std::fs::write(
            &boot_config,
            format!("{GOOD_BOOT_CONFIG}  fdtoverlays /dtbo/radxa-camera-13m-214.dtbo\n"),
        )
        .unwrap();
        std::fs::write(&snapshot, snapshot_body).unwrap();
        let paths = CameraProbePaths {
            dev_dir: dev,
            sys_video4linux_dir: sys,
            camera_conf: etc.join("camera.conf"),
            camera_probation: etc.join("camera.probation"),
        };
        std::fs::write(
            &paths.camera_conf,
            "camera_id=csi-imx214\nboard=cubie-a7z\nsensor=Sony IMX214\ncamera_present=true\noverlay_ref=radxa-camera-13m-214\nvendor_isp=true\ndefault_mode=1920x1080@30\noverlay_state=pending_reboot\n",
        )
        .unwrap();
        std::fs::write(
            &paths.camera_probation,
            format!(
                "# install-camera-overlay.sh probation\ncamera_id=csi-imx214\nboard=cubie-a7z\noverlay=radxa-camera-13m-214\nexpected_node=/dev/video0\nsensor=Sony IMX214\nsnapshot={}\nboot_config={}\n",
                snapshot.display(),
                boot_config.display()
            ),
        )
        .unwrap();
        Tree {
            _dir: dir,
            paths,
            boot_config,
            snapshot,
        }
    }

    #[test]
    fn no_probation_marker_is_a_clean_no_op() {
        let t = tree(GOOD_BOOT_CONFIG);
        std::fs::remove_file(&t.paths.camera_probation).unwrap();
        let before = std::fs::read_to_string(&t.paths.camera_conf).unwrap();
        assert_eq!(run(&t.paths).unwrap(), CameraProbeOutcome::NoProbation);
        // Nothing touched: not the boot config, not camera.conf.
        assert_eq!(std::fs::read_to_string(&t.paths.camera_conf).unwrap(), before);
        assert!(std::fs::read_to_string(&t.boot_config)
            .unwrap()
            .contains("fdtoverlays"));
    }

    #[test]
    fn a_bound_sensor_confirms_and_keeps_the_overlay() {
        let t = tree(GOOD_BOOT_CONFIG);
        // The sensor enumerated this boot.
        std::fs::write(t.paths.dev_dir.join("video0"), b"").unwrap();

        let marker = parse_marker(&t.paths.camera_probation);
        let present = camera_present(
            &t.paths.dev_dir,
            &t.paths.sys_video4linux_dir,
            "/dev/video0",
            "Sony IMX214",
        );
        assert_eq!(present.as_deref(), Some("/dev/video0"));
        let outcome = apply_decision(&t.paths, &marker, present).unwrap();
        assert_eq!(
            outcome,
            CameraProbeOutcome::Confirmed {
                evidence: "/dev/video0".to_string()
            }
        );

        // The boot config is untouched — a confirmed overlay is kept.
        assert!(std::fs::read_to_string(&t.boot_config)
            .unwrap()
            .contains("fdtoverlays"));
        // Probation is cleared so the probe never runs again for this apply.
        assert!(!t.paths.camera_probation.exists());
        // camera.conf advanced WITHOUT losing the keys the camera service needs
        // to open the sensor.
        let conf = parse_marker(&t.paths.camera_conf);
        assert_eq!(conf.get("overlay_state").map(String::as_str), Some("confirmed"));
        assert_eq!(conf.get("camera_present").map(String::as_str), Some("true"));
        assert_eq!(
            conf.get("default_mode").map(String::as_str),
            Some("1920x1080@30"),
            "the confirm path must merge, not replace"
        );
        assert_eq!(conf.get("vendor_isp").map(String::as_str), Some("true"));
    }

    #[test]
    fn a_sensor_on_an_unexpected_node_still_confirms_by_its_reported_name() {
        // The profile's first mode says /dev/video0 but the board enumerated the
        // sensor on video3. That is a bound camera, not an absent one.
        let t = tree(GOOD_BOOT_CONFIG);
        let n = t.paths.sys_video4linux_dir.join("video3");
        std::fs::create_dir_all(&n).unwrap();
        std::fs::write(n.join("name"), b"sunxi-vin: imx214_mipi\n").unwrap();

        let present = camera_present(
            &t.paths.dev_dir,
            &t.paths.sys_video4linux_dir,
            "/dev/video0",
            "Sony IMX214",
        );
        assert_eq!(present.as_deref(), Some("video3 (Sony IMX214)"));
    }

    #[test]
    fn an_unrelated_webcam_is_not_proof_of_this_camera() {
        // "some /dev/video* exists" must never confirm a CSI overlay: a USB
        // webcam on a board whose CSI sensor never bound would otherwise keep a
        // boot-config edit that does nothing.
        let t = tree(GOOD_BOOT_CONFIG);
        let n = t.paths.sys_video4linux_dir.join("video1");
        std::fs::create_dir_all(&n).unwrap();
        std::fs::write(n.join("name"), b"HD Pro Webcam C920\n").unwrap();
        std::fs::write(t.paths.dev_dir.join("video1"), b"").unwrap();

        assert!(camera_present(
            &t.paths.dev_dir,
            &t.paths.sys_video4linux_dir,
            "/dev/video0",
            "Sony IMX214",
        )
        .is_none());
    }

    #[test]
    fn an_absent_sensor_auto_reverts_the_boot_config() {
        // The brick case: the overlay was applied blind on a board with no
        // camera. The next boot must restore the known-good boot config.
        let t = tree(GOOD_BOOT_CONFIG);
        let marker = parse_marker(&t.paths.camera_probation);

        let outcome = apply_decision(&t.paths, &marker, None).unwrap();
        assert_eq!(
            outcome,
            CameraProbeOutcome::Reverted {
                boot_config_restored: true
            }
        );
        assert_eq!(
            std::fs::read_to_string(&t.boot_config).unwrap(),
            GOOD_BOOT_CONFIG,
            "the pre-overlay boot config must be restored verbatim"
        );
        assert!(!t.paths.camera_probation.exists());
        let conf = parse_marker(&t.paths.camera_conf);
        assert_eq!(conf.get("camera_present").map(String::as_str), Some("false"));
        assert_eq!(conf.get("overlay_state").map(String::as_str), Some("reverted"));
    }

    #[test]
    fn a_truncated_snapshot_is_never_written_over_a_working_boot_config() {
        // Restoring an empty snapshot IS the unbootable board. The revert still
        // has to happen at the marker/conf level, but the boot config is left
        // alone and the outcome says so.
        let t = tree("");
        let marker = parse_marker(&t.paths.camera_probation);
        let live_before = std::fs::read_to_string(&t.boot_config).unwrap();

        let outcome = apply_decision(&t.paths, &marker, None).unwrap();
        assert_eq!(
            outcome,
            CameraProbeOutcome::Reverted {
                boot_config_restored: false
            }
        );
        assert_eq!(
            std::fs::read_to_string(&t.boot_config).unwrap(),
            live_before,
            "a truncated snapshot must not be restored"
        );
        assert!(std::fs::metadata(&t.snapshot).unwrap().len() < MIN_SNAPSHOT_BYTES as u64);
    }

    #[test]
    fn a_marker_with_no_snapshot_reverts_state_without_touching_boot() {
        let t = tree(GOOD_BOOT_CONFIG);
        std::fs::write(
            &t.paths.camera_probation,
            "camera_id=csi-imx214\nboard=cubie-a7z\nexpected_node=/dev/video0\nsensor=Sony IMX214\n",
        )
        .unwrap();
        let marker = parse_marker(&t.paths.camera_probation);
        let outcome = apply_decision(&t.paths, &marker, None).unwrap();
        assert_eq!(
            outcome,
            CameraProbeOutcome::Reverted {
                boot_config_restored: false
            }
        );
        assert!(!t.paths.camera_probation.exists());
    }

    #[test]
    fn sensor_tokens_drop_noise_words() {
        assert_eq!(sensor_tokens("Sony IMX214"), vec!["sony", "imx214"]);
        assert_eq!(sensor_tokens("OV5647"), vec!["ov5647"]);
        // Nothing long enough to match on → no sysfs match is attempted.
        assert!(sensor_tokens("a b").is_empty());
        assert!(sensor_tokens("").is_empty());
    }

    #[test]
    fn an_empty_sensor_declaration_never_matches_by_name() {
        let t = tree(GOOD_BOOT_CONFIG);
        let n = t.paths.sys_video4linux_dir.join("video0");
        std::fs::create_dir_all(&n).unwrap();
        std::fs::write(n.join("name"), b"anything at all\n").unwrap();
        assert!(sensor_node_by_name(&t.paths.sys_video4linux_dir, "").is_none());
    }

    #[test]
    fn run_reverts_end_to_end_when_the_bind_window_expires() {
        // The whole path an absent camera takes on the next boot: read the
        // marker, poll, find nothing, restore the boot config, clear probation.
        let t = tree(GOOD_BOOT_CONFIG);
        let outcome = run_with_window(&t.paths, std::time::Duration::ZERO).unwrap();
        assert_eq!(
            outcome,
            CameraProbeOutcome::Reverted {
                boot_config_restored: true
            }
        );
        assert_eq!(
            std::fs::read_to_string(&t.boot_config).unwrap(),
            GOOD_BOOT_CONFIG
        );
        assert!(!t.paths.camera_probation.exists());

        // And it is one-shot: the second boot is a clean no-op, so a reverted
        // board does not re-run the restore over a config the operator has
        // since changed.
        assert_eq!(
            run_with_window(&t.paths, std::time::Duration::ZERO).unwrap(),
            CameraProbeOutcome::NoProbation
        );
    }

    #[test]
    fn run_confirms_end_to_end_when_the_sensor_is_bound() {
        let t = tree(GOOD_BOOT_CONFIG);
        std::fs::write(t.paths.dev_dir.join("video0"), b"").unwrap();
        let outcome = run_with_window(&t.paths, std::time::Duration::ZERO).unwrap();
        assert_eq!(
            outcome,
            CameraProbeOutcome::Confirmed {
                evidence: "/dev/video0".to_string()
            }
        );
        assert!(std::fs::read_to_string(&t.boot_config)
            .unwrap()
            .contains("fdtoverlays"));
        assert!(!t.paths.camera_probation.exists());
    }
}
