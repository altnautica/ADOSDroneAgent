//! USB composite gadget lifecycle (CDC-NCM + RNDIS).
//!
//! Builds a libcomposite gadget on the OTG port so a tethered host (Mac, Win,
//! Linux, Android 11+) gets a USB-Ethernet link and a DHCP lease for
//! 192.168.7.2. The gadget lives under `/sys/kernel/config/usb_gadget/ados_gs`
//! and binds to the first `/sys/class/udc`; after bind, usb0 gets
//! 192.168.7.1/24 and a single-host `dnsmasq` serves the tethered host. Ports
//! `usb_gadget.py`.
//!
//! The dnsmasq fork goes through [`ManagedProcess`] (setsid + killpg, Rule 37)
//! so a teardown can never orphan a dnsmasq holding a bound socket on usb0.
//! Posture: requires root + configfs; off-Linux or unprivileged, the configfs
//! writes fail cleanly and `setup` returns false without panicking.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tracing::{error, info, warn};

use crate::process::ManagedProcess;

const GADGET_NAME: &str = "ados_gs";
const USB_INTERFACE: &str = "usb0";
const USB_IP: &str = "192.168.7.1";
const USB_NETMASK_PREFIX: u32 = 24;
const DHCP_RANGE_START: &str = "192.168.7.2";
const DHCP_RANGE_END: &str = "192.168.7.2";

// USB descriptor values, straight from the gadget spec.
const ID_VENDOR: &str = "0x1d6b"; // Linux Foundation
const ID_PRODUCT: &str = "0x0104"; // Multifunction composite gadget
const BCD_DEVICE: &str = "0x0100";
const BCD_USB: &str = "0x0200";
const STR_MANUFACTURER: &str = "ADOS Ground Station";
const STR_PRODUCT: &str = "ADOS GS";
const CONFIG_MAX_POWER: &str = "250"; // mA

/// Render the usb0 dnsmasq conf body. EXACT line order matches the Python
/// `_start_dnsmasq` conf; ends in a single trailing newline. The pid path is
/// the canonical `DNSMASQ_USB0_PID`.
pub fn render_usb_dnsmasq_conf(pid_path: &str) -> String {
    let lines = [
        format!("interface={USB_INTERFACE}"),
        "bind-interfaces".to_string(),
        "except-interface=lo".to_string(),
        format!("listen-address={USB_IP}"),
        format!("dhcp-range={DHCP_RANGE_START},{DHCP_RANGE_END},255.255.255.0,12h"),
        format!("dhcp-option=option:router,{USB_IP}"),
        format!("dhcp-option=option:dns-server,{USB_IP}"),
        "no-resolv".to_string(),
        "no-hosts".to_string(),
        "log-dhcp".to_string(),
        format!("pid-file={pid_path}"),
        String::new(),
    ];
    lines.join("\n")
}

/// libcomposite composite-gadget setup for the ground-station profile.
pub struct UsbGadgetManager {
    gadget_root: PathBuf,
    udc_dir: PathBuf,
    net_dir: PathBuf,
    dnsmasq_conf_path: PathBuf,
    dnsmasq_pid_path: PathBuf,
    dnsmasq: Option<ManagedProcess>,
    bound: bool,
}

impl UsbGadgetManager {
    /// Manager with canonical configfs / sysfs / runtime paths.
    pub fn new() -> Self {
        Self::with_roots(
            PathBuf::from("/sys/kernel/config/usb_gadget"),
            PathBuf::from("/sys/class/udc"),
            PathBuf::from("/sys/class/net"),
            PathBuf::from(crate::paths::DNSMASQ_USB0_CONF),
            PathBuf::from(crate::paths::DNSMASQ_USB0_PID),
        )
    }

    /// Full constructor (tests point the roots at a tempdir).
    pub fn with_roots(
        gadget_root: PathBuf,
        udc_dir: PathBuf,
        net_dir: PathBuf,
        dnsmasq_conf_path: PathBuf,
        dnsmasq_pid_path: PathBuf,
    ) -> Self {
        Self {
            gadget_root,
            udc_dir,
            net_dir,
            dnsmasq_conf_path,
            dnsmasq_pid_path,
            dnsmasq: None,
            bound: false,
        }
    }

    fn gadget_dir(&self) -> PathBuf {
        self.gadget_root.join(GADGET_NAME)
    }

    /// True if the configfs gadget root is present (the dwc2 + configfs-usb
    /// modules must both be loaded for it to appear). Mirrors
    /// `configfs_available`.
    pub fn configfs_available(&self) -> bool {
        self.gadget_root.is_dir()
    }

    /// Build the configfs tree and bind to a UDC. Mirrors `setup`. On failure
    /// returns false and leaves partial state; `teardown` is safe regardless.
    pub fn build_gadget_tree(&mut self) -> std::io::Result<()> {
        let gadget = self.gadget_dir();

        // Idempotent: a stale tree from a prior run is torn down first.
        if gadget.exists() {
            info!(path = %gadget.display(), "usb_gadget_stale_found");
            self.teardown_tree();
        }

        std::fs::create_dir(&gadget)?;
        write_no_newline(&gadget.join("idVendor"), ID_VENDOR)?;
        write_no_newline(&gadget.join("idProduct"), ID_PRODUCT)?;
        write_no_newline(&gadget.join("bcdDevice"), BCD_DEVICE)?;
        write_no_newline(&gadget.join("bcdUSB"), BCD_USB)?;

        let strings = gadget.join("strings").join("0x409");
        std::fs::create_dir_all(&strings)?;
        write_no_newline(&strings.join("manufacturer"), STR_MANUFACTURER)?;
        write_no_newline(&strings.join("product"), STR_PRODUCT)?;

        // NCM (macOS, Win11, Linux, Android 11+) + RNDIS (Win10 fallback).
        std::fs::create_dir_all(gadget.join("functions").join("ncm.usb0"))?;
        std::fs::create_dir_all(gadget.join("functions").join("rndis.usb0"))?;

        let config = gadget.join("configs").join("c.1");
        std::fs::create_dir_all(&config)?;
        write_no_newline(&config.join("MaxPower"), CONFIG_MAX_POWER)?;

        // Link both functions into the one composite config.
        for fname in ["ncm.usb0", "rndis.usb0"] {
            let link = config.join(fname);
            if !link.exists() {
                std::os::unix::fs::symlink(gadget.join("functions").join(fname), &link)?;
            }
        }

        let udc = match self.pick_udc() {
            Some(u) => u,
            None => {
                error!("usb_gadget_no_udc");
                return Err(std::io::Error::other("no /sys/class/udc entries"));
            }
        };
        write_no_newline(&gadget.join("UDC"), &udc)?;
        self.bound = true;
        info!(udc = %udc, "usb_gadget_bound");
        Ok(())
    }

    /// First entry in `/sys/class/udc`, alphabetically. Mirrors `_pick_udc`.
    fn pick_udc(&self) -> Option<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.udc_dir)
            .ok()?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names.into_iter().next()
    }

    /// Full bring-up: build the tree, bring usb0 up, start dnsmasq. Mirrors
    /// `setup`. Returns false on any hard failure (dnsmasq is best-effort).
    pub async fn setup(&mut self) -> bool {
        warn_if_not_root();
        if !self.configfs_available() {
            error!(path = %self.gadget_root.display(), "usb_gadget_configfs_missing");
            return false;
        }
        if let Err(exc) = self.build_gadget_tree() {
            error!(error = %exc, "usb_gadget_setup_failed");
            return false;
        }
        if !self.bring_up_interface().await {
            return false;
        }
        if !self.start_dnsmasq().await {
            warn!("usb_gadget_dnsmasq_failed");
        }
        true
    }

    /// Poll for usb0 (20 × 0.1s), flush, assign 192.168.7.1/24, link up. Mirrors
    /// `_bring_up_interface`.
    async fn bring_up_interface(&self) -> bool {
        let iface_path = self.net_dir.join(USB_INTERFACE);
        let mut appeared = false;
        for _ in 0..20 {
            if iface_path.exists() {
                appeared = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !appeared {
            error!(interface = USB_INTERFACE, "usb_gadget_interface_missing");
            return false;
        }
        // ip addr flush is best-effort; add + link-up are required.
        run_ip(&["addr", "flush", "dev", USB_INTERFACE]).await;
        let cidr = format!("{USB_IP}/{USB_NETMASK_PREFIX}");
        if !run_ip(&["addr", "add", &cidr, "dev", USB_INTERFACE]).await {
            error!("usb_gadget_ip_failed");
            return false;
        }
        if !run_ip(&["link", "set", USB_INTERFACE, "up"]).await {
            error!("usb_gadget_ip_failed");
            return false;
        }
        info!(
            interface = USB_INTERFACE,
            ip = USB_IP,
            "usb_gadget_interface_up"
        );
        true
    }

    /// Write the conf and fork dnsmasq under a managed process group. Mirrors
    /// `_start_dnsmasq`.
    async fn start_dnsmasq(&mut self) -> bool {
        if let Some(parent) = self.dnsmasq_conf_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conf = render_usb_dnsmasq_conf(&self.dnsmasq_pid_path.to_string_lossy());
        if let Err(exc) = std::fs::write(&self.dnsmasq_conf_path, conf) {
            error!(error = %exc, "dnsmasq_conf_write_failed");
            return false;
        }
        let conf_arg = format!("--conf-file={}", self.dnsmasq_conf_path.to_string_lossy());
        match ManagedProcess::spawn("dnsmasq", &["--keep-in-foreground", &conf_arg]) {
            Ok(proc) => {
                info!(pid = ?proc.pid(), conf = %self.dnsmasq_conf_path.display(), "usb_gadget_dnsmasq_started");
                self.dnsmasq = Some(proc);
                true
            }
            Err(exc) => {
                error!(error = %exc, "dnsmasq_spawn_failed");
                false
            }
        }
    }

    /// Tear everything down in order: kill dnsmasq → unbind UDC → remove
    /// symlinks → rmdir config/functions/strings/gadget. Mirrors `teardown`.
    pub async fn teardown(&mut self) {
        // dnsmasq first so the lease does not linger as the iface disappears.
        if let Some(mut proc) = self.dnsmasq.take() {
            proc.kill().await; // killpg, no orphan
            info!("usb_gadget_dnsmasq_stopped");
        }
        self.teardown_tree();
    }

    /// The configfs-tree teardown (no dnsmasq). Split out so `build_gadget_tree`
    /// can reuse it for the stale-rebuild path without an async context.
    fn teardown_tree(&mut self) {
        let gadget = self.gadget_dir();

        // Unbind from the UDC (write a newline, as the Python does).
        let udc_file = gadget.join("UDC");
        if udc_file.exists() {
            if let Err(exc) = std::fs::write(&udc_file, "\n") {
                tracing::debug!(error = %exc, "usb_gadget_unbind_failed");
            }
        }

        // Remove the function symlinks (and, on a regular filesystem, the
        // MaxPower attribute file) in configs/c.1 before rmdir. On real configfs
        // MaxPower is an attribute that vanishes with the c.1 rmdir, so the
        // remove_file is a harmless no-op there; on a normal fs it is required
        // for the rmdir to succeed.
        let config = gadget.join("configs").join("c.1");
        if config.exists() {
            if let Ok(rd) = std::fs::read_dir(&config) {
                for entry in rd.flatten() {
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    if !is_dir {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            let _ = std::fs::remove_dir(&config);
        }

        // Remove function directories.
        let functions = gadget.join("functions");
        if functions.exists() {
            if let Ok(rd) = std::fs::read_dir(&functions) {
                for entry in rd.flatten() {
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
        }

        // Remove strings. On a regular filesystem the manufacturer / product
        // attribute files must be cleared before the rmdir; on configfs they
        // are attributes that vanish with the rmdir, so the remove_file is a
        // harmless no-op there.
        let strings = gadget.join("strings").join("0x409");
        if strings.exists() {
            if let Ok(rd) = std::fs::read_dir(&strings) {
                for entry in rd.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&strings);
        }

        // Remove the now-empty intermediate parent dirs before the gadget
        // rmdir. On configfs these collapse on their own, but a final rmdir of
        // the gadget needs an empty dir on a regular filesystem too.
        for sub in ["configs", "functions", "strings"] {
            let _ = std::fs::remove_dir(gadget.join(sub));
        }

        // Remove the top-level gadget dir last.
        if gadget.exists() {
            match std::fs::remove_dir(&gadget) {
                Ok(()) => info!("usb_gadget_removed"),
                Err(exc) => tracing::debug!(error = %exc, "usb_gadget_rmdir_failed"),
            }
        }
        self.bound = false;
    }

    /// Status snapshot for the API surface.
    pub fn status(&self) -> Value {
        json!({
            "bound": self.bound,
            "interface": USB_INTERFACE,
            "ip": USB_IP,
            "configfs_available": self.configfs_available(),
        })
    }
}

impl Default for UsbGadgetManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Write `value` to a configfs attribute with NO trailing newline (configfs
/// rejects a trailing newline on some attributes). Mirrors the Python `_write`.
fn write_no_newline(path: &Path, value: &str) -> std::io::Result<()> {
    std::fs::write(path, value)
}

fn warn_if_not_root() {
    #[cfg(unix)]
    {
        // SAFETY: geteuid is always safe.
        let euid = unsafe { geteuid() };
        if euid != 0 {
            warn!(euid = euid, "usb_gadget_not_root");
        }
    }
}

#[cfg(unix)]
extern "C" {
    fn geteuid() -> u32;
}

/// Run an `ip` subcommand, returning whether it exited zero. Best-effort; a
/// spawn failure logs and returns false.
async fn run_ip(args: &[&str]) -> bool {
    let mut cmd = tokio::process::Command::new("ip");
    cmd.args(args).stdin(std::process::Stdio::null());
    match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(out)) => out.status.success(),
        Ok(Err(exc)) => {
            warn!(error = %exc, "usb_gadget_ip_spawn_failed");
            false
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dnsmasq_conf_is_byte_exact() {
        let conf = render_usb_dnsmasq_conf("/run/ados/dnsmasq-usb0.pid");
        let expected = "interface=usb0\n\
bind-interfaces\n\
except-interface=lo\n\
listen-address=192.168.7.1\n\
dhcp-range=192.168.7.2,192.168.7.2,255.255.255.0,12h\n\
dhcp-option=option:router,192.168.7.1\n\
dhcp-option=option:dns-server,192.168.7.1\n\
no-resolv\n\
no-hosts\n\
log-dhcp\n\
pid-file=/run/ados/dnsmasq-usb0.pid\n";
        assert_eq!(conf, expected);
    }

    #[test]
    fn build_gadget_tree_writes_descriptors_without_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let gadget_root = dir.path().join("usb_gadget");
        std::fs::create_dir_all(&gadget_root).unwrap();
        // Fake a UDC so the bind step succeeds.
        let udc_dir = dir.path().join("udc");
        std::fs::create_dir_all(udc_dir.join("fe980000.usb")).unwrap();

        let mut m = UsbGadgetManager::with_roots(
            gadget_root.clone(),
            udc_dir,
            dir.path().join("net"),
            dir.path().join("dnsmasq-usb0.conf"),
            dir.path().join("dnsmasq-usb0.pid"),
        );
        assert!(m.configfs_available());
        m.build_gadget_tree().unwrap();

        let gadget = gadget_root.join("ados_gs");
        // Descriptors written with NO trailing newline.
        assert_eq!(
            std::fs::read_to_string(gadget.join("idVendor")).unwrap(),
            "0x1d6b"
        );
        assert_eq!(
            std::fs::read_to_string(gadget.join("idProduct")).unwrap(),
            "0x0104"
        );
        assert_eq!(
            std::fs::read_to_string(gadget.join("bcdUSB")).unwrap(),
            "0x0200"
        );
        // strings.
        assert_eq!(
            std::fs::read_to_string(gadget.join("strings/0x409/manufacturer")).unwrap(),
            "ADOS Ground Station"
        );
        // Both functions exist.
        assert!(gadget.join("functions/ncm.usb0").is_dir());
        assert!(gadget.join("functions/rndis.usb0").is_dir());
        // MaxPower.
        assert_eq!(
            std::fs::read_to_string(gadget.join("configs/c.1/MaxPower")).unwrap(),
            "250"
        );
        // Both functions symlinked into the config.
        assert!(gadget.join("configs/c.1/ncm.usb0").exists());
        assert!(gadget.join("configs/c.1/rndis.usb0").exists());
        // UDC bound to the fake entry.
        assert_eq!(
            std::fs::read_to_string(gadget.join("UDC")).unwrap(),
            "fe980000.usb"
        );
    }

    #[test]
    fn build_gadget_tree_fails_when_no_udc() {
        let dir = tempfile::tempdir().unwrap();
        let gadget_root = dir.path().join("usb_gadget");
        std::fs::create_dir_all(&gadget_root).unwrap();
        // Empty UDC dir → no entries.
        let udc_dir = dir.path().join("udc");
        std::fs::create_dir_all(&udc_dir).unwrap();
        let mut m = UsbGadgetManager::with_roots(
            gadget_root,
            udc_dir,
            dir.path().join("net"),
            dir.path().join("c.conf"),
            dir.path().join("c.pid"),
        );
        assert!(m.build_gadget_tree().is_err());
    }

    #[test]
    fn teardown_removes_the_structural_tree_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let gadget_root = dir.path().join("usb_gadget");
        std::fs::create_dir_all(&gadget_root).unwrap();
        let udc_dir = dir.path().join("udc");
        std::fs::create_dir_all(udc_dir.join("fe980000.usb")).unwrap();
        let mut m = UsbGadgetManager::with_roots(
            gadget_root.clone(),
            udc_dir,
            dir.path().join("net"),
            dir.path().join("c.conf"),
            dir.path().join("c.pid"),
        );
        m.build_gadget_tree().unwrap();
        let gadget = gadget_root.join("ados_gs");
        assert!(gadget.exists());
        m.teardown_tree();
        // The structural sub-trees (configs / functions / strings) and the
        // function symlinks are gone. On a regular filesystem the gadget dir
        // itself can linger because the descriptor *files* (idVendor, UDC, ...)
        // are real files here; on real configfs they are attributes that vanish
        // with the gadget rmdir, which the teardown attempts last. So assert the
        // verifiable invariant: every structural dir is removed.
        assert!(!gadget.join("configs").exists());
        assert!(!gadget.join("functions").exists());
        assert!(!gadget.join("strings").exists());
        assert!(!m.bound);
        // Idempotent: a second teardown on a partially-removed tree is a no-op.
        m.teardown_tree();
    }

    #[test]
    fn configfs_unavailable_when_root_missing() {
        let dir = tempfile::tempdir().unwrap();
        let m = UsbGadgetManager::with_roots(
            dir.path().join("does-not-exist"),
            dir.path().join("udc"),
            dir.path().join("net"),
            dir.path().join("c.conf"),
            dir.path().join("c.pid"),
        );
        assert!(!m.configfs_available());
    }
}
