//! Plugin accounts and the data directory the runner is pointed at.

use super::*;

/// Account calls recorded instead of touching the account database.
#[derive(Default)]
struct RecordingAccounts {
    calls: parking_lot::Mutex<Vec<String>>,
    handed: parking_lot::Mutex<Vec<PathBuf>>,
}

impl crate::plugin_account::PluginAccounts for RecordingAccounts {
    fn ensure(&self, plugin_id: &str) -> Result<(), SupervisorError> {
        self.calls.lock().push(format!("ensure {plugin_id}"));
        Ok(())
    }
    fn remove(&self, plugin_id: &str) -> Result<(), SupervisorError> {
        self.calls.lock().push(format!("remove {plugin_id}"));
        Ok(())
    }
    fn hand_over(&self, dir: &Path, _plugin_id: &str) -> Result<(), SupervisorError> {
        self.handed.lock().push(dir.to_path_buf());
        Ok(())
    }
}

fn install_thermal(sup: &mut PluginSupervisor) {
    sup.install_contents(
        parse_archive_bytes(build_unsigned_archive(SUBPROC_MANIFEST)).unwrap(),
        Path::new("/tmp/x.adosplug"),
    )
    .unwrap();
}

#[test]
fn install_makes_the_data_dir_the_runner_is_pointed_at_and_remove_honours_keep_data() {
    use std::os::unix::fs::PermissionsExt;
    let id = "com.example.thermal";
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("device-id"), "0a1b2c3d4e5f\n").unwrap();
    let accounts = Arc::new(RecordingAccounts::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_accounts(accounts.clone());
    install_thermal(&mut sup);
    assert_eq!(*accounts.calls.lock(), ["ensure com.example.thermal"]);
    let handed = &accounts.handed;

    let base = dir.path().join("plugin-data").join(id);
    let drones = base.join("drones");
    let leaf = drones.join("0a1b2c3d4e5f");
    for d in [&base, &drones, &leaf] {
        let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", d.display());
    }
    // Deepest first: the chain stays the host's until its base is handed over.
    assert_eq!(*handed.lock(), [leaf.clone(), drones, base.clone()]);

    // The dir the runner's environment names is the one that exists.
    sup.enable(id).unwrap();
    let env = std::fs::read_to_string(token_credential_path(id, Some(&dir.path().join("sockets"))))
        .unwrap();
    assert!(
        env.contains(&format!("ADOS_PLUGIN_DATA_DIR={}\n", leaf.display())),
        "{env}"
    );

    std::fs::write(leaf.join("world.sqlite"), b"x").unwrap();
    sup.remove(id, true).unwrap();
    assert!(leaf.join("world.sqlite").exists());
    // Data kept: so is the user that owns it.
    assert!(!accounts
        .calls
        .lock()
        .iter()
        .any(|c| c.starts_with("remove")));
    install_thermal(&mut sup);
    sup.remove(id, false).unwrap();
    assert!(!base.exists());
    assert_eq!(
        accounts.calls.lock().last().map(String::as_str),
        Some("remove com.example.thermal")
    );
}

/// An existing data dir is the plugin account's: a later pass hands it
/// over again but creates and chowns nothing under it, so a link the
/// plugin planted there never aims the host's root writes elsewhere.
#[test]
fn an_existing_data_dir_is_handed_over_again_but_never_entered() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("device-id"), "0a1b2c3d4e5f\n").unwrap();
    let accounts = Arc::new(RecordingAccounts::default());
    let mut sup = PluginSupervisor::new(paths_in(dir.path()), false, None, "0.48.11")
        .with_backend(Arc::new(RecordingBackend::default()))
        .with_accounts(accounts.clone());
    let handed = &accounts.handed;
    install_thermal(&mut sup);
    let base = dir.path().join("plugin-data/com.example.thermal");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::remove_dir_all(base.join("drones")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, base.join("drones")).unwrap();
    handed.lock().clear();

    sup.enable("com.example.thermal").unwrap();
    assert_eq!(*handed.lock(), [base]);
    assert!(std::fs::read_dir(&elsewhere).unwrap().next().is_none());
}

/// A unit binds its socket dir and loads its token credential when it
/// starts, so both exist, with its data dir, before the backend starts it:
/// on a first enable and on a re-enable after the host dropped the socket
/// and token of the disabled plugin. The token is one the host verifies.
#[test]
fn enable_starts_the_unit_only_once_what_it_binds_and_reads_exists() {
    let id = "com.example.thermal";
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    let socket_dir = plugin_socket_dir(&paths.socket_dir, id);
    let env = token_credential_path(id, Some(&paths.socket_dir));
    let data = paths.data_root.join(id);
    let secret = paths.token_secret.clone();
    let at_start = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let rec = {
        let (at_start, socket_dir, env) = (at_start.clone(), socket_dir.clone(), env.clone());
        RecordingBackend::default().on_enable_start(move |name| {
            if name == unit_name_for(id) {
                at_start
                    .lock()
                    .push((socket_dir.is_dir(), env.is_file(), data.is_dir()));
            }
        })
    };
    let mut sup = PluginSupervisor::new(paths, false, None, "0.48.11").with_backend(Arc::new(rec));
    install_thermal(&mut sup);

    sup.enable(id).unwrap();
    sup.disable(id).unwrap();
    std::fs::remove_file(&env).unwrap();
    std::fs::remove_dir_all(&socket_dir).unwrap();
    sup.enable(id).unwrap();
    assert_eq!(*at_start.lock(), [(true, true, true); 2]);

    let body = std::fs::read_to_string(&env).unwrap();
    let token = body
        .lines()
        .find_map(|l| l.strip_prefix("ADOS_PLUGIN_TOKEN="))
        .unwrap();
    let token = ados_protocol::plugin::CapabilityToken::from_token_string(token).unwrap();
    assert_eq!(token.plugin_id, id);
    assert!(shared_issuer(&secret)
        .unwrap()
        .verify(&token, token.issued_at + 1)
        .is_ok());
}
