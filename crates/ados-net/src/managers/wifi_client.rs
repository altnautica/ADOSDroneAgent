//! WiFi client (station) manager for the ground-station profile.
//!
//! The onboard wlan0 radio is either an AP (hostapd) or a station joining an
//! upstream network for backhaul; the two are mutually exclusive on one radio.
//! This manager coordinates with the AP service through an advisory file lock
//! and a "was the AP up?" flag so a failed join restores the AP. NetworkManager
//! owns credential storage; we persist only the "enabled on boot" flag and the
//! last SSID. Implements the [`UplinkManager`] trait. Ports
//! `wifi_client_manager.py`.
//!
//! RISK: the wlan0 lock must live across the whole `stop-hostapd → nmcli
//! connect` window and be released only by [`WifiClientManager::leave`] (or a
//! failed join's cleanup). It is held in the manager struct, not taken and
//! dropped per call.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::cmd::CmdRunner;
use crate::nmcli;
use crate::router::UplinkManager;

use self::lock::Wlan0Lock;

const WLAN_IFACE: &str = "wlan0";
const HOSTAPD_UNIT: &str = "ados-hostapd.service";
const LOCK_PATH: &str = "/var/lock/ados-wlan0.lock";
const AP_FLAG_PATH: &str = "/run/ados/ap-was-enabled";
const CLIENT_CONFIG_PATH: &str = "/etc/ados/ground-station-wifi-client.json";

const RUN_TIMEOUT: Duration = Duration::from_secs(15);
const SHORT_TIMEOUT: Duration = Duration::from_secs(5);
const STATUS_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SYSTEMCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// Persisted client config. Field order matches the Python dict insertion
/// order so `to_string_pretty` byte-matches `json.dumps(indent=2)`.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub enabled_on_boot: bool,
    #[serde(default)]
    pub last_ssid: Option<String>,
}

/// NetworkManager-backed WiFi station manager for wlan0.
pub struct WifiClientManager {
    interface: String,
    runner: Arc<dyn CmdRunner>,
    lock_path: PathBuf,
    ap_flag_path: PathBuf,
    client_config_path: PathBuf,
    /// The held wlan0 advisory lock. `Some` while we own the radio for STA mode
    /// across the stop-hostapd → connect window; dropped on `leave`.
    lock: Option<Wlan0Lock>,
}

impl WifiClientManager {
    /// Manager with canonical paths and the production runner.
    pub fn new(runner: Arc<dyn CmdRunner>) -> Self {
        Self::with_paths(
            WLAN_IFACE,
            runner,
            PathBuf::from(LOCK_PATH),
            PathBuf::from(AP_FLAG_PATH),
            PathBuf::from(CLIENT_CONFIG_PATH),
        )
    }

    /// Full constructor (tests).
    pub fn with_paths(
        interface: impl Into<String>,
        runner: Arc<dyn CmdRunner>,
        lock_path: PathBuf,
        ap_flag_path: PathBuf,
        client_config_path: PathBuf,
    ) -> Self {
        Self {
            interface: interface.into(),
            runner,
            lock_path,
            ap_flag_path,
            client_config_path,
            lock: None,
        }
    }

    /// True while the manager holds the wlan0 advisory lock.
    pub fn holds_lock(&self) -> bool {
        self.lock.is_some()
    }

    // ---------------- lock handling ----------------

    /// Acquire the exclusive non-blocking wlan0 lock and stash it in the
    /// struct. Returns false when another holder owns it.
    fn acquire_lock(&mut self) -> bool {
        if self.lock.is_some() {
            return true;
        }
        match Wlan0Lock::try_acquire(&self.lock_path) {
            Ok(lk) => {
                self.lock = Some(lk);
                true
            }
            Err(exc) => {
                warn!(error = %exc, "wlan0_lock_failed");
                false
            }
        }
    }

    /// Drop the held lock (LOCK_UN + close happen in `Wlan0Lock::drop`).
    fn release_lock(&mut self) {
        self.lock = None;
    }

    async fn is_hostapd_active(&self) -> bool {
        let out = self
            .runner
            .run(&["systemctl", "is-active", HOSTAPD_UNIT], SHORT_TIMEOUT)
            .await;
        out.stdout.trim() == "active"
    }

    fn write_ap_flag(&self, enabled: bool) {
        if let Some(parent) = self.ap_flag_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = if enabled { "1\n" } else { "0\n" };
        if let Err(exc) = std::fs::write(&self.ap_flag_path, body) {
            warn!(error = %exc, "ap_flag_write_failed");
        }
    }

    fn read_ap_flag(&self) -> bool {
        std::fs::read_to_string(&self.ap_flag_path)
            .map(|s| s.trim() == "1")
            .unwrap_or(false)
    }

    fn clear_ap_flag(&self) {
        match std::fs::remove_file(&self.ap_flag_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(exc) => warn!(error = %exc, "ap_flag_clear_failed"),
        }
    }

    // ---------------- client config ----------------

    fn load_client_config(&self) -> ClientConfig {
        match std::fs::read(&self.client_config_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => ClientConfig::default(),
        }
    }

    /// Persist `{enabled_on_boot, last_ssid}` as `json.dumps(indent=2)+"\n"`,
    /// atomically (tmp `.json.tmp` sibling + rename). Mirrors
    /// `_save_client_config`.
    fn save_client_config(&self, data: &ClientConfig) {
        let body = match serde_json::to_string_pretty(data) {
            Ok(s) => format!("{s}\n"),
            Err(exc) => {
                warn!(error = %exc, "client_config_encode_failed");
                return;
            }
        };
        if let Err(exc) = crate::sidecar::write_atomic(&self.client_config_path, body.as_bytes()) {
            warn!(error = %exc, "client_config_write_failed");
        }
    }

    /// Set the "rejoin on boot" flag, preserving the last SSID.
    pub async fn set_enabled_on_boot(&self, enabled: bool) -> ClientConfig {
        let mut data = self.load_client_config();
        data.enabled_on_boot = enabled;
        self.save_client_config(&data);
        data
    }

    // ---------------- public API ----------------

    /// Join a WiFi network, coordinating wlan0 with hostapd. Mirrors `join`.
    pub async fn join(&mut self, ssid: &str, passphrase: Option<&str>, force: bool) -> Value {
        if ssid.is_empty() {
            return json!({"joined": false, "error": "ssid_required", "ip": null, "gateway": null});
        }

        let ap_active = self.is_hostapd_active().await;
        if ap_active && !force {
            return json!({
                "joined": false,
                "error": "wlan0_busy_ap_active",
                "hint": "Stop AP first or force",
                "ip": null,
                "gateway": null,
            });
        }

        if !self.acquire_lock() {
            return json!({"joined": false, "error": "wlan0_locked", "ip": null, "gateway": null});
        }

        // Record whether the AP was up so leave() (or a failed join) restores it.
        self.write_ap_flag(ap_active);
        if ap_active {
            info!(ssid = ssid, "stopping_hostapd_for_client");
            self.runner
                .run(&["systemctl", "stop", HOSTAPD_UNIT], SYSTEMCTL_TIMEOUT)
                .await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        // Keep the passphrase OUT of argv: a cleartext `password <pw>` argument
        // would be visible in /proc/<pid>/cmdline to any local user for the life
        // of the nmcli call. Instead, run nmcli in interactive-secret mode
        // (`--ask`) which prompts for the secret and reads it from stdin, and
        // feed the passphrase on stdin. With no secret needed (open network)
        // the argv is unchanged and no stdin is sent.
        let out = match passphrase.filter(|p| !p.is_empty()) {
            Some(pw) => {
                let cmd: Vec<&str> = vec![
                    "nmcli",
                    "--ask",
                    "device",
                    "wifi",
                    "connect",
                    ssid,
                    "ifname",
                    &self.interface,
                ];
                // nmcli --ask reads the WiFi secret as a line from stdin; the
                // trailing newline terminates the prompt response.
                let mut secret = pw.as_bytes().to_vec();
                secret.push(b'\n');
                let res = self
                    .runner
                    .run_with_stdin(&cmd, &secret, CONNECT_TIMEOUT)
                    .await;
                // Zeroize our copy of the secret promptly.
                secret.fill(0);
                res
            }
            None => {
                let cmd: Vec<&str> = vec![
                    "nmcli",
                    "device",
                    "wifi",
                    "connect",
                    ssid,
                    "ifname",
                    &self.interface,
                ];
                self.runner.run(&cmd, CONNECT_TIMEOUT).await
            }
        };
        if !out.ok() {
            warn!(ssid = ssid, "wifi_join_failed");
            // Restore the AP if we stole it, then release the lock.
            if ap_active {
                self.runner
                    .run(&["systemctl", "start", HOSTAPD_UNIT], SYSTEMCTL_TIMEOUT)
                    .await;
                self.clear_ap_flag();
            }
            self.release_lock();
            return json!({
                "joined": false,
                "error": err_or(&out.stderr, "nmcli_failed"),
                "ip": null,
                "gateway": null,
            });
        }

        // Force power-save off so the radio never parks and drops the uplink.
        // Resolve the real active connection name on this iface first, because
        // NetworkManager may auto-name the connection differently from the SSID
        // (so `connection modify <ssid>` would warn-and-no-op). Fall back to the
        // SSID when no active connection is reported.
        let conn = self
            .active_connection_name()
            .await
            .unwrap_or_else(|| ssid.to_string());
        self.disable_powersave(&conn).await;

        tokio::time::sleep(Duration::from_secs(2)).await;
        let st = self.status().await;
        let mut data = self.load_client_config();
        data.last_ssid = Some(ssid.to_string());
        self.save_client_config(&data);

        // Join succeeded. The lock stays held (we own the radio in STA mode);
        // it is released by leave().
        json!({
            "joined": true,
            "error": null,
            "ip": st.get("ip").cloned().unwrap_or(Value::Null),
            "gateway": st.get("gateway").cloned().unwrap_or(Value::Null),
        })
    }

    /// Resolve the NetworkManager connection name currently active on this
    /// iface (`nmcli -t -f NAME,DEVICE connection show --active`). Returns the
    /// first row whose DEVICE matches the managed interface. `None` when no
    /// active connection is bound to the iface (or the query failed).
    async fn active_connection_name(&self) -> Option<String> {
        let out = self
            .runner
            .run(
                &[
                    "nmcli",
                    "-t",
                    "-f",
                    "NAME,DEVICE",
                    "connection",
                    "show",
                    "--active",
                ],
                STATUS_TIMEOUT,
            )
            .await;
        if !out.ok() {
            return None;
        }
        for row in nmcli::parse_terse(&out.stdout, 2) {
            if row[1].trim() == self.interface {
                let name = row[0].trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
        None
    }

    async fn disable_powersave(&self, connection: &str) {
        let nm = self
            .runner
            .run(
                &[
                    "nmcli",
                    "connection",
                    "modify",
                    connection,
                    "802-11-wireless.powersave",
                    "2",
                ],
                RUN_TIMEOUT,
            )
            .await;
        if !nm.ok() {
            warn!(connection = connection, "wifi_powersave_nmcli_failed");
        }
        let iw = self
            .runner
            .run(
                &["iw", "dev", &self.interface, "set", "power_save", "off"],
                RUN_TIMEOUT,
            )
            .await;
        if !iw.ok() {
            warn!(interface = %self.interface, "wifi_powersave_iw_failed");
        }
    }

    /// Disconnect the current WiFi client connection, restore the AP if it was
    /// ours, and release the wlan0 lock. Mirrors `leave`.
    pub async fn leave(&mut self) -> Value {
        let st = self.status().await;
        let prev_ssid = st.get("ssid").and_then(|v| v.as_str()).map(str::to_string);
        let prev_ssid = match prev_ssid {
            Some(s) => s,
            None => {
                self.release_lock();
                return json!({"left": false, "previous_ssid": null});
            }
        };

        let down = self
            .runner
            .run(
                &["nmcli", "connection", "down", &prev_ssid],
                SYSTEMCTL_TIMEOUT,
            )
            .await;
        if !down.ok() {
            // Fallback: disconnect the device.
            self.runner
                .run(
                    &["nmcli", "device", "disconnect", &self.interface],
                    SYSTEMCTL_TIMEOUT,
                )
                .await;
        }

        // Restore hostapd if it was running before we took the radio.
        if self.read_ap_flag() {
            info!("restoring_hostapd_after_client_leave");
            self.runner
                .run(&["systemctl", "start", HOSTAPD_UNIT], SYSTEMCTL_TIMEOUT)
                .await;
        }
        self.clear_ap_flag();
        self.release_lock();

        json!({"left": true, "previous_ssid": prev_ssid})
    }

    /// Delete a saved NetworkManager connection profile by name. Removes the
    /// stored credentials + the autoconnect profile so the agent never silently
    /// rejoins on reboot; the active link drops as a side effect when the deleted
    /// profile is the currently-active one. Mirrors `forget`. Stateless (`&self`,
    /// no lock): deleting a saved profile does not change AP/STA radio ownership.
    pub async fn forget(&self, connection_name: &str) -> Value {
        if connection_name.is_empty() {
            return json!({"forgot": false, "name": "", "error": "name_required"});
        }
        let out = self
            .runner
            .run(
                &["nmcli", "connection", "delete", connection_name],
                SYSTEMCTL_TIMEOUT,
            )
            .await;
        if out.ok() {
            json!({"forgot": true, "name": connection_name, "error": null})
        } else {
            json!({"forgot": false, "name": connection_name, "error": "nmcli_failed"})
        }
    }

    /// Current station status. Mirrors `status`.
    pub async fn status(&self) -> serde_json::Map<String, Value> {
        let list = self
            .runner
            .run(
                &[
                    "nmcli",
                    "-t",
                    "-f",
                    "ACTIVE,SSID,BSSID,SIGNAL,SECURITY",
                    "device",
                    "wifi",
                    "list",
                    "ifname",
                    &self.interface,
                ],
                STATUS_TIMEOUT,
            )
            .await;
        let mut active_ssid: Option<String> = None;
        let mut bssid: Option<String> = None;
        let mut signal_val: Option<i64> = None;
        let mut security: Option<String> = None;
        if list.ok() {
            for row in nmcli::parse_terse(&list.stdout, 5) {
                if row[0].trim() == "yes" {
                    active_ssid = Some(row[1].clone());
                    bssid = Some(row[2].clone());
                    signal_val = row[3].parse::<i64>().ok();
                    security = Some(row[4].clone());
                    break;
                }
            }
        }

        let addr = self
            .runner
            .run(
                &["ip", "-4", "addr", "show", &self.interface],
                STATUS_TIMEOUT,
            )
            .await;
        let ip = if addr.ok() {
            super::ethernet::parse_inet(&addr.stdout)
        } else {
            None
        };
        let route = self
            .runner
            .run(
                &[
                    "ip",
                    "-4",
                    "route",
                    "show",
                    "default",
                    "dev",
                    &self.interface,
                ],
                STATUS_TIMEOUT,
            )
            .await;
        let gateway = if route.ok() {
            super::ethernet::parse_default_via(&route.stdout)
        } else {
            None
        };

        let connected = active_ssid.is_some() && ip.is_some();
        let mut m = serde_json::Map::new();
        m.insert("connected".into(), json!(connected));
        m.insert("ssid".into(), json!(active_ssid));
        m.insert("bssid".into(), json!(bssid));
        m.insert("signal".into(), json!(signal_val));
        m.insert("ip".into(), json!(ip));
        m.insert("gateway".into(), json!(gateway));
        m.insert("security".into(), json!(security));
        m
    }
}

#[async_trait]
impl UplinkManager for WifiClientManager {
    async fn is_up(&self) -> bool {
        self.status()
            .await
            .get("connected")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }
    fn get_iface(&self) -> String {
        self.interface.clone()
    }
    async fn get_gateway(&self) -> Option<String> {
        self.status()
            .await
            .get("gateway")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

fn err_or(stderr: &str, fallback: &str) -> String {
    let t = stderr.trim();
    if t.is_empty() {
        fallback.to_string()
    } else {
        t.to_string()
    }
}

/// The advisory wlan0 lock. On Linux it is a real `flock(LOCK_EX|LOCK_NB)` held
/// on an open fd via `nix::fcntl::Flock`; the lock is released (`LOCK_UN`) and
/// the fd closed when the value is dropped. On non-Linux dev hosts an
/// equivalent `flock` is taken through a tiny libc FFI so the lock-lifetime
/// behavior is the same and testable.
mod lock {
    use std::fs::{File, OpenOptions};
    use std::path::Path;

    /// A held advisory lock. Drop releases it.
    pub struct Wlan0Lock {
        #[cfg(target_os = "linux")]
        _flock: nix::fcntl::Flock<File>,
        #[cfg(not(target_os = "linux"))]
        _file: File,
    }

    impl Wlan0Lock {
        /// Try to take the exclusive non-blocking lock on `path`. Errs when the
        /// lock is already held by another owner or the file cannot be opened.
        pub fn try_acquire(path: &Path) -> std::io::Result<Self> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(path)?;

            #[cfg(target_os = "linux")]
            {
                use nix::fcntl::{Flock, FlockArg};
                match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                    Ok(flock) => Ok(Self { _flock: flock }),
                    Err((_file, errno)) => Err(std::io::Error::from_raw_os_error(errno as i32)),
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                use std::os::unix::io::AsRawFd;
                // LOCK_EX = 2, LOCK_NB = 4 on BSD/macOS and Linux alike.
                let rc = unsafe { flock_ffi(file.as_raw_fd(), 2 | 4) };
                if rc == 0 {
                    Ok(Self { _file: file })
                } else {
                    Err(std::io::Error::last_os_error())
                }
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    impl Drop for Wlan0Lock {
        fn drop(&mut self) {
            use std::os::unix::io::AsRawFd;
            // LOCK_UN = 8. Best-effort; the fd close also drops the lock.
            unsafe {
                let _ = flock_ffi(self._file.as_raw_fd(), 8);
            }
        }
    }

    // On Linux nix::Flock's own Drop releases (LOCK_UN) and closes the fd.

    #[cfg(not(target_os = "linux"))]
    extern "C" {
        #[link_name = "flock"]
        fn flock_ffi(fd: i32, operation: i32) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::testing::ScriptedRunner;
    use crate::cmd::CmdOut;

    fn mgr(dir: &std::path::Path, runner: Arc<ScriptedRunner>) -> WifiClientManager {
        WifiClientManager::with_paths(
            "wlan0",
            runner,
            dir.join("ados-wlan0.lock"),
            dir.join("ap-was-enabled"),
            dir.join("ground-station-wifi-client.json"),
        )
    }

    #[test]
    fn flock_is_held_in_struct_and_blocks_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let r1 = Arc::new(ScriptedRunner::new());
        let mut m1 = mgr(dir.path(), r1);
        // Acquire on m1 → held.
        assert!(m1.acquire_lock());
        assert!(m1.holds_lock());

        // A second manager pointed at the SAME lock path cannot acquire while
        // m1 holds it. (Separate fd → flock contention is observed.)
        let r2 = Arc::new(ScriptedRunner::new());
        let mut m2 = mgr(dir.path(), r2);
        // Point m2 at m1's exact lock file.
        m2.lock_path = dir.path().join("ados-wlan0.lock");
        assert!(!m2.acquire_lock(), "second holder must fail while m1 holds");
        assert!(!m2.holds_lock());

        // Release m1 → m2 can now take it.
        m1.release_lock();
        assert!(!m1.holds_lock());
        assert!(m2.acquire_lock(), "lock is free after m1 released");
        assert!(m2.holds_lock());
    }

    #[test]
    fn ap_flag_transitions_write_read_clear() {
        let dir = tempfile::tempdir().unwrap();
        let m = mgr(dir.path(), Arc::new(ScriptedRunner::new()));
        // Absent → false.
        assert!(!m.read_ap_flag());
        // Write "1\n" → true, exact bytes.
        m.write_ap_flag(true);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ap-was-enabled")).unwrap(),
            "1\n"
        );
        assert!(m.read_ap_flag());
        // Write "0\n" → false.
        m.write_ap_flag(false);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("ap-was-enabled")).unwrap(),
            "0\n"
        );
        assert!(!m.read_ap_flag());
        // Clear unlinks.
        m.write_ap_flag(true);
        m.clear_ap_flag();
        assert!(!dir.path().join("ap-was-enabled").exists());
    }

    #[test]
    fn client_config_save_is_byte_exact_indent2_plus_newline() {
        let dir = tempfile::tempdir().unwrap();
        let m = mgr(dir.path(), Arc::new(ScriptedRunner::new()));
        m.save_client_config(&ClientConfig {
            enabled_on_boot: false,
            last_ssid: Some("MyAP".to_string()),
        });
        let body =
            std::fs::read_to_string(dir.path().join("ground-station-wifi-client.json")).unwrap();
        assert_eq!(
            body,
            "{\n  \"enabled_on_boot\": false,\n  \"last_ssid\": \"MyAP\"\n}\n"
        );
        // No torn tmp.
        assert!(!dir
            .path()
            .join("ground-station-wifi-client.json.tmp")
            .exists());
        // Round-trips.
        let loaded = m.load_client_config();
        assert_eq!(loaded.last_ssid.as_deref(), Some("MyAP"));
    }

    #[tokio::test]
    async fn join_refuses_when_ap_active_and_not_forced() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 0,
            stdout: "active\n".to_string(),
            stderr: String::new(),
        }); // is-active → active
        let mut m = mgr(dir.path(), runner);
        let res = m.join("SomeAP", Some("pw"), false).await;
        assert_eq!(res["joined"], false);
        assert_eq!(res["error"], "wlan0_busy_ap_active");
        // Did NOT take the lock on the refusal path.
        assert!(!m.holds_lock());
    }

    #[tokio::test]
    async fn failed_join_releases_lock_and_persists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 3,
            stdout: String::new(),
            stderr: String::new(),
        }); // is-active → inactive (rc!=0, stdout != "active")
        runner.push(CmdOut::failed(1, "no network with SSID")); // nmcli connect fails
        let mut m = mgr(dir.path(), runner);
        let res = m.join("BadAP", Some("pw"), false).await;
        assert_eq!(res["joined"], false);
        assert_eq!(res["error"], "no network with SSID");
        // Lock released after the failure.
        assert!(!m.holds_lock());
        // last_ssid was never persisted (config file absent).
        assert!(!dir.path().join("ground-station-wifi-client.json").exists());
    }

    #[tokio::test]
    async fn successful_join_keeps_lock_until_leave() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 3,
            stdout: String::new(),
            stderr: String::new(),
        }); // is-active → inactive
        runner.push(CmdOut::failed(0, "")); // nmcli --ask connect ok
        runner.push(CmdOut {
            rc: 0,
            stdout: "MyAP:wlan0\n".to_string(),
            stderr: String::new(),
        }); // connection show --active → active conn name on wlan0
        runner.push(CmdOut::failed(0, "")); // powersave nmcli
        runner.push(CmdOut::failed(0, "")); // powersave iw
                                            // status(): wifi list + ip addr + ip route.
        runner.push(CmdOut {
            rc: 0,
            stdout: "yes:MyAP:AA\\:BB\\:CC:70:WPA2\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut {
            rc: 0,
            stdout: "    inet 192.168.5.20/24 scope global wlan0\n".to_string(),
            stderr: String::new(),
        });
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 192.168.5.1 dev wlan0\n".to_string(),
            stderr: String::new(),
        });
        let m_runner = Arc::clone(&runner);
        let mut m = mgr(dir.path(), runner);
        let res = m.join("MyAP", Some("secret"), false).await;
        assert_eq!(res["joined"], true);
        assert_eq!(res["ip"], "192.168.5.20");
        // Lock is still held after a successful join.
        assert!(m.holds_lock());
        // last_ssid persisted.
        let cfg = m.load_client_config();
        assert_eq!(cfg.last_ssid.as_deref(), Some("MyAP"));

        // The passphrase is NEVER in any recorded argv (it would otherwise be
        // readable in /proc/<pid>/cmdline). It travels on stdin instead.
        let calls = m_runner.recorded();
        assert!(
            !calls.iter().any(|c| c.iter().any(|a| a == "secret")),
            "passphrase leaked into argv: {calls:?}"
        );
        // The connect call used --ask (interactive-secret mode) and did NOT
        // carry a `password` argument.
        let connect = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "connect"))
            .expect("a connect call was recorded");
        assert!(connect.iter().any(|a| a == "--ask"));
        assert!(!connect.iter().any(|a| a == "password"));
        // The secret WAS fed on stdin for that call (trailing newline included).
        let stdins = m_runner.recorded_stdins();
        assert!(
            stdins.iter().any(|s| s == b"secret\n"),
            "passphrase was not supplied on stdin"
        );
    }

    #[tokio::test]
    async fn open_network_join_sends_no_stdin_and_no_password_arg() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 3,
            stdout: String::new(),
            stderr: String::new(),
        }); // is-active → inactive
        runner.push(CmdOut::failed(0, "")); // nmcli connect ok (open, no secret)
        runner.push(CmdOut {
            rc: 0,
            stdout: "OpenNet:wlan0\n".to_string(),
            stderr: String::new(),
        }); // connection show --active
        runner.push(CmdOut::failed(0, "")); // powersave nmcli
        runner.push(CmdOut::failed(0, "")); // powersave iw
        runner.push(CmdOut {
            rc: 0,
            stdout: "yes:OpenNet:AA\\:BB\\:CC:70:\n".to_string(),
            stderr: String::new(),
        }); // wifi list
        runner.push(CmdOut {
            rc: 0,
            stdout: "    inet 192.168.9.5/24 scope global wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip addr
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 192.168.9.1 dev wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip route
        let m_runner = Arc::clone(&runner);
        let mut m = mgr(dir.path(), runner);
        let res = m.join("OpenNet", None, false).await;
        assert_eq!(res["joined"], true);
        // No `--ask`, no `password`, and no stdin payload for an open network.
        let connect = m_runner
            .recorded()
            .into_iter()
            .find(|c| c.iter().any(|a| a == "connect"))
            .expect("a connect call was recorded");
        assert!(!connect.iter().any(|a| a == "--ask"));
        assert!(!connect.iter().any(|a| a == "password"));
        assert!(
            m_runner.recorded_stdins().iter().all(|s| s.is_empty()),
            "open-network join must not feed any stdin"
        );
    }

    #[tokio::test]
    async fn powersave_runs_both_toggles_after_a_successful_connect() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 3,
            stdout: String::new(),
            stderr: String::new(),
        }); // is-active → inactive
        runner.push(CmdOut::failed(0, "")); // nmcli --ask connect ok
        runner.push(CmdOut {
            rc: 0,
            stdout: "HomeWifi:wlan0\n".to_string(),
            stderr: String::new(),
        }); // connection show --active
        runner.push(CmdOut::failed(0, "")); // powersave nmcli modify
        runner.push(CmdOut::failed(0, "")); // powersave iw
        runner.push(CmdOut {
            rc: 0,
            stdout: "yes:HomeWifi:AA\\:BB\\:CC:70:WPA2\n".to_string(),
            stderr: String::new(),
        }); // wifi list
        runner.push(CmdOut {
            rc: 0,
            stdout: "    inet 192.168.1.50/24 scope global wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip addr
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 192.168.1.1 dev wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip route
        let m_runner = Arc::clone(&runner);
        let mut m = mgr(dir.path(), runner);
        let res = m.join("HomeWifi", Some("secret"), false).await;
        assert_eq!(res["joined"], true);

        let calls = m_runner.recorded();
        // Both the connection-level (nmcli) and the runtime (iw) power-save
        // toggles fired.
        let nm_ps = vec![
            "nmcli".to_string(),
            "connection".to_string(),
            "modify".to_string(),
            "HomeWifi".to_string(),
            "802-11-wireless.powersave".to_string(),
            "2".to_string(),
        ];
        let iw_ps = vec![
            "iw".to_string(),
            "dev".to_string(),
            "wlan0".to_string(),
            "set".to_string(),
            "power_save".to_string(),
            "off".to_string(),
        ];
        assert!(calls.contains(&nm_ps), "nmcli powersave toggle missing");
        assert!(calls.contains(&iw_ps), "iw powersave toggle missing");

        // Both toggles come AFTER the connect (the radio must be associated
        // before NM has a connection to modify).
        let connect_idx = calls
            .iter()
            .position(|c| c.iter().any(|a| a == "connect"))
            .expect("a connect call");
        let iw_idx = calls.iter().position(|c| *c == iw_ps).expect("iw toggle");
        let nm_idx = calls.iter().position(|c| *c == nm_ps).expect("nm toggle");
        assert!(iw_idx > connect_idx, "iw toggle ran before connect");
        assert!(nm_idx > connect_idx, "nmcli toggle ran before connect");
    }

    #[tokio::test]
    async fn powersave_toggle_failure_is_nonfatal_to_the_join() {
        // A power-save toggle that fails (driver does not support it) must not
        // fail an otherwise-good join.
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 3,
            stdout: String::new(),
            stderr: String::new(),
        }); // is-active → inactive
        runner.push(CmdOut::failed(0, "")); // nmcli --ask connect ok
        runner.push(CmdOut {
            rc: 0,
            stdout: "HomeWifi:wlan0\n".to_string(),
            stderr: String::new(),
        }); // connection show --active
        runner.push(CmdOut::failed(1, "not supported")); // powersave nmcli FAILS
        runner.push(CmdOut::failed(1, "not supported")); // powersave iw FAILS
        runner.push(CmdOut {
            rc: 0,
            stdout: "yes:HomeWifi:AA\\:BB\\:CC:70:WPA2\n".to_string(),
            stderr: String::new(),
        }); // wifi list
        runner.push(CmdOut {
            rc: 0,
            stdout: "    inet 192.168.1.50/24 scope global wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip addr
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 192.168.1.1 dev wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip route
        let mut m = mgr(dir.path(), runner);
        let res = m.join("HomeWifi", Some("secret"), false).await;
        // The join still succeeds and the lock is held.
        assert_eq!(res["joined"], true);
        assert_eq!(res["ip"], "192.168.1.50");
        assert!(m.holds_lock());
    }

    #[tokio::test]
    async fn failed_connect_skips_powersave_entirely() {
        // When the connect itself fails, no power-save toggle is attempted (the
        // failure path restores any AP and releases the lock before returning).
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 3,
            stdout: String::new(),
            stderr: String::new(),
        }); // is-active → inactive (no AP to restore)
        runner.push(CmdOut::failed(1, "No network with SSID 'HomeWifi' found")); // connect FAILS
        let m_runner = Arc::clone(&runner);
        let mut m = mgr(dir.path(), runner);
        let res = m.join("HomeWifi", Some("secret"), false).await;
        assert_eq!(res["joined"], false);
        // Neither power-save toggle was issued.
        let calls = m_runner.recorded();
        assert!(
            !calls
                .iter()
                .any(|c| c.iter().any(|a| a == "802-11-wireless.powersave")),
            "nmcli powersave was issued after a failed connect"
        );
        assert!(
            !calls.iter().any(|c| c.iter().any(|a| a == "power_save")),
            "iw powersave was issued after a failed connect"
        );
        // And no status() probe ran either (no wifi list call recorded).
        assert!(
            !calls.iter().any(|c| c.iter().any(|a| a == "list")),
            "status() ran after a failed connect"
        );
    }

    #[tokio::test]
    async fn powersave_targets_resolved_active_connection_not_ssid() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new());
        runner.push(CmdOut {
            rc: 3,
            stdout: String::new(),
            stderr: String::new(),
        }); // is-active → inactive
        runner.push(CmdOut::failed(0, "")); // nmcli --ask connect ok
        runner.push(CmdOut {
            rc: 0,
            // NM auto-named the connection differently from the SSID.
            stdout: "MyAP 1:wlan0\nWired connection 1:eth0\n".to_string(),
            stderr: String::new(),
        }); // connection show --active
        runner.push(CmdOut::failed(0, "")); // powersave nmcli
        runner.push(CmdOut::failed(0, "")); // powersave iw
        runner.push(CmdOut {
            rc: 0,
            stdout: "yes:MyAP:AA\\:BB\\:CC:70:WPA2\n".to_string(),
            stderr: String::new(),
        }); // wifi list
        runner.push(CmdOut {
            rc: 0,
            stdout: "    inet 192.168.5.20/24 scope global wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip addr
        runner.push(CmdOut {
            rc: 0,
            stdout: "default via 192.168.5.1 dev wlan0\n".to_string(),
            stderr: String::new(),
        }); // ip route
        let m_runner = Arc::clone(&runner);
        let mut m = mgr(dir.path(), runner);
        let res = m.join("MyAP", Some("secret"), false).await;
        assert_eq!(res["joined"], true);
        // The `connection modify` call targets the resolved active name
        // ("MyAP 1"), not the SSID ("MyAP").
        let modify = m_runner
            .recorded()
            .into_iter()
            .find(|c| c.iter().any(|a| a == "modify"))
            .expect("a connection modify call was recorded");
        assert!(
            modify.iter().any(|a| a == "MyAP 1"),
            "powersave modify did not target the resolved connection name: {modify:?}"
        );
        assert!(!modify.iter().any(|a| a == "MyAP"));
    }
}
