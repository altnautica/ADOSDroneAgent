use super::*;
use crate::backend::RecordingBackend;
use crate::supervisor::Paths;
use std::io::Write;

fn paths_in(dir: &Path) -> Paths {
    Paths {
        install_dir: dir.join("plugins"),
        unit_dir: dir.join("units"),
        state_path: dir.join("state/plugin-state.json"),
        log_dir: dir.join("logs"),
        control_dir: dir.join("plugin-host"),
        loopback_guard_state: dir.join("plugin-loopback-guard.json"),
        socket_dir: dir.join("sockets"),
        token_secret: dir.join("secrets/plugin-token-secret"),
        runner: dir.join("bin/ados-plugin-runner"),
        run_dir: dir.join("run"),
        data_root: dir.join("plugin-data"),
        device_id_file: dir.join("device-id"),
    }
}

fn manifest(version: &str, perms: &[&str]) -> String {
    let mut m = format!(
        "id: com.example.thermal\nversion: {version}\nrisk: high\ncompatibility:\n  \
         ados_version: \">=0.1.0,<2.0.0\"\nagent:\n  entrypoint: agent/py/x.py\n  permissions:\n"
    );
    for p in perms {
        m.push_str(&format!("    - {p}\n"));
    }
    m
}

fn archive(manifest_yaml: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        w.start_file("manifest.yaml", opts).unwrap();
        w.write_all(manifest_yaml.as_bytes()).unwrap();
        w.start_file("agent/py/x.py", opts).unwrap();
        w.write_all(b"print('hi')").unwrap();
        w.finish().unwrap();
    }
    buf
}

struct FakeSource {
    row: VersionRow,
    archive: Vec<u8>,
    notices: Mutex<Vec<Value>>,
    queried: Mutex<u32>,
    downloads: Mutex<u32>,
    /// How long each download takes (a slow mirror).
    delay: Duration,
}

/// A vehicle that reads disarmed.
struct Disarmed;

impl ArmedProbe for Disarmed {
    fn treated_as_armed(&self) -> bool {
        false
    }
}

/// An armed verdict the test flips.
struct Toggle(std::sync::atomic::AtomicBool);

impl ArmedProbe for Toggle {
    fn treated_as_armed(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl UpdateSource for FakeSource {
    fn ready(&self) -> bool {
        true
    }
    fn latest(&self, _: &str) -> Result<Option<VersionRow>, String> {
        *self.queried.lock().unwrap() += 1;
        Ok(Some(self.row.clone()))
    }
    fn download(&self, _: &str, _: &str) -> Result<Vec<u8>, String> {
        *self.downloads.lock().unwrap() += 1;
        std::thread::sleep(self.delay);
        Ok(self.archive.clone())
    }
    fn notify(&self, notice: &Value) {
        self.notices.lock().unwrap().push(notice.clone());
    }
}

fn source(version: &str, perms: &[&str]) -> FakeSource {
    FakeSource {
        row: VersionRow {
            version: version.to_string(),
            download_url: "https://example.com/p.adosplug".to_string(),
            archive_sha256: "ab".repeat(32),
            signer_key_id: String::new(),
        },
        archive: archive(&manifest(version, perms)),
        notices: Mutex::new(Vec::new()),
        queried: Mutex::new(0),
        downloads: Mutex::new(0),
        delay: Duration::ZERO,
    }
}

/// A backend whose `enable_start` fails while `failing` is set.
struct FlakyBackend {
    inner: RecordingBackend,
    failing: std::sync::atomic::AtomicBool,
}

impl crate::backend::ServiceBackend for FlakyBackend {
    fn install(
        &self,
        name: &str,
        spec: &crate::backend::UnitSpec,
    ) -> Result<bool, crate::errors::SupervisorError> {
        self.inner.install(name, spec)
    }
    fn uninstall(&self, name: &str) -> Result<(), crate::errors::SupervisorError> {
        self.inner.uninstall(name)
    }
    fn enable_start(&self, name: &str) -> Result<(), crate::errors::SupervisorError> {
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(crate::errors::SupervisorError(
                "unit failed to start".into(),
            ));
        }
        self.inner.enable_start(name)
    }
    fn stop_disable(&self, name: &str) -> Result<(), crate::errors::SupervisorError> {
        self.inner.stop_disable(name)
    }
    fn restart(&self, name: &str) -> Result<(), crate::errors::SupervisorError> {
        self.inner.restart(name)
    }
    fn is_active(&self, name: &str) -> Result<bool, crate::errors::SupervisorError> {
        self.inner.is_active(name)
    }
    fn probe(
        &self,
        probe: &crate::backend::ProbeSpec,
    ) -> Result<(), crate::errors::SupervisorError> {
        self.inner.probe(probe)
    }
    fn enforces_sandbox(&self) -> bool {
        self.inner.enforces_sandbox()
    }
}

/// A throwaway Ed25519 key enrolled as `signer` in `keys_dir`.
fn enrol(keys_dir: &Path, signer: &str) -> ed25519_dalek::SigningKey {
    use ed25519_dalek::pkcs8::{spki::der::pem::LineEnding, EncodePublicKey};
    use sha2::Digest;
    let key =
        ed25519_dalek::SigningKey::from_bytes(&sha2::Sha256::digest(signer.as_bytes()).into());
    std::fs::create_dir_all(keys_dir).unwrap();
    let pem = key
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .unwrap();
    std::fs::write(keys_dir.join(format!("{signer}.pem")), pem).unwrap();
    key
}

/// `archive` with a `SIGNATURE` entry by `signer` over its payload hash.
fn signed(archive: Vec<u8>, signer: &str, key: &ed25519_dalek::SigningKey) -> Vec<u8> {
    use base64::Engine;
    use ed25519_dalek::Signer;
    let hash = parse_archive_bytes(archive.clone()).unwrap().payload_hash;
    let signature = base64::engine::general_purpose::STANDARD.encode(key.sign(&hash).to_bytes());
    let mut w = zip::ZipWriter::new_append(std::io::Cursor::new(archive)).unwrap();
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    w.start_file("SIGNATURE", opts).unwrap();
    w.write_all(format!("{signer}\n{signature}\n").as_bytes())
        .unwrap();
    w.finish().unwrap().into_inner()
}

fn running_v1(dir: &Path) -> PluginSupervisor {
    let mut sup = PluginSupervisor::new(paths_in(dir), false, None, "1.0.0")
        .with_backend(Arc::new(RecordingBackend::default()));
    let contents =
        crate::archive::parse_archive_bytes(archive(&manifest("1.0.0", &["hardware.spi"])))
            .unwrap();
    sup.install_contents(contents, Path::new("/tmp/v1.adosplug"))
        .unwrap();
    sup.grant_permission("com.example.thermal", "hardware.spi")
        .unwrap();
    sup.enable("com.example.thermal").unwrap();
    sup
}

#[test]
fn a_registry_row_from_another_signer_notifies_without_downloading() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = running_v1(dir.path());
    let mut src = source("1.1.0", &["hardware.spi"]);
    src.row.signer_key_id = "someone-else".to_string();
    assert_eq!(
        run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
        Outcome::Notify
    );
    let n = &src.notices.lock().unwrap()[0];
    assert_eq!(n["reason"], "signer_change");
    assert_eq!(n["offered_signer"], "someone-else");
    assert_eq!(*src.downloads.lock().unwrap(), 0);
    let rec = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(rec.version, "1.0.0");
    assert_eq!(rec.status, PluginStatus::Running);
}

#[test]
fn an_archive_whose_verified_signer_differs_is_refused_before_the_install_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    let alice = enrol(&keys, "alice");
    let mallory = enrol(&keys, "mallory");
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "1.0.0")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_trusted_keys_dir(&keys);
    let v1 = signed(
        archive(&manifest("1.0.0", &["hardware.spi"])),
        "alice",
        &alice,
    );
    sup.install_contents(
        parse_archive_bytes(v1).unwrap(),
        Path::new("/tmp/v1.adosplug"),
    )
    .unwrap();
    sup.grant_permission("com.example.thermal", "hardware.spi")
        .unwrap();
    sup.enable("com.example.thermal").unwrap();

    // The row claims the installed key, but the archive it serves is
    // signed by another enrolled key, or not signed at all.
    let v11 = archive(&manifest("1.1.0", &["hardware.spi"]));
    for (served, offered) in [
        (signed(v11.clone(), "mallory", &mallory), json!("mallory")),
        (v11.clone(), Value::Null),
    ] {
        let mut src = source("1.1.0", &["hardware.spi"]);
        src.row.signer_key_id = "alice".to_string();
        src.archive = served;
        assert_eq!(
            run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
            Outcome::Notify
        );
        let n = &src.notices.lock().unwrap()[0];
        assert_eq!(n["reason"], "signer_change");
        assert_eq!(n["current_signer"], "alice");
        assert_eq!(n["offered_signer"], offered);
        let rec = sup.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.0.0");
        assert_eq!(rec.status, PluginStatus::Running);
    }

    // The same key's build still updates silently.
    let mut src = source("1.1.0", &["hardware.spi"]);
    src.row.signer_key_id = "alice".to_string();
    src.archive = signed(v11, "alice", &alice);
    assert_eq!(
        run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
        Outcome::SilentInstall
    );
    let rec = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(rec.version, "1.1.0");
    assert_eq!(rec.signer_id.as_deref(), Some("alice"));
}

#[test]
fn a_minor_bump_with_the_same_permissions_installs_and_keeps_the_grants() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = running_v1(dir.path());
    let src = source("1.1.0", &["hardware.spi"]);
    let results = run_cycle(&mut sup, &src, None, &Disarmed);
    assert_eq!(
        results,
        vec![("com.example.thermal".to_string(), Outcome::SilentInstall)]
    );
    let rec = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(rec.version, "1.1.0");
    assert_eq!(rec.status, PluginStatus::Running);
    assert!(state::granted_caps(rec).contains("hardware.spi"));
    assert_eq!(
        rec.last_update_attempt.as_ref().unwrap()["outcome"],
        "success"
    );
    assert!(rec.last_update_check_at.is_some());
}

#[test]
fn a_major_bump_or_a_permission_change_only_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = running_v1(dir.path());
    let major = source("2.0.0", &["hardware.spi"]);
    assert_eq!(
        run_cycle(&mut sup, &major, None, &Disarmed)[0].1,
        Outcome::Notify
    );
    assert_eq!(major.notices.lock().unwrap()[0]["reason"], "major_bump");

    let widened = source("1.2.0", &["hardware.spi", "mavlink.write"]);
    assert_eq!(
        run_cycle(&mut sup, &widened, None, &Disarmed)[0].1,
        Outcome::Notify
    );
    let n = &widened.notices.lock().unwrap()[0];
    assert_eq!(n["reason"], "permission_delta");
    assert_eq!(n["new_permissions"], json!(["mavlink.write"]));
    assert_eq!(
        sup.find_install("com.example.thermal").unwrap().version,
        "1.0.0"
    );
}

#[test]
fn every_check_reads_the_archive_that_would_be_installed() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = running_v1(dir.path());

    // The archive is another plugin: refused, nothing touched.
    let mut other = source("1.1.0", &["hardware.spi"]);
    other.archive = archive(
        &manifest("1.1.0", &["hardware.spi"]).replace("com.example.thermal", "com.example.other"),
    );
    assert_eq!(
        run_cycle(&mut sup, &other, None, &Disarmed)[0].1,
        Outcome::Failed
    );

    // The archive fetches a payload binary the installed version did not,
    // under the same permission set: only the code-surface check sees it.
    let fetching = manifest("1.1.0", &["hardware.spi"])
        + "  payloads:\n    - path: bin/tool\n      \
           source: https://github.com/o/r/releases/download/v1/tool\n      \
           sha256: "
        + &"ab".repeat(32)
        + "\n      size_bytes: 10\n";
    let mut widened = source("1.1.0", &["hardware.spi"]);
    widened.archive = archive(&fetching);
    assert_eq!(
        run_cycle(&mut sup, &widened, None, &Disarmed)[0].1,
        Outcome::Notify
    );
    assert_eq!(
        widened.notices.lock().unwrap()[0]["reason"],
        "code_surface_change"
    );

    // A new risk label alone is a code-surface change too.
    let mut riskier = source("1.1.0", &["hardware.spi"]);
    riskier.archive =
        archive(&manifest("1.1.0", &["hardware.spi"]).replace("risk: high", "risk: critical"));
    assert_eq!(
        run_cycle(&mut sup, &riskier, None, &Disarmed)[0].1,
        Outcome::Notify
    );
    assert_eq!(
        riskier.notices.lock().unwrap()[0]["reason"],
        "code_surface_change"
    );

    // A row with no archive pin is refused before any download.
    let mut unpinned = source("1.1.0", &["hardware.spi"]);
    unpinned.row.archive_sha256 = String::new();
    assert_eq!(
        run_cycle(&mut sup, &unpinned, None, &Disarmed)[0].1,
        Outcome::Failed
    );
    assert_eq!(*unpinned.downloads.lock().unwrap(), 0);

    let rec = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(rec.version, "1.0.0");
    assert_eq!(rec.status, PluginStatus::Running);
}

#[test]
fn a_plugin_an_update_left_stopped_is_started_again_once_it_can() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(FlakyBackend {
        inner: RecordingBackend::default(),
        failing: std::sync::atomic::AtomicBool::new(false),
    });
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "1.0.0")
        .with_backend(backend.clone());
    let v1 = parse_archive_bytes(archive(&manifest("1.0.0", &["hardware.spi"]))).unwrap();
    sup.install_contents(v1, Path::new("/tmp/v1.adosplug"))
        .unwrap();
    sup.enable("com.example.thermal").unwrap();

    // The new version installs but its unit will not start.
    backend
        .failing
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let src = source("1.1.0", &["hardware.spi"]);
    assert_eq!(
        run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
        Outcome::Failed
    );
    let rec = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(rec.version, "1.1.0");
    assert!(rec.reenable_pending);
    // The daily cycle skips a stopped plugin, but the retry does not.
    retry_pending_enables(&mut sup);
    assert!(
        sup.find_install("com.example.thermal")
            .unwrap()
            .reenable_pending
    );

    backend
        .failing
        .store(false, std::sync::atomic::Ordering::SeqCst);
    retry_pending_enables(&mut sup);
    let rec = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(rec.status, PluginStatus::Running);
    assert!(!rec.reenable_pending);
}

#[test]
fn an_operator_disable_cancels_the_pending_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = running_v1(dir.path());
    sup.disable("com.example.thermal").unwrap();
    sup.note_reenable_pending("com.example.thermal").unwrap();
    sup.disable("com.example.thermal").unwrap();
    retry_pending_enables(&mut sup);
    let rec = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(rec.status, PluginStatus::Disabled);
    assert!(!rec.reenable_pending);
}

#[test]
fn a_pinned_plugin_is_never_queried() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = running_v1(dir.path());
    let mut install = sup.find_install("com.example.thermal").unwrap().clone();
    install.pinned_version = Some("1.0.0".to_string());
    let src = source("1.1.0", &["hardware.spi"]);
    assert_eq!(check_one(&mut sup, &install, &src, None), Outcome::Skipped);
    assert_eq!(*src.queried.lock().unwrap(), 0);
}

#[test]
fn a_refused_archive_rolls_back_to_the_running_old_version() {
    let dir = tempfile::tempdir().unwrap();
    let mut sup = running_v1(dir.path());
    let mut src = source("1.1.0", &["hardware.spi"]);
    src.archive = b"not a zip".to_vec();
    assert_eq!(
        run_cycle(&mut sup, &src, None, &Disarmed)[0].1,
        Outcome::Failed
    );
    let rec = sup.find_install("com.example.thermal").unwrap();
    assert_eq!(rec.version, "1.0.0");
    assert_eq!(rec.status, PluginStatus::Running);
    assert_eq!(
        rec.last_update_attempt.as_ref().unwrap()["outcome"],
        "failed"
    );
}

#[test]
fn the_registry_payload_yields_its_newest_row() {
    let payload = json!({"plugin": {}, "versions": [
        {"version": "1.2.0", "manifest_yaml": "x", "supported_boards": ["rpi4"],
         "download_url": "https://example.com/a", "archive_sha256": "ab"},
        {"version": "1.1.0"}]});
    let row = latest_version_row(&payload).unwrap().unwrap();
    assert_eq!(row.version, "1.2.0");
    assert_eq!(row.archive_sha256, "ab");
    assert!(latest_version_row(&json!({"error": "nope"})).is_err());
    assert_eq!(
        latest_version_row(&json!({"plugin": {}, "versions": []})).unwrap(),
        None
    );
}

#[test]
fn auto_update_defers_while_armed_and_runs_after_disarm() {
    let dir = tempfile::tempdir().unwrap();
    let sup = Mutex::new(running_v1(dir.path()));
    let src = source("1.1.0", &["hardware.spi"]);
    let probe = Toggle(std::sync::atomic::AtomicBool::new(true));

    // Armed: no query, no download, nothing stopped.
    assert!(run_cycle_shared(&sup, &src, None, &probe).is_none());
    assert!(run_cycle(&mut sup.lock().unwrap(), &src, None, &probe).is_empty());
    assert_eq!(*src.queried.lock().unwrap(), 0);
    assert_eq!(*src.downloads.lock().unwrap(), 0);
    {
        let guard = sup.lock().unwrap();
        let rec = guard.find_install("com.example.thermal").unwrap();
        assert_eq!(rec.version, "1.0.0");
        assert_eq!(rec.status, PluginStatus::Running);
    }

    // Disarmed: the deferred update runs.
    probe.0.store(false, std::sync::atomic::Ordering::SeqCst);
    let results = run_cycle_shared(&sup, &src, None, &probe).unwrap();
    assert_eq!(
        results,
        vec![("com.example.thermal".to_string(), Outcome::SilentInstall)]
    );
    let guard = sup.lock().unwrap();
    assert_eq!(
        guard.find_install("com.example.thermal").unwrap().version,
        "1.1.0"
    );
}

#[test]
fn lock_free_during_fetch() {
    let dir = tempfile::tempdir().unwrap();
    let sup = Arc::new(Mutex::new(running_v1(dir.path())));
    let mut src = source("1.1.0", &["hardware.spi"]);
    src.delay = Duration::from_millis(800);
    let src = Arc::new(src);
    let (cycle_sup, cycle_src) = (Arc::clone(&sup), Arc::clone(&src));
    let cycle = std::thread::spawn(move || {
        run_cycle_shared(&cycle_sup, cycle_src.as_ref(), None, &Disarmed)
    });
    // Wait until the download is under way.
    let started = std::time::Instant::now();
    while *src.downloads.lock().unwrap() == 0 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "download never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    // Another lifecycle caller gets the supervisor within 100 ms.
    let asked = std::time::Instant::now();
    let guard = sup.lock().unwrap();
    assert!(
        asked.elapsed() < Duration::from_millis(100),
        "lock took {:?}",
        asked.elapsed()
    );
    drop(guard);
    let results = cycle.join().unwrap().unwrap();
    assert_eq!(results[0].1, Outcome::SilentInstall);
}
