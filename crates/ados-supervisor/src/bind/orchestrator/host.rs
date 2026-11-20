//! The host the bind orchestrator acts on: every file, process, interface and
//! regulatory call the bind sequence makes outside its own memory. Service
//! units go through the separate [`ProcessManager`](crate::process_manager::ProcessManager)
//! seam.
//!
//! [`SystemHost`] is the real host (the files under `/etc` and `/run/ados`,
//! `socat`, `wfb_keygen`, `ip`, `iw`, sysfs counters). The seam exists so the
//! state machine itself can be driven against a simulated host, two rigs in one
//! thread on a virtual clock, by the deterministic simulation tests.

use std::path::Path;
use std::time::{Duration, SystemTime};

use ados_protocol::logd::emitter::EventEmitter;
use ados_protocol::sidecar::write_atomic;
use async_trait::async_trait;

use super::InjectionPrepOutcome;
use crate::bind::keys::{self, FleetIdentity};
use crate::bind::{iface, socat, BindRole, UPSTREAM_DRONE_KEY, UPSTREAM_GS_KEY};

/// Why a socat run produced no exit status.
#[derive(Debug)]
pub enum SocatFailure {
    /// The process could not be started.
    Spawn(std::io::Error),
    /// Draining its output or waiting for it failed.
    Run(std::io::Error),
}

/// Everything the bind sequence does to the host besides service units.
#[async_trait]
pub trait BindHost: Send + Sync {
    /// Whether `path` is an existing regular file (artifacts, helper scripts,
    /// the deposited upstream key).
    fn file_present(&self, path: &str) -> bool;

    /// Whether `bin` resolves to an executable file on `$PATH`.
    fn on_path(&self, bin: &str) -> bool;

    /// Ground station: produce a fresh `/etc/gs.key` + `/etc/drone.key` pair,
    /// removing any leftovers first so a half-written pair never ships.
    async fn generate_keypair(&self) -> Result<(), String>;

    /// Reload the injection driver onto the configured region options while
    /// the normal radio unit is stopped. A no-op when already current.
    async fn reconcile_injection_driver(&self);

    /// Put the injection adapter(s) into verified monitor mode; one outcome per
    /// candidate.
    async fn prepare_injection_iface(&self) -> Vec<InjectionPrepOutcome>;

    /// Re-assert the configured global regulatory domain. Idempotent.
    async fn reconcile_reg_domain(&self, events: &EventEmitter);

    /// Wait until the bind tunnel interface has an address, up to `timeout`.
    async fn wait_for_iface(&self, iface: &str, timeout: Duration) -> bool;

    /// The bind tunnel's received-packet counter, `None` when it cannot be read.
    fn rx_packets(&self, iface: &str) -> Option<u64>;

    /// Kill bind socats left by an earlier session. Idempotent.
    async fn kill_stale_socats(&self);

    /// Run `socat <args>` to exit in its own process group; dropping the future
    /// kills the group. Returns the exit status (`-1` for a signal death) and
    /// stderr.
    async fn run_socat(&self, args: &[String]) -> Result<(i32, Vec<u8>), SocatFailure>;

    /// Wall-clock now, the reference the upstream key's mtime is judged against.
    fn wall_now(&self) -> SystemTime;

    /// Whether the file at `path` was written at or after `since`.
    fn file_fresh(&self, path: &str, since: SystemTime) -> bool;

    /// Read a whole file.
    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>>;

    /// Atomically write a key file, mode 0600.
    fn write_key(&self, path: &str, blob: &[u8]) -> Result<(), String>;

    /// The 16-hex fingerprint of the key file at `path`.
    fn read_public_fingerprint(&self, path: &str) -> Result<String, String>;

    /// Persist the pair fields into the agent config (see
    /// [`keys::persist_pair_state`]). False when nothing was written.
    fn persist_pair_state(
        &self,
        role: BindRole,
        peer_device_id: Option<&str>,
        paired_at: Option<&str>,
        auto_pair_enabled: Option<bool>,
        fleet: Option<FleetIdentity>,
    ) -> bool;

    /// Persist only the fleet addressing (see [`keys::persist_fleet`]). False
    /// when nothing was written.
    fn persist_fleet(&self, fleet: FleetIdentity) -> bool;

    /// Write the setup-complete sentinel.
    fn write_setup_complete(&self, body: &[u8]) -> Result<(), String>;

    /// Atomically replace the cross-process bind-state sentinel.
    fn write_sentinel(&self, body: &[u8]);

    /// The bind-state sentinel as last written, `None` when absent.
    fn read_sentinel(&self) -> Option<Vec<u8>>;
}

/// The real host.
pub struct SystemHost;

#[async_trait]
impl BindHost for SystemHost {
    fn file_present(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }

    fn on_path(&self, bin: &str) -> bool {
        crate::bind::on_path(bin)
    }

    async fn generate_keypair(&self) -> Result<(), String> {
        if !crate::bind::on_path("wfb_keygen") {
            return Err(
                "wfb_keygen binary not found on PATH. install.sh provisions \
                 wfb-ng with the keygen tool; reinstall the agent."
                    .to_string(),
            );
        }
        for path in [UPSTREAM_GS_KEY, UPSTREAM_DRONE_KEY] {
            let _ = std::fs::remove_file(path);
        }
        let out = tokio::process::Command::new("wfb_keygen")
            .current_dir("/etc")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| format!("wfb_keygen spawn failed: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "wfb_keygen failed (rc={:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        if !Path::new(UPSTREAM_GS_KEY).is_file() || !Path::new(UPSTREAM_DRONE_KEY).is_file() {
            return Err(format!(
                "wfb_keygen exited 0 but did not produce {UPSTREAM_GS_KEY} + {UPSTREAM_DRONE_KEY}"
            ));
        }
        Ok(())
    }

    async fn reconcile_injection_driver(&self) {
        tokio::task::spawn_blocking(|| crate::rtl_modprobe::reconcile_live_driver(true))
            .await
            .ok();
    }

    async fn prepare_injection_iface(&self) -> Vec<InjectionPrepOutcome> {
        super::prep::prepare_injection_iface_for_bind().await
    }

    async fn reconcile_reg_domain(&self, events: &EventEmitter) {
        crate::reg_reconciler::reconcile_global_domain(events).await;
    }

    async fn wait_for_iface(&self, iface: &str, timeout: Duration) -> bool {
        iface::wait_for_iface(iface, timeout).await
    }

    fn rx_packets(&self, iface: &str) -> Option<u64> {
        iface::read_rx_packets_counter(iface)
    }

    async fn kill_stale_socats(&self) {
        socat::kill_stale_bind_socats().await;
    }

    async fn run_socat(&self, args: &[String]) -> Result<(i32, Vec<u8>), SocatFailure> {
        let mut proc = socat::SocatProcess::spawn(args).map_err(SocatFailure::Spawn)?;
        let (rc, _stdout, stderr) = proc.run().await.map_err(SocatFailure::Run)?;
        Ok((rc, stderr))
    }

    fn wall_now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn file_fresh(&self, path: &str, since: SystemTime) -> bool {
        super::peer::upstream_key_fresh(Path::new(path), since)
    }

    fn read_file(&self, path: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn write_key(&self, path: &str, blob: &[u8]) -> Result<(), String> {
        write_atomic(Path::new(path), blob, 0o600).map_err(|e| e.to_string())
    }

    fn read_public_fingerprint(&self, path: &str) -> Result<String, String> {
        keys::read_public_fingerprint(Path::new(path))
    }

    fn persist_pair_state(
        &self,
        role: BindRole,
        peer_device_id: Option<&str>,
        paired_at: Option<&str>,
        auto_pair_enabled: Option<bool>,
        fleet: Option<FleetIdentity>,
    ) -> bool {
        keys::persist_pair_state(
            Path::new(crate::bind::CONFIG_YAML),
            Path::new(crate::bind::CONFIG_LOCK_PATH),
            role,
            peer_device_id,
            paired_at,
            auto_pair_enabled,
            fleet,
        )
    }

    fn persist_fleet(&self, fleet: FleetIdentity) -> bool {
        keys::persist_fleet(
            Path::new(crate::bind::CONFIG_YAML),
            Path::new(crate::bind::CONFIG_LOCK_PATH),
            fleet,
        )
    }

    fn write_setup_complete(&self, body: &[u8]) -> Result<(), String> {
        write_atomic(Path::new(crate::bind::SETUP_COMPLETE_PATH), body, 0o644)
            .map_err(|e| e.to_string())
    }

    fn write_sentinel(&self, body: &[u8]) {
        let sentinel = Path::new(crate::bind::BIND_STATE_SENTINEL);
        if let Err(e) = write_atomic(sentinel, body, 0o644) {
            tracing::debug!(error = %e, "bind_sentinel_write_failed");
        }
    }

    fn read_sentinel(&self) -> Option<Vec<u8>> {
        std::fs::read(crate::bind::BIND_STATE_SENTINEL).ok()
    }
}
