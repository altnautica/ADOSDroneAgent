use super::*;

#[test]
fn rotation_lands_well_inside_the_token_ttl() {
    // The whole point of the proactive rotation: a token must be replaced
    // with real life left on it, so a missed tick on a busy box does not
    // produce the expiry this loop exists to prevent.
    assert!(
        (ROTATE_INTERVAL.as_secs() as i64) * 2 <= TOKEN_TTL_SECONDS,
        "rotation interval {}s does not leave slack inside the {TOKEN_TTL_SECONDS}s TTL",
        ROTATE_INTERVAL.as_secs()
    );
}

#[test]
fn the_state_poll_is_a_fixed_short_interval() {
    // A recovery loop that backs off turns a transient miss into a plugin
    // inert for minutes, so this is pinned rather than left to taste.
    assert!(POLL_INTERVAL >= Duration::from_secs(2));
    assert!(POLL_INTERVAL <= Duration::from_secs(5));
}

struct RecordingUnits(Mutex<Vec<String>>);

impl UnitControl for RecordingUnits {
    fn refresh_units(&self) {}
    fn stop_plugin_units(&self, plugin_id: &str) {
        if let Ok(mut stopped) = self.0.lock() {
            stopped.push(plugin_id.to_string());
        }
    }
}

#[tokio::test]
async fn tampered_binary_stops_plugin() {
    use sha2::{Digest, Sha256};
    const ID: &str = "com.example.tamper";
    let dir = tempfile::tempdir().unwrap();
    let socket_dir = dir.path().join("sockets");
    let state_path = dir.path().join("state/plugin-state.json");
    let install_dir = dir.path().join("plugins");
    std::fs::create_dir_all(&socket_dir).unwrap();
    let plugin_dir = install_dir.join(ID);
    std::fs::create_dir_all(plugin_dir.join("bin")).unwrap();
    let manifest = format!(
        "id: {ID}\nversion: 1.0.0\nrisk: medium\ncompatibility:\n  ados_version: \
         \">=0.1.0,<99.0.0\"\nagent:\n  entrypoint: plugin:Tamper\n  isolation: subprocess\n"
    );
    std::fs::write(plugin_dir.join("manifest.yaml"), &manifest).unwrap();
    std::fs::write(plugin_dir.join("bin/tool"), b"original").unwrap();
    let hex = |b: &[u8]| hex::encode(Sha256::digest(b));
    let digests: BTreeMap<String, String> = [
        ("bin/tool".to_string(), hex(b"original")),
        ("manifest.yaml".to_string(), hex(manifest.as_bytes())),
    ]
    .into();
    crate::attestation::Attestation::new(None, &digests, &[])
        .write_to(&plugin_dir)
        .unwrap();
    let install = state::PluginInstall {
        plugin_id: ID.to_string(),
        version: "1.0.0".to_string(),
        source: state::PluginSource::LocalFile,
        source_uri: None,
        signer_id: None,
        manifest_hash: hex(manifest.as_bytes()),
        status: PluginStatus::Running,
        installed_at: 1,
        enabled_at: Some(1),
        failure_reason: None,
        permissions: BTreeMap::new(),
        auto_update: false,
        pinned_version: None,
        last_update_check_at: None,
        last_update_attempt: None,
        reenable_pending: false,
        model_status: None,
        service_status: None,
        revocation_pending: false,
        archive_sha256: None,
    };
    state::save_state(&[install], Some(&state_path)).unwrap();

    let issuer = Arc::new(crate::shared_issuer(&dir.path().join("secrets/token-secret")).unwrap());
    let mint = Arc::new(TokenMint::new(
        issuer.clone(),
        state_path.clone(),
        socket_dir.clone(),
        dir.path().join("plugin-data"),
        String::new(),
    ));
    let server = Arc::new(
        PluginIpcServer::new(
            &socket_dir,
            issuer,
            Arc::new(crate::EventBus::new()),
            Arc::new(crate::NoopHost),
        )
        .with_token_mint(mint.clone()),
    );
    let units = Arc::new(RecordingUnits(Mutex::new(Vec::new())));
    let reconciler = PluginReconciler::new(server, mint, state_path.clone(), install_dir)
        .with_unit_control(
            units.clone(),
            dir.path().join("state.sock"),
            dir.path().into(),
        );

    assert_eq!(reconciler.reconcile().serving, 1);
    // Same size, different bytes: the digest decides, not the size.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(plugin_dir.join("bin/tool"), b"modified").unwrap();
    let report = reconciler.reconcile();
    assert_eq!(report.stopped, 1);
    assert_eq!(report.serving, 0);
    let installs = state::load_state_checked(Some(&state_path)).unwrap();
    assert_eq!(installs[0].status, PluginStatus::Failed);
    assert!(installs[0]
        .failure_reason
        .as_deref()
        .is_some_and(|r| r.starts_with(TAMPERED_REASON)));
    assert_eq!(units.0.lock().map(|s| s.clone()).unwrap_or_default(), [ID]);
    // A failed plugin is not wanted, so the next pass changes nothing.
    assert_eq!(reconciler.reconcile(), ReconcileReport::default());
}
