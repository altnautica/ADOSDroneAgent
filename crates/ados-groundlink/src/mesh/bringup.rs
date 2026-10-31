//! Mesh bring-up for the relay / receiver roles (`ados-groundlink
//! --mesh-bringup`, the `ados-batman` unit).
//!
//! Joins the mesh carrier with authenticated keying through wpa_supplicant,
//! binds it to `bat0`, and drives the batman-adv gateway mode from the role,
//! `cloud_uplink` and the live uplink flag. A fixed-cadence supervision pass
//! then re-joins and re-binds when the interface, its wpa_supplicant, the join
//! or the batman membership is lost, follows the resolver to a reassigned
//! adapter, and re-evaluates the gateway mode. Neighbour / gateway state is
//! published by the role loops' poll (`mesh-state.json`), not here.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ados_proc::{GroupChild, Spec, StderrMode, StdoutMode};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::gateway::{
    apply_gateway_mode, apply_persisted_gateway_preference, gateway_mode_for, modprobe_batman,
};
use super::identity::{ensure_mesh_identity_at, MeshIdentityError};
use super::secure_join::{
    bat_member, channel_freq_mhz, iface_joined, iw_set_type, mesh_passphrase, wpa_conf_path,
    wpa_supplicant_conf, write_private,
};
use super::{batctl, iface};
use crate::gs_config::GroundStationConfig;

/// Supervision cadence. Fixed: the loop never backs off and never gives up.
pub const SUPERVISE_INTERVAL: Duration = Duration::from_secs(3);
const JOIN_VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
const JOIN_VERIFY_POLL: Duration = Duration::from_millis(500);
const WPA_STOP_GRACE: Duration = Duration::from_secs(5);
const CMD_TIMEOUT: Duration = Duration::from_secs(5);
const ROLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the bring-up reads and writes.
#[derive(Debug, Clone)]
pub struct BringupPaths {
    pub run_dir: PathBuf,
    pub sys_net: PathBuf,
    pub mesh_id_path: PathBuf,
    pub gateway_pref: PathBuf,
}

impl BringupPaths {
    pub fn live() -> Self {
        Self {
            run_dir: ados_protocol::paths::run_dir(),
            sys_net: PathBuf::from("/sys/class/net"),
            mesh_id_path: PathBuf::from(crate::paths::MESH_ID_PATH),
            gateway_pref: PathBuf::from(crate::paths::MESH_GATEWAY_JSON),
        }
    }

    fn wpa_dir(&self) -> PathBuf {
        self.run_dir.join("mesh")
    }

    fn uplink_flag(&self) -> PathBuf {
        self.run_dir.join("uplink-active")
    }
}

/// Why setup did not bring the mesh up.
#[derive(Debug)]
pub enum BringupError {
    /// A relay has no delivered mesh identity: downgrade, do not crash-loop.
    IdentityMissing(String),
    Fatal(String),
}

/// The bring-up state of one node.
pub struct MeshBringup {
    role: String,
    cfg: GroundStationConfig,
    device_id: String,
    paths: BringupPaths,
    mesh_iface: String,
    mesh_id: String,
    psk: Vec<u8>,
    wpa: Option<GroupChild>,
    gw_mode: Option<&'static str>,
    fault: Option<&'static str>,
}

impl MeshBringup {
    pub fn new(
        role: &str,
        cfg: GroundStationConfig,
        device_id: String,
        paths: BringupPaths,
    ) -> Self {
        Self {
            role: role.to_string(),
            cfg,
            device_id,
            paths,
            mesh_iface: String::new(),
            mesh_id: String::new(),
            psk: Vec::new(),
            wpa: None,
            gw_mode: None,
            fault: None,
        }
    }

    /// Initial bring-up: identity, interface, module, secure join, bind, gateway.
    pub async fn setup(&mut self) -> Result<(), BringupError> {
        let configured = self.cfg.mesh.mesh_id.clone().unwrap_or_default();
        let identity = ensure_mesh_identity_at(
            &self.role,
            &configured,
            &self.paths.mesh_id_path,
            Path::new(&self.cfg.mesh.shared_key_path),
            &self.device_id,
        )
        .map_err(|e| match e {
            MeshIdentityError::Missing(m) => BringupError::IdentityMissing(m),
            other => BringupError::Fatal(other.to_string()),
        })?;
        self.mesh_id = identity.mesh_id;
        self.psk = identity.psk;

        self.mesh_iface = self
            .pick_iface()
            .ok_or_else(|| BringupError::Fatal("no mesh interface found".into()))?;
        if !modprobe_batman().await {
            return Err(BringupError::Fatal("batman-adv module did not load".into()));
        }
        if !self.join_and_bind().await {
            return Err(BringupError::Fatal("secure mesh join failed".into()));
        }
        let mode = self.update_gateway_mode().await;
        let pinned = apply_persisted_gateway_preference(&self.paths.gateway_pref).await;
        tracing::info!(
            role = %self.role,
            mesh_iface = %self.mesh_iface,
            carrier = %self.cfg.mesh.carrier,
            mesh_id = %self.mesh_id,
            gw_mode = ?mode,
            pinned_mac = ?pinned,
            "mesh_up"
        );
        Ok(())
    }

    fn pick_iface(&self) -> Option<String> {
        iface::pick_mesh_iface(
            self.cfg.mesh.interface_override.as_deref(),
            &self.role,
            &self.paths.run_dir,
            &self.paths.sys_net,
        )
    }

    async fn stop_wpa(&mut self) {
        if let Some(mut wpa) = self.wpa.take() {
            wpa.terminate(WPA_STOP_GRACE).await;
        }
    }

    async fn join_and_bind(&mut self) -> bool {
        self.stop_wpa().await;
        let Some(wpa) = self.bring_up_iface().await else {
            return false;
        };
        self.wpa = Some(wpa);
        bind_iface_to_bat(&self.mesh_iface, &self.cfg.mesh.bat_iface).await
    }

    /// Join with authenticated keying or not at all: write the private config,
    /// set the interface type, start wpa_supplicant and wait for `iw` to show the
    /// join. Never falls back to an open join.
    async fn bring_up_iface(&self) -> Option<GroupChild> {
        let iface = self.mesh_iface.as_str();
        let carrier = self.cfg.mesh.carrier.as_str();
        let Some(set_type) = iw_set_type(carrier) else {
            tracing::error!(carrier, "unknown_carrier");
            return None;
        };
        let conf = channel_freq_mhz(self.cfg.mesh.channel).and_then(|freq| {
            wpa_supplicant_conf(carrier, &self.mesh_id, &mesh_passphrase(&self.psk), freq)
        });
        let conf = match conf {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "mesh_identity_invalid");
                return None;
            }
        };
        let conf_path = wpa_conf_path(&self.paths.wpa_dir(), iface);
        if let Err(e) = write_private(&conf_path, &conf) {
            tracing::error!(path = %conf_path.display(), error = %e, "mesh_wpa_conf_write_failed");
            return None;
        }
        batctl::run("ip", &["link", "set", iface, "down"], CMD_TIMEOUT).await;
        let (rc, _, err) =
            batctl::run("iw", &["dev", iface, "set", "type", set_type], CMD_TIMEOUT).await;
        if rc != 0 {
            tracing::warn!(iface, r#type = set_type, err = %err.trim(), "iw_set_type_failed");
        }
        batctl::run("ip", &["link", "set", iface, "up"], CMD_TIMEOUT).await;

        let args: Vec<String> = ["-i", iface, "-D", "nl80211", "-c"]
            .iter()
            .map(|s| s.to_string())
            .chain([conf_path.to_string_lossy().into_owned()])
            .collect();
        let spec = Spec {
            program: "wpa_supplicant",
            args: &args,
            stdout: StdoutMode::Null,
            stderr: StderrMode::Inherit,
        };
        let mut wpa = match GroupChild::spawn("wpa_supplicant", spec) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(iface, error = %e, "wpa_supplicant_spawn_failed");
                return None;
            }
        };
        let deadline = Instant::now() + JOIN_VERIFY_TIMEOUT;
        loop {
            if !wpa.is_running() {
                tracing::error!(iface, "wpa_supplicant_exited");
                return None;
            }
            if iface_joined(iface, carrier, &self.mesh_id).await {
                return Some(wpa);
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(JOIN_VERIFY_POLL).await;
        }
        tracing::error!(iface, carrier, "mesh_secure_join_unverified");
        wpa.terminate(WPA_STOP_GRACE).await;
        None
    }

    /// Why the mesh is not up as configured, or `None` when it is.
    async fn mesh_fault(&mut self) -> Option<&'static str> {
        let iface = self.mesh_iface.clone();
        if iface.is_empty() || !self.paths.sys_net.join(&iface).exists() {
            return Some("iface_missing");
        }
        if self.reassigned_iface().is_some() {
            return Some("iface_reassigned");
        }
        if !self.wpa.as_mut().is_some_and(GroupChild::is_running) {
            return Some("wpa_supplicant_dead");
        }
        if !iface_joined(&iface, &self.cfg.mesh.carrier, &self.mesh_id).await {
            return Some("not_joined");
        }
        if !bat_member(&self.paths.sys_net, &iface, &self.cfg.mesh.bat_iface) {
            return Some("not_in_bat");
        }
        None
    }

    /// The adapter the resolver now assigns to the mesh, when present and not the
    /// one in use. An operator override is never reassigned.
    fn reassigned_iface(&self) -> Option<String> {
        if self
            .cfg
            .mesh
            .interface_override
            .as_deref()
            .is_some_and(|o| !o.is_empty())
        {
            return None;
        }
        iface::published_mesh_adapter(
            &self.role,
            &self.paths.run_dir,
            std::time::SystemTime::now(),
        )
        .filter(|p| *p != self.mesh_iface && self.paths.sys_net.join(p).exists())
    }

    /// Re-decide the gateway mode; run `batctl` only when it changed.
    async fn update_gateway_mode(&mut self) -> Option<&'static str> {
        let mode = gateway_mode_for(
            &self.role,
            &self.cfg.cloud_uplink,
            self.paths.uplink_flag().is_file(),
        );
        if self.gw_mode != Some(mode) && apply_gateway_mode(mode).await {
            tracing::info!(previous = ?self.gw_mode, mode, "mesh_gw_mode_applied");
            self.gw_mode = Some(mode);
        }
        self.gw_mode
    }

    /// One supervision pass: heal the mesh if lost, then re-evaluate the gateway.
    pub async fn tick(&mut self) {
        let mut fault = self.mesh_fault().await;
        if let Some(reason) = fault {
            if Some(reason) != self.fault {
                tracing::warn!(iface = %self.mesh_iface, reason, "mesh_link_lost");
            }
            if reason == "iface_missing" || reason == "iface_reassigned" {
                // A replugged dongle can return under another name, and the
                // resolver can hand the mesh a different adapter.
                self.stop_wpa().await;
                if reason == "iface_reassigned" && !self.mesh_iface.is_empty() {
                    batctl::run("batctl", &["if", "del", &self.mesh_iface], CMD_TIMEOUT).await;
                }
                if let Some(next) = self
                    .pick_iface()
                    .filter(|i| self.paths.sys_net.join(i).exists())
                {
                    self.mesh_iface = next;
                    if self.join_and_bind().await {
                        fault = None;
                    }
                }
            } else if self.join_and_bind().await {
                fault = None;
            }
            if fault.is_none() {
                tracing::info!(iface = %self.mesh_iface, "mesh_rejoined");
            }
        }
        self.fault = fault;
        self.update_gateway_mode().await;
    }

    /// Leave the mesh and take the interfaces down.
    pub async fn teardown(&mut self) {
        self.stop_wpa().await;
        if !self.mesh_iface.is_empty() {
            let iface = self.mesh_iface.clone();
            batctl::run("batctl", &["if", "del", &iface], CMD_TIMEOUT).await;
            batctl::run("ip", &["link", "set", &iface, "down"], CMD_TIMEOUT).await;
            let _ = std::fs::remove_file(wpa_conf_path(&self.paths.wpa_dir(), &iface));
        }
        batctl::run(
            "ip",
            &["link", "set", &self.cfg.mesh.bat_iface, "down"],
            CMD_TIMEOUT,
        )
        .await;
    }
}

async fn bind_iface_to_bat(iface: &str, bat_iface: &str) -> bool {
    let (rc, _, err) = batctl::run("batctl", &["if", "add", iface], CMD_TIMEOUT).await;
    if rc != 0 && !err.to_lowercase().contains("already") {
        tracing::error!(iface, err = %err.trim(), "batctl_if_add_failed");
        return false;
    }
    let (rc, _, err) = batctl::run("ip", &["link", "set", bat_iface, "up"], CMD_TIMEOUT).await;
    if rc != 0 {
        tracing::error!(iface = bat_iface, err = %err.trim(), "bat_iface_up_failed");
        return false;
    }
    true
}

/// The supervisor request that moves this node to `direct`.
pub const DIRECT_ROLE_REQUEST: &str =
    r#"{"op": "set_role", "role": "direct", "reason": "mesh_identity_missing"}"#;

/// Ask the supervisor (the owner of role transitions) to move this node to
/// `direct`. Best-effort: an unreachable supervisor is logged.
pub async fn request_direct_role(sock: &Path) {
    let connect = tokio::time::timeout(ROLE_REQUEST_TIMEOUT, tokio::net::UnixStream::connect(sock));
    let mut stream = match connect.await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "mesh_role_downgrade_unreachable");
            return;
        }
        Err(_) => {
            tracing::error!("mesh_role_downgrade_unreachable: connect timed out");
            return;
        }
    };
    if let Err(e) = stream
        .write_all(format!("{DIRECT_ROLE_REQUEST}\n").as_bytes())
        .await
    {
        tracing::error!(error = %e, "mesh_role_downgrade_unreachable");
        return;
    }
    let mut reply = String::new();
    let _ = tokio::time::timeout(
        ROLE_REQUEST_TIMEOUT,
        BufReader::new(stream).read_line(&mut reply),
    )
    .await;
    tracing::info!(reply = %reply.trim(), "mesh_role_downgrade_requested");
}
