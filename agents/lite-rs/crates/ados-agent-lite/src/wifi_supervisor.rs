//! Wi-Fi AP fallback supervisor for first-boot setup without UART.
//!
//! On a freshly flashed image the operator may not have a USB-UART cable
//! handy. This module watches `wlan0` for a `wpa_supplicant` association
//! after boot and, after 30 s of consecutive no-association, stands up a
//! hostapd + dnsmasq pair on the same interface with SSID `ados-XXXX`
//! (last 4 hex chars of the wlan0 MAC, lowercase) and the agent's pair
//! code as the WPA2 passphrase. The operator joins the soft-AP, opens
//! `http://192.168.4.1:8080`, and configures the real network through
//! the setup webapp. Once the webapp writes a fresh
//! `/etc/wpa_supplicant/wpa_supplicant.conf` the supervisor tears the AP
//! down, hands the radio back to `wpa_supplicant`, and resumes watching.
//!
//! On non-Linux targets the public surface compiles to a no-op stub so
//! the developer cross-compile loop on macOS / Windows hosts is not
//! blocked by Linux-specific tooling assumptions.

#![allow(dead_code)]

use tokio::task::JoinHandle;

/// Public handle returned by [`WifiSupervisor::spawn`]. Owns the
/// background tokio task; dropping it cancels the task. The Linux path
/// guarantees that any spawned `hostapd` / `dnsmasq` children are
/// killed before the task returns (see the inner `Drop` impl on the
/// `RunningAp` guard).
#[must_use = "drop the handle to stop the supervisor task"]
pub struct WifiSupervisor {
    handle: JoinHandle<()>,
}

impl WifiSupervisor {
    /// Spawn the supervisor. `pair_code` becomes the WPA2 passphrase if
    /// the AP fires; `mac_suffix` is the last 4 hex chars of the wlan0
    /// MAC and forms the SSID.
    ///
    /// Returns immediately with a handle. The actual probe loop runs on
    /// a background tokio task on the current runtime.
    pub fn spawn(pair_code: String, mac_suffix: String) -> Self {
        let handle = tokio::spawn(async move {
            run(pair_code, mac_suffix).await;
        });
        Self { handle }
    }

    /// Abort the background task. Called automatically on `Drop`.
    pub fn abort(&self) {
        self.handle.abort();
    }
}

impl Drop for WifiSupervisor {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

#[cfg(not(target_os = "linux"))]
async fn run(_pair_code: String, _mac_suffix: String) {
    // No-op stub: the AP-fallback path depends on Linux-only tooling
    // (`ip`, `iw`, `hostapd`, `dnsmasq`, `wpa_supplicant`). Cross-compile
    // builds on macOS and Windows hosts get a quiet supervisor that
    // immediately returns; the Linux target carries the real loop.
    tracing::debug!("wifi_supervisor: non-linux target, supervisor disabled");
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::Path;
    use std::time::Duration;

    use tokio::process::{Child, Command};
    use tokio::time::{sleep, Instant};

    /// Polling cadence for the association probe. Five seconds keeps the
    /// kernel + iw cost negligible and gives the operator a smooth
    /// "association seen" → "AP comes up" transition without spinning.
    const PROBE_INTERVAL: Duration = Duration::from_secs(5);

    /// Time after boot (or after the last association loss) at which the
    /// AP fires when no link has been seen.
    const FALLBACK_AFTER: Duration = Duration::from_secs(30);

    /// Backoff after a failed `hostapd` / `dnsmasq` spawn before retrying
    /// the AP path. Long enough that a misconfigured rootfs (missing
    /// binary, broken template) doesn't pin the supervisor in a tight
    /// loop, short enough that an operator who unplugs / replugs a USB
    /// dongle doesn't wait minutes.
    const SPAWN_RETRY_BACKOFF: Duration = Duration::from_secs(60);

    /// Path the operator's setup webapp writes when they pick a real
    /// Wi-Fi network from the soft-AP. The supervisor watches this path;
    /// when it appears (or its mtime moves), the AP comes down and
    /// `wpa_supplicant` is invoked against the new file.
    const WPA_SUPPLICANT_CONF: &str = "/etc/wpa_supplicant/wpa_supplicant.conf";

    /// Hostapd template shipped under the rootfs overlay. The supervisor
    /// reads the template, substitutes `__SSID__` and `__PASSPHRASE__`,
    /// and writes the materialized config to `RUNTIME_HOSTAPD_CONF`.
    const HOSTAPD_TEMPLATE: &str = "/etc/ados/ap-fallback/hostapd.conf.template";
    const DNSMASQ_CONF: &str = "/etc/ados/ap-fallback/dnsmasq.conf";

    /// Materialized hostapd config path. Lives on tmpfs so the
    /// pair-code-bearing config never touches persistent storage.
    const RUNTIME_HOSTAPD_CONF: &str = "/run/ados/hostapd.conf";

    /// Static IP the soft-AP serves on. Matches `dnsmasq.conf`.
    const AP_CIDR: &str = "192.168.4.1/24";
    const WLAN_IFACE: &str = "wlan0";

    /// Pairing JSON path. Re-read on every AP fire so an operator who
    /// rotates the pair code mid-session sees the new code on the next
    /// AP cycle. Mirrors the binary's existing `/etc/ados/pairing.json`
    /// default.
    const PAIRING_JSON: &str = "/etc/ados/pairing.json";

    pub(super) async fn run(initial_pair_code: String, mac_suffix: String) {
        let mac_suffix = mac_suffix.to_lowercase();
        let ssid = format!("ados-{}", mac_suffix);

        // Outer loop: probe → AP-fire → AP-teardown → probe again. The
        // AP-fire arm only runs when association has been absent for at
        // least FALLBACK_AFTER seconds.
        let mut current_pair_code = initial_pair_code;
        loop {
            if wait_for_no_association(FALLBACK_AFTER).await {
                // Pair code may have changed since boot if the operator
                // ran `ados pair --autogen` over UART before the AP
                // window. Re-read pairing.json best-effort; fall back to
                // the boot-time code if the file is missing or the
                // current pairing_code field is unset.
                if let Some(refreshed) = read_pairing_code(PAIRING_JSON) {
                    if !refreshed.is_empty() {
                        current_pair_code = refreshed;
                    }
                }

                if current_pair_code.is_empty() {
                    tracing::warn!(
                        "wifi_supervisor: pair code is empty; refusing to bring up an open AP. \
                         Re-run `ados pair --autogen` to seed pairing.json and retry."
                    );
                    sleep(SPAWN_RETRY_BACKOFF).await;
                    continue;
                }

                match start_ap(&ssid, &current_pair_code).await {
                    Ok(mut running) => {
                        tracing::info!(
                            ssid = %ssid,
                            ip = "192.168.4.1",
                            passphrase = %current_pair_code,
                            "AP fallback active"
                        );
                        running.watch_for_handoff().await;
                        // Drop runs hostapd.kill() + dnsmasq.kill().
                        drop(running);
                        // After teardown, hand the radio back to
                        // wpa_supplicant if a config exists.
                        if Path::new(WPA_SUPPLICANT_CONF).exists() {
                            spawn_wpa_supplicant_async().await;
                        }
                    }
                    Err(e) => {
                        tracing::error!(
                            error = %e,
                            "wifi_supervisor: AP spawn failed; backing off"
                        );
                        sleep(SPAWN_RETRY_BACKOFF).await;
                    }
                }
            }
        }
    }

    /// Block until the wlan0 interface has had no association for at
    /// least `threshold` seconds. Returns `true` once the threshold is
    /// crossed. The function never returns `false` (an associated state
    /// just resets the timer and keeps probing) but the bool keeps the
    /// caller's outer loop expressive.
    async fn wait_for_no_association(threshold: Duration) -> bool {
        let mut first_unassociated_at: Option<Instant> = None;
        loop {
            let associated = is_associated().await;
            if associated {
                first_unassociated_at = None;
            } else {
                let started = first_unassociated_at.get_or_insert_with(Instant::now);
                if started.elapsed() >= threshold {
                    return true;
                }
            }
            sleep(PROBE_INTERVAL).await;
        }
    }

    /// Probe whether wlan0 is currently associated. Two signals must
    /// agree: (a) `/proc/net/wireless` carries a `wlan*` line, AND (b)
    /// `iw dev wlan0 link` reports a non-empty `Connected to` line. Some
    /// drivers populate `/proc/net/wireless` with the interface even
    /// when down, so the iw probe disambiguates.
    async fn is_associated() -> bool {
        let proc_present = match tokio::fs::read_to_string("/proc/net/wireless").await {
            Ok(text) => text.lines().any(|l| l.trim_start().starts_with("wlan")),
            Err(_) => false,
        };
        if !proc_present {
            return false;
        }

        // `iw dev wlan0 link` exits 0 with body "Not connected." when
        // the interface exists but isn't associated, and exits 0 with a
        // body that begins "Connected to <bssid>" when it is. Use stdout
        // text inspection rather than exit code.
        let iw_path = resolve_bin(&[
            "/usr/sbin/iw",
            "/sbin/iw",
            "/usr/bin/iw",
        ]);
        if let Some(iw) = iw_path {
            if let Ok(out) = tokio::process::Command::new(iw)
                .args(["dev", WLAN_IFACE, "link"])
                .output()
                .await
            {
                if let Ok(text) = String::from_utf8(out.stdout) {
                    let connected = text
                        .lines()
                        .any(|l| l.trim_start().starts_with("Connected to"));
                    return connected;
                }
            }
        }

        // Fall back to `ip link show wlan0`: presence of an `UP` flag
        // is a weak signal but better than failing closed when iw is
        // missing. A driver that says UP but is not associated will
        // simply trigger the AP fallback after the threshold, which is
        // the correct behavior on a fresh image with no networks
        // configured.
        if let Some(ip) = resolve_bin(&["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip"]) {
            if let Ok(out) = tokio::process::Command::new(ip)
                .args(["link", "show", WLAN_IFACE])
                .output()
                .await
            {
                if let Ok(text) = String::from_utf8(out.stdout) {
                    return text.contains("state UP");
                }
            }
        }
        false
    }

    /// Stand up the soft-AP. Returns a [`RunningAp`] guard whose `Drop`
    /// kills both children. Errors propagate as `anyhow::Error` so the
    /// outer loop can log and back off without unwrapping.
    async fn start_ap(ssid: &str, passphrase: &str) -> anyhow::Result<RunningAp> {
        // Materialize the hostapd config. Read the template from the
        // overlay path and substitute SSID + passphrase.
        let template = tokio::fs::read_to_string(HOSTAPD_TEMPLATE)
            .await
            .map_err(|e| anyhow::anyhow!("read hostapd template: {e}"))?;
        let materialized = template
            .replace("__SSID__", ssid)
            .replace("__PASSPHRASE__", passphrase);

        // Ensure /run/ados exists; first-boot service should have made
        // it but the supervisor can race with that. mkdir -p semantics.
        if let Some(parent) = Path::new(RUNTIME_HOSTAPD_CONF).parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        write_secret_file(RUNTIME_HOSTAPD_CONF, &materialized).await?;

        // Bring wlan0 up with a static IP. Idempotent: re-adding an
        // existing address gives EEXIST which we tolerate.
        let ip_bin = resolve_bin(&["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip"])
            .ok_or_else(|| anyhow::anyhow!("ip binary not found in /usr/sbin, /sbin, /usr/bin"))?;
        let _ = Command::new(ip_bin)
            .args(["addr", "add", AP_CIDR, "dev", WLAN_IFACE])
            .status()
            .await;
        let status = Command::new(ip_bin)
            .args(["link", "set", WLAN_IFACE, "up"])
            .status()
            .await
            .map_err(|e| anyhow::anyhow!("ip link set wlan0 up: {e}"))?;
        if !status.success() {
            return Err(anyhow::anyhow!(
                "ip link set wlan0 up exited with {:?}",
                status.code()
            ));
        }

        // Spawn hostapd. Resolve the binary via the absolute-path
        // allowlist so a subverted PATH cannot redirect this.
        let hostapd_bin = resolve_bin(&["/usr/sbin/hostapd", "/sbin/hostapd"])
            .ok_or_else(|| anyhow::anyhow!("hostapd binary not found"))?;
        let hostapd_child = Command::new(hostapd_bin)
            .arg(RUNTIME_HOSTAPD_CONF)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn hostapd: {e}"))?;

        // Spawn dnsmasq.
        let dnsmasq_bin = resolve_bin(&["/usr/sbin/dnsmasq", "/sbin/dnsmasq"])
            .ok_or_else(|| anyhow::anyhow!("dnsmasq binary not found"))?;
        let dnsmasq_child = Command::new(dnsmasq_bin)
            .args(["-C", DNSMASQ_CONF, "-k"])
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn dnsmasq: {e}"))?;

        Ok(RunningAp {
            hostapd: Some(hostapd_child),
            dnsmasq: Some(dnsmasq_child),
        })
    }

    /// Guard for a running AP. Holds both child handles. Dropping kills
    /// them; the explicit teardown helper awaits the kill so callers can
    /// surface any wait error.
    pub(super) struct RunningAp {
        hostapd: Option<Child>,
        dnsmasq: Option<Child>,
    }

    impl RunningAp {
        /// Block until either (a) the operator's setup webapp writes a
        /// fresh `/etc/wpa_supplicant/wpa_supplicant.conf`, or (b) one
        /// of the AP children exits unexpectedly. Either condition is
        /// the cue to tear down and return to the probe loop.
        async fn watch_for_handoff(&mut self) {
            // Snapshot the wpa_supplicant.conf mtime at AP start so we
            // detect a freshly-written file even if it existed (empty
            // or stale) before fallback fired.
            let baseline = tokio::fs::metadata(WPA_SUPPLICANT_CONF)
                .await
                .ok()
                .and_then(|m| m.modified().ok());

            loop {
                // (a) Did the setup webapp drop a new config?
                let current = tokio::fs::metadata(WPA_SUPPLICANT_CONF)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok());
                if let Some(now) = current {
                    if baseline.map(|b| now > b).unwrap_or(true) {
                        tracing::info!(
                            "wifi_supervisor: detected fresh wpa_supplicant.conf; tearing AP down"
                        );
                        return;
                    }
                }

                // (b) Did either child die? hostapd.try_wait() returns
                // Ok(Some(_)) once the process has exited; treat that as
                // a signal to tear down so the outer loop logs and backs
                // off rather than holding a half-dead AP.
                if let Some(child) = self.hostapd.as_mut() {
                    if let Ok(Some(status)) = child.try_wait() {
                        tracing::warn!(
                            ?status,
                            "wifi_supervisor: hostapd exited; tearing AP down"
                        );
                        return;
                    }
                }
                if let Some(child) = self.dnsmasq.as_mut() {
                    if let Ok(Some(status)) = child.try_wait() {
                        tracing::warn!(
                            ?status,
                            "wifi_supervisor: dnsmasq exited; tearing AP down"
                        );
                        return;
                    }
                }
                sleep(Duration::from_secs(2)).await;
            }
        }
    }

    impl Drop for RunningAp {
        fn drop(&mut self) {
            // Best-effort synchronous kill on drop. start_kill schedules
            // a SIGKILL without awaiting; the kill_on_drop flag set on
            // the spawn ensures the kernel reaps the child even if the
            // tokio runtime tears down before the wait completes.
            if let Some(mut child) = self.hostapd.take() {
                let _ = child.start_kill();
            }
            if let Some(mut child) = self.dnsmasq.take() {
                let _ = child.start_kill();
            }
            // Best-effort flush of the static IP so a subsequent
            // wpa_supplicant connection is not confused by a stale
            // 192.168.4.1 alias remaining on wlan0.
            if let Some(ip) = resolve_bin(&["/usr/sbin/ip", "/sbin/ip", "/usr/bin/ip"]) {
                let _ = std::process::Command::new(ip)
                    .args(["addr", "del", AP_CIDR, "dev", WLAN_IFACE])
                    .status();
            }
        }
    }

    /// Spawn `wpa_supplicant -B -i wlan0 -c <conf>` and return. The
    /// daemon backgrounds itself; the supervisor returns to the probe
    /// loop and watches for association on the next tick.
    async fn spawn_wpa_supplicant_async() {
        let Some(bin) = resolve_bin(&[
            "/usr/sbin/wpa_supplicant",
            "/sbin/wpa_supplicant",
            "/usr/bin/wpa_supplicant",
        ]) else {
            tracing::warn!(
                "wifi_supervisor: wpa_supplicant binary not found; cannot hand radio back"
            );
            return;
        };
        let status = Command::new(bin)
            .args(["-B", "-i", WLAN_IFACE, "-c", WPA_SUPPLICANT_CONF])
            .status()
            .await;
        match status {
            Ok(s) if s.success() => {
                tracing::info!("wifi_supervisor: wpa_supplicant respawned with operator config");
            }
            Ok(s) => {
                tracing::warn!(?s, "wifi_supervisor: wpa_supplicant exited non-zero");
            }
            Err(e) => {
                tracing::warn!(error = %e, "wifi_supervisor: wpa_supplicant spawn failed");
            }
        }
    }

    /// Read pairing.json and return the current pairing_code field, if
    /// any. Best-effort: any IO or JSON error returns `None` and the
    /// caller falls back to the boot-time pair code.
    fn read_pairing_code(path: &str) -> Option<String> {
        let bytes = std::fs::read(path).ok()?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        value
            .get("pairing_code")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Resolve a binary against an absolute-path allowlist. Returns the
    /// first existing path or `None`. Mirrors the discipline used by
    /// the binary's `resolve_ip_binary` helper so a subverted `$PATH`
    /// cannot redirect the AP-fallback path.
    fn resolve_bin(candidates: &[&'static str]) -> Option<&'static str> {
        for candidate in candidates {
            if Path::new(candidate).exists() {
                return Some(*candidate);
            }
        }
        None
    }

    /// Write a sensitive file (the materialized hostapd config carrying
    /// the WPA2 passphrase) with mode 0600 atomically. Uses a tmp +
    /// rename pattern so a partial write never reaches readers.
    async fn write_secret_file(path: &str, contents: &str) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let parent = Path::new(path)
            .parent()
            .ok_or_else(|| anyhow::anyhow!("path has no parent: {path}"))?;
        let tmp = parent.join(format!(
            ".{}.tmp",
            Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("hostapd.conf")
        ));
        let bytes = contents.as_bytes().to_vec();
        let tmp_path = tmp.clone();
        // Open + write inside a blocking task because std::fs::OpenOptions
        // is sync; a tokio::task::spawn_blocking keeps the supervisor
        // task non-blocking and the file is small (~600 bytes).
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp_path)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("join blocking write: {e}"))?
        .map_err(|e| anyhow::anyhow!("write tmp hostapd config: {e}"))?;
        tokio::fs::rename(&tmp, path)
            .await
            .map_err(|e| anyhow::anyhow!("rename hostapd config into place: {e}"))?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
async fn run(pair_code: String, mac_suffix: String) {
    linux::run(pair_code, mac_suffix).await;
}
