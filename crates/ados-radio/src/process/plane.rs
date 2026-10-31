//! One wfb child process and its stats counters.

use super::*;

impl TxPlaneCounters {
    /// Fold one parsed stats line into the totals.
    pub fn record(&self, pkt: &crate::tx_liveness::TxPkt) {
        self.lines.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(pkt.bytes_in, Ordering::Relaxed);
        self.bytes_injected
            .fetch_add(pkt.bytes_injected, Ordering::Relaxed);
    }

    /// The totals as of now.
    pub fn totals(&self) -> TxPlaneTotals {
        TxPlaneTotals {
            lines: self.lines.load(Ordering::Relaxed),
            bytes_in: self.bytes_in.load(Ordering::Relaxed),
            bytes_injected: self.bytes_injected.load(Ordering::Relaxed),
        }
    }
}

impl WfbProcess {
    /// Spawn the **data-plane** `wfb_tx`. stderr → truncated log file; its
    /// stats stream feeds `stats`, the TX-health and receive-queue watchdogs'
    /// delta counters.
    pub async fn spawn_data_tx(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link_id: u32,
        stats: Arc<TxPlaneCounters>,
    ) -> std::io::Result<Self> {
        Self::spawn_tx_plane(
            &data_tx_args(iface, cfg, key_path, link_id),
            DATA_TX_LOG,
            stats,
        )
        .await
    }

    /// Spawn the **tx-control** `wfb_tx` (over-the-air HopAnnounce/PresenceBeacon
    /// transport). stderr → truncated log file; its stats stream feeds `stats`,
    /// the control-plane watchdog's transmit counter.
    pub async fn spawn_tx_control(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link_id: u32,
        stats: Arc<TxPlaneCounters>,
    ) -> std::io::Result<Self> {
        Self::spawn_tx_plane(
            &tx_control_args(iface, cfg, key_path, link_id),
            TX_CONTROL_LOG,
            stats,
        )
        .await
    }

    /// Spawn the **rx-control** `wfb_rx` (receives HopAck off the air → 5810).
    /// `link_id` is the ground station's — the shared uplink.
    ///
    /// stdout is PIPED because this receiver's `-l 1000` stats stream is the
    /// drone's only real link measurement (see [`rx_control_args`]); stderr
    /// still goes to its truncated log file. The caller MUST take the stdout
    /// handle and keep draining it — an unread pipe fills at 64 KiB and
    /// `wfb_rx` blocks in `fprintf(stdout)`, which would stall HopAck delivery.
    pub async fn spawn_rx_control(
        iface: &str,
        key_path: &Path,
        link_id: u32,
    ) -> std::io::Result<Self> {
        Self::spawn_in_group(
            "wfb_rx",
            &rx_control_args(iface, key_path, link_id),
            RX_CONTROL_LOG,
        )
        .await
    }

    /// Spawn the **auxiliary tx** `wfb_tx` (radio_id 2, application ingress).
    /// stderr → truncated log file; its stats stream feeds `stats`, the aux
    /// watchdog's delta counter.
    pub async fn spawn_aux_tx(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link_id: u32,
        stats: Arc<TxPlaneCounters>,
    ) -> std::io::Result<Self> {
        Self::spawn_tx_plane(
            &aux_tx_args(iface, cfg, key_path, link_id),
            AUX_TX_LOG,
            stats,
        )
        .await
    }

    /// Spawn the **auxiliary rx** `wfb_rx` (radio_id 3, re-emit decoded frames on
    /// 127.0.0.1:`aux_rx_port`). stderr → truncated log file. `link_id` is the
    /// ground station's — the shared aux uplink.
    ///
    /// stdout is piped and drained by an owned reader that folds the receiver's
    /// `-l 1000` stats into the uplink link block: this receiver carries the
    /// continuous GCS uplink, so it is the drone's busiest uplink measurement.
    pub async fn spawn_aux_rx(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link_id: u32,
        uplink: Arc<UplinkFeed>,
    ) -> std::io::Result<Self> {
        let mut p = Self::spawn_in_group(
            "wfb_rx",
            &aux_rx_args(iface, cfg, key_path, link_id),
            AUX_RX_LOG,
        )
        .await?;
        p.stats_task = p.take_stdout().map(|out| {
            tokio::spawn(stats_reader_loop(
                out,
                uplink,
                crate::link_quality::UplinkPlane::Aux,
                None,
            ))
        });
        Ok(p)
    }

    /// Take the child's stdout handle (for the stats reader). Returns `None` if
    /// stdout was not piped or already taken.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.take_stdout()
    }

    /// Spawn a `wfb_tx` plane with stdout piped and drained by an owned reader
    /// that folds every stats line into `stats`. An unread stdout pipe would
    /// fill at 64 KiB and block the transmitter in `fprintf(stdout)`.
    pub(super) async fn spawn_tx_plane(
        args: &[String],
        stderr_log: &str,
        stats: Arc<TxPlaneCounters>,
    ) -> std::io::Result<Self> {
        let mut p = Self::spawn_in_group("wfb_tx", args, stderr_log).await?;
        p.stats_task = p
            .take_stdout()
            .map(|out| tokio::spawn(tx_stats_reader_loop(out, stats)));
        Ok(p)
    }

    /// Spawn `program` with `args` as a process-group leader, stdout piped for
    /// the plane's stats reader and stderr appended to the plane's
    /// `stderr_log` file (emptied at spawn, capped by [`cap_plane_logs`]). The
    /// caller MUST take and drain stdout: an unread pipe fills at 64 KiB and
    /// the wfb binary then blocks in `fprintf(stdout)`.
    pub(super) async fn spawn_in_group(
        program: &str,
        args: &[String],
        stderr_log: &str,
    ) -> std::io::Result<Self> {
        let child = GroupChild::spawn(
            program,
            Spec {
                program,
                args,
                stdout: StdoutMode::Piped,
                stderr: StderrMode::AppendLog(stderr_log.into()),
            },
        )?;
        Ok(Self {
            child,
            stats_task: None,
        })
    }

    /// True if the process has not yet exited.
    pub fn is_running(&mut self) -> bool {
        self.child.is_running()
    }

    /// The OS PID, or `None` once the child has been reaped.
    pub fn pid(&self) -> Option<u32> {
        self.child.pid()
    }

    /// Kill the entire process group and wait for the child to exit.
    pub async fn kill(&mut self) {
        self.child.kill().await;
    }
}

impl Drop for WfbProcess {
    fn drop(&mut self) {
        // The `GroupChild` drop kills the group.
        if let Some(t) = self.stats_task.take() {
            t.abort();
        }
    }
}
