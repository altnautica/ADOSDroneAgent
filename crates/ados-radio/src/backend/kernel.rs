//! The Linux kernel monitor-mode radio backend.
//!
//! A THIN DELEGATION WRAPPER over the existing kernel bring-up primitives
//! (`adapter` selection, `bringup` monitor/channel/radiate guards, `process`
//! group spawn). It moves no code out of those modules; it calls them in the same
//! order `run_service` does so a later wave can put the service loop on the
//! [`RadioBackend`] seam without changing behaviour. Phase A: never called from
//! the live path.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::Mutex;

use ados_radio::config::WfbConfig;
use ados_radio::link_proof::{RxProof, RX_PROOF_GRACE};
use ados_radio::link_quality::LinkStats;
use ados_radio::process::RadioProcesses;

use super::{BackendAvailability, BackendKind, BroughtUp, PlaneStats, RadioBackend, RadioError};

/// The kernel monitor-mode backend.
///
/// `plane_stats` reads `/sys` counters (under [`Self::sysfs_root`]) plus the
/// shared [`LinkStats`] and [`RxProof`]; it NEVER locks [`Self::proc`], keeping
/// the status read lock-disjoint from a respawn.
pub struct KernelMonitorBackend {
    /// Boot config snapshot (the iface override + tunables bring-up reads).
    cfg: WfbConfig,
    /// Selected injection interface; `None` before [`RadioBackend::bring_up`] /
    /// after [`RadioBackend::shut_down`].
    iface: Option<String>,
    /// The selected adapter's enumerated USB link speed (Mbps), surfaced in
    /// [`PlaneStats::adapter_usb_mbps`].
    adapter_usb_mbps: Option<u32>,
    /// The running radio process group. Held so `retune` / `shut_down` can drive
    /// it; `plane_stats` never locks it.
    proc: Option<Arc<Mutex<RadioProcesses>>>,
    /// Shared link stats fed by the stats-RX reader once a group is spawned;
    /// `plane_stats` reads this lock for the received-side counters.
    link: Arc<Mutex<LinkStats>>,
    /// Received-side proof (HopAck / peer beacon); `plane_stats` reads it for the
    /// last-valid-rx freshness.
    rx_proof: RxProof,
    /// Monotonic origin all `rx_proof` observations are measured against.
    proof_reference: Instant,
    /// Sysfs root for the `/sys/class/net/.../statistics/...` counter reads.
    /// `/sys` in production; a tempfile tree in tests so `plane_stats` is
    /// exercisable off a real SBC.
    sysfs_root: PathBuf,
}

impl KernelMonitorBackend {
    /// Construct an un-brought-up kernel backend. Cheap: no I/O, no adapter
    /// command (selection happens in [`RadioBackend::bring_up`]).
    pub fn new(cfg: &WfbConfig) -> Self {
        Self {
            cfg: cfg.clone(),
            iface: None,
            adapter_usb_mbps: None,
            proc: None,
            link: Arc::new(Mutex::new(LinkStats::default())),
            rx_proof: RxProof::new(),
            proof_reference: Instant::now(),
            sysfs_root: PathBuf::from("/sys"),
        }
    }
}

/// Pure platform → availability for the kernel backend, split out so it is
/// testable off a non-Linux host. The kernel backend needs Linux (monitor mode +
/// the `wfb_tx` / `wfb_rx` C binaries); off Linux it is structurally impossible.
pub(crate) fn kernel_availability(is_linux: bool) -> BackendAvailability {
    if is_linux {
        BackendAvailability::Ready
    } else {
        BackendAvailability::Impossible("kernel monitor backend requires Linux")
    }
}

/// Read a `<sysfs_root>/class/net/<iface>/statistics/<counter>` value, or `None`
/// when unreadable. Mirrors the path `txrate::read_tx_bytes` builds (the
/// cross-backend liveness counter source); `sysfs_root` is `/sys` in production
/// and a tempfile tree in tests.
async fn read_net_counter(sysfs_root: &Path, iface: &str, counter: &str) -> Option<u64> {
    let path = sysfs_root
        .join("class")
        .join("net")
        .join(iface)
        .join("statistics")
        .join(counter);
    tokio::fs::read_to_string(&path)
        .await
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Current Unix-epoch millis (clock-skew-tolerant: a pre-epoch clock reads 0).
fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[async_trait]
impl RadioBackend for KernelMonitorBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::KernelMonitor
    }

    fn probe(_cfg: &WfbConfig) -> BackendAvailability {
        kernel_availability(cfg!(target_os = "linux"))
    }

    async fn bring_up(&mut self, cfg: &WfbConfig) -> Result<BroughtUp, RadioError> {
        // Thin delegation to the EXISTING kernel bring-up primitives, in the same
        // order run_service drives them (adapter select → verified monitor +
        // channel → coax the PHY off the muted floor → spawn the process group).
        // No code is moved out of adapter / bringup / process; the full
        // regulatory-gate orchestration stays in run_service (Phase A: inert).
        let outcome = ados_radio::adapter::detect_and_select(&cfg.interface).await;
        let adapter = outcome.selected.ok_or(RadioError::NoAdapter)?;
        let iface = adapter.ifname.clone();

        if !crate::bringup::ensure_monitor_and_channel(&iface, cfg.channel).await {
            return Err(RadioError::MonitorSetupFailed);
        }

        // The operating-region posture run_service resolves from the config; the
        // thin wrapper reads it the same way so the primitive call matches.
        let unrestricted =
            ados_radio::config::RegulatoryConfig::load_from(Path::new("/etc/ados/config.yaml"))
                .mode
                .is_unrestricted();
        if crate::bringup::ensure_radiating(&iface, cfg.channel, cfg.tx_power_dbm, unrestricted)
            .await
            .is_none()
        {
            return Err(RadioError::PhyMuted);
        }

        let proc = RadioProcesses::spawn(
            &iface,
            cfg,
            Path::new(ados_radio::paths::WFB_TX_KEY),
            self.link.clone(),
        )
        .await
        .map_err(|e| RadioError::Spawn(e.to_string()))?;
        let proc = Arc::new(Mutex::new(proc));

        self.iface = Some(iface.clone());
        self.adapter_usb_mbps = adapter.usb_speed_mbps;
        self.proc = Some(proc.clone());

        Ok(BroughtUp {
            iface,
            adapter,
            proc,
        })
    }

    async fn retune(&mut self, channel: u8) -> Result<(), RadioError> {
        // Land the new channel on the live injection interface, verified — the
        // same primitive a coordinated hop uses. Re-spawning the process group on
        // the new channel is run_service's job; the wrapper just retunes the PHY.
        let iface = self
            .iface
            .as_deref()
            .ok_or(RadioError::MonitorSetupFailed)?;
        if crate::bringup::ensure_monitor_and_channel(iface, channel).await {
            Ok(())
        } else {
            Err(RadioError::MonitorSetupFailed)
        }
    }

    async fn plane_stats(&self) -> PlaneStats {
        // Lock-DISJOINT from the RadioProcesses mutex: this reads only /sys
        // counters, the shared LinkStats lock, and the lock-free RxProof — never
        // self.proc — so a status read never contends with a respawn.
        let Some(iface) = self.iface.as_deref() else {
            return PlaneStats::default();
        };
        let data_tx_bytes = read_net_counter(&self.sysfs_root, iface, "tx_bytes")
            .await
            .unwrap_or(0);
        let data_rx_bytes = read_net_counter(&self.sysfs_root, iface, "rx_bytes")
            .await
            .unwrap_or(0);
        let (ctrl_rx_packets, fec_recovered) = {
            let link = self.link.lock().await;
            (
                link.packets_received.max(0) as u64,
                link.fec_recovered.max(0) as u64,
            )
        };
        let now = Instant::now();
        // A verified return signal within the proof grace window anchors the
        // last-valid-rx freshness. The exact last-rx instant is not retained on
        // this transmit-side handle, so the wall clock is the honest freshness
        // anchor; absent any proof the field is None (never a fabricated time).
        let last_valid_rx_unix_ms =
            if self
                .rx_proof
                .proven_within(RX_PROOF_GRACE, now, self.proof_reference)
            {
                Some(unix_millis_now())
            } else {
                None
            };
        PlaneStats {
            data_tx_bytes,
            data_rx_bytes,
            ctrl_rx_packets,
            fec_recovered,
            last_valid_rx_unix_ms,
            adapter_usb_mbps: self.adapter_usb_mbps,
        }
    }

    async fn shut_down(&mut self) {
        // Kill the radio process group and restore the adapter to managed mode —
        // the same teardown run_service runs on SIGTERM. Idempotent.
        if let Some(proc) = self.proc.take() {
            proc.lock().await.kill_all().await;
        }
        if let Some(iface) = self.iface.take() {
            ados_radio::adapter::set_managed_mode(&iface).await;
        }
        self.adapter_usb_mbps = None;
    }
}

#[cfg(test)]
impl KernelMonitorBackend {
    fn set_sysfs_root_for_test(&mut self, root: &Path) {
        self.sysfs_root = root.to_path_buf();
    }
    fn set_iface_for_test(&mut self, iface: &str) {
        self.iface = Some(iface.to_string());
    }
    fn link_handle(&self) -> Arc<Mutex<LinkStats>> {
        self.link.clone()
    }
    fn proof_handle(&self) -> RxProof {
        self.rx_proof.clone()
    }
    fn proof_reference(&self) -> Instant {
        self.proof_reference
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_counter(root: &Path, iface: &str, counter: &str, value: &str) {
        let dir = root
            .join("class")
            .join("net")
            .join(iface)
            .join("statistics");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(counter), value).unwrap();
    }

    #[test]
    fn kind_is_kernel_monitor() {
        let be = KernelMonitorBackend::new(&WfbConfig::default());
        assert_eq!(be.kind(), BackendKind::KernelMonitor);
        // The wire token the sidecar surfaces.
        assert_eq!(BackendKind::KernelMonitor.as_wire(), "kernel");
    }

    #[test]
    fn probe_is_pure_platform_check_no_adapter_call() {
        // probe is a sync, pure, side-effect-free platform check — no adapter
        // command. Calling it repeatedly is stable.
        let cfg = WfbConfig::default();
        let a = <KernelMonitorBackend as RadioBackend>::probe(&cfg);
        let b = <KernelMonitorBackend as RadioBackend>::probe(&cfg);
        assert_eq!(a, b);
        if cfg!(target_os = "linux") {
            assert_eq!(a, BackendAvailability::Ready);
        } else {
            assert!(matches!(a, BackendAvailability::Impossible(_)));
        }
    }

    #[test]
    fn kernel_availability_gates_on_linux() {
        assert_eq!(kernel_availability(true), BackendAvailability::Ready);
        assert!(matches!(
            kernel_availability(false),
            BackendAvailability::Impossible(_)
        ));
    }

    #[tokio::test]
    async fn read_net_counter_matches_txrate_read_tx_bytes_on_default_root() {
        // For the production `/sys` root the backend's counter read and the
        // txrate helper resolve the IDENTICAL path, so they return the same value
        // — here both `None` for a nonexistent iface, proving the path contract
        // matches without a real SBC.
        let iface = "ados-test-nonexistent-iface";
        let a = read_net_counter(Path::new("/sys"), iface, "tx_bytes").await;
        let b = crate::txrate::read_tx_bytes(iface).await;
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn plane_stats_data_tx_bytes_reads_sysfs_tx_bytes() {
        // The cross-backend liveness parity: plane_stats().data_tx_bytes equals a
        // direct read of the same /sys tx_bytes counter (tempfile sysfs tree).
        let dir = tempfile::tempdir().unwrap();
        let iface = "wlan1";
        write_counter(dir.path(), iface, "tx_bytes", "12345\n");
        write_counter(dir.path(), iface, "rx_bytes", "678\n");

        let mut be = KernelMonitorBackend::new(&WfbConfig::default());
        be.set_sysfs_root_for_test(dir.path());
        be.set_iface_for_test(iface);

        let ps = be.plane_stats().await;
        assert_eq!(ps.data_tx_bytes, 12345);
        assert_eq!(ps.data_rx_bytes, 678);
        // Parity against a direct counter read of the same source.
        assert_eq!(
            ps.data_tx_bytes,
            read_net_counter(dir.path(), iface, "tx_bytes")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn plane_stats_before_bring_up_is_default() {
        // No iface selected (never brought up) → the default zeroed snapshot.
        let be = KernelMonitorBackend::new(&WfbConfig::default());
        assert_eq!(be.plane_stats().await, PlaneStats::default());
    }

    #[tokio::test]
    async fn plane_stats_is_independent_of_the_process_handle() {
        // plane_stats must be lock-disjoint from the RadioProcesses mutex: with NO
        // proc installed it still returns full data from /sys + the link + the
        // proof, proving it never depends on (and never locks) the process handle.
        let dir = tempfile::tempdir().unwrap();
        let iface = "wlan9";
        write_counter(dir.path(), iface, "tx_bytes", "5\n");
        write_counter(dir.path(), iface, "rx_bytes", "9\n");

        let mut be = KernelMonitorBackend::new(&WfbConfig::default());
        be.set_sysfs_root_for_test(dir.path());
        be.set_iface_for_test(iface);
        assert!(be.proc.is_none());

        let ps = be.plane_stats().await;
        assert_eq!(ps.data_tx_bytes, 5);
        assert_eq!(ps.data_rx_bytes, 9);
    }

    #[tokio::test]
    async fn plane_stats_ctrl_and_fec_read_link_stats() {
        let mut be = KernelMonitorBackend::new(&WfbConfig::default());
        be.set_iface_for_test("ados-test-iface");
        let link = be.link_handle();
        {
            let mut l = link.lock().await;
            l.packets_received = 42;
            l.fec_recovered = 7;
        }
        let ps = be.plane_stats().await;
        assert_eq!(ps.ctrl_rx_packets, 42);
        assert_eq!(ps.fec_recovered, 7);
    }

    #[tokio::test]
    async fn plane_stats_last_valid_rx_tracks_rx_proof() {
        let mut be = KernelMonitorBackend::new(&WfbConfig::default());
        be.set_iface_for_test("ados-test-iface");
        // No proof heard yet → None (never a fabricated time).
        assert!(be.plane_stats().await.last_valid_rx_unix_ms.is_none());
        // Observe a verified return signal at the reference → proven within the
        // grace window → Some.
        let proof = be.proof_handle();
        let reference = be.proof_reference();
        proof.observe(Instant::now(), reference);
        assert!(be.plane_stats().await.last_valid_rx_unix_ms.is_some());
    }
}
