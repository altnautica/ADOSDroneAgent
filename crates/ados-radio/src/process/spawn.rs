//! Spawning the radio group and reading its state.

use super::*;

impl RadioProcesses {
    /// Spawn the data plane + both control planes, and the link-stats reader on
    /// the rx-control receiver's stats stream.
    ///
    /// The two fleet link ids are derived once from `cfg` here and retained: the
    /// transmit side keys to this drone's own slot, the two uplink receivers key
    /// to the ground station's shared slot 0.
    pub async fn spawn(
        iface: &str,
        cfg: &WfbConfig,
        key_path: &Path,
        link: std::sync::Arc<tokio::sync::Mutex<crate::link_quality::LinkStats>>,
    ) -> std::io::Result<Self> {
        let own_link_id = crate::config::link_id(cfg.fleet_id, cfg.fleet_slot);
        let uplink_link_id = crate::config::link_id(cfg.fleet_id, crate::config::SLOT_GROUND);
        let data_stats = Arc::new(TxPlaneCounters::default());
        let control_stats = Arc::new(TxPlaneCounters::default());
        let rx_stats_lines = Arc::new(AtomicU64::new(0));
        let data_tx =
            WfbProcess::spawn_data_tx(iface, cfg, key_path, own_link_id, data_stats.clone())
                .await?;
        let tx_control =
            WfbProcess::spawn_tx_control(iface, cfg, key_path, own_link_id, control_stats.clone())
                .await?;
        let mut rx_control = WfbProcess::spawn_rx_control(iface, key_path, uplink_link_id).await?;
        let uplink = UplinkFeed::new(link);
        let stats_reader =
            Self::spawn_stats_reader(&mut rx_control, uplink.clone(), rx_stats_lines.clone());

        tracing::info!(
            fleet_id = cfg.fleet_id,
            fleet_slot = cfg.fleet_slot,
            own_link_id,
            uplink_link_id,
            "wfb_fleet_identity"
        );

        Ok(Self {
            data_tx,
            tx_control,
            rx_control,
            stats_reader,
            uplink,
            data_stats,
            control_stats,
            aux_stats: Arc::new(TxPlaneCounters::default()),
            rx_stats_lines,
            // Safe-by-default: the auxiliary stream never starts at boot. It is
            // brought up only by an explicit open_aux_stream call.
            aux_tx: None,
            aux_rx: None,
            aux_settings: None,
            iface: iface.to_string(),
            tx_key_path: key_path.to_path_buf(),
            own_link_id,
            uplink_link_id,
            data_fec_k: cfg.fec_k,
            data_fec_n: cfg.fec_n,
            data_mcs_index: cfg.mcs_index,
            applies: ApplyCounters::default(),
        })
    }

    /// Take the rx-control receiver's stdout and spawn the reader that folds its
    /// `-l 1000` stats stream into the uplink link block.
    ///
    /// The handle MUST be taken, not left: that pipe has a 64 KiB kernel buffer
    /// and `wfb_rx` prints its stats with a blocking `fprintf(stdout)`, so an
    /// unread stream would eventually wedge the receiver that also delivers
    /// HopAck. Shared by the initial spawn and the whole-group respawn so the
    /// two can never diverge on which stream the link block is measured from.
    pub(super) fn spawn_stats_reader(
        rx_control: &mut WfbProcess,
        uplink: Arc<UplinkFeed>,
        rx_stats_lines: Arc<AtomicU64>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        match rx_control.take_stdout() {
            Some(out) => Some(tokio::spawn(stats_reader_loop(
                out,
                uplink,
                crate::link_quality::UplinkPlane::Control,
                Some(rx_stats_lines),
            ))),
            None => {
                tracing::warn!("rx_control_stdout_unavailable: control plane stays unmeasured");
                None
            }
        }
    }

    /// The data-plane PID, for the logs.
    pub fn data_tx_pid(&self) -> Option<u32> {
        self.data_tx.pid()
    }

    /// The first plane of the group that has exited, or `None` while all three
    /// run. A cheap `try_wait` reap (it never blocks), so an exit-watch task can
    /// poll it on a short interval and respawn the group the moment any plane
    /// crashes, rather than waiting out a 30 s counter window.
    pub fn exited_plane(&mut self) -> Option<&'static str> {
        if !self.data_tx.is_running() {
            Some("data_tx")
        } else if !self.tx_control.is_running() {
            Some("tx_control")
        } else if !self.rx_control.is_running() {
            Some("rx_control")
        } else {
            None
        }
    }

    /// The control planes' cumulative stats counters, for the delta-counter
    /// watchdog: the tx-control plane's totals and the rx-control stats lines.
    pub fn control_progress(&self) -> (TxPlaneTotals, u64) {
        (
            self.control_stats.totals(),
            self.rx_stats_lines.load(Ordering::Relaxed),
        )
    }

    /// The data plane's stats totals handle, for the TX-health and
    /// receive-queue watchdogs and the heartbeat's transmit rate.
    pub fn data_stats(&self) -> Arc<TxPlaneCounters> {
        self.data_stats.clone()
    }

    /// The aux transmit plane's stats totals.
    pub fn aux_totals(&self) -> TxPlaneTotals {
        self.aux_stats.totals()
    }

    /// The data plane's currently-running Reed-Solomon `(k, n)` ratio.
    pub fn data_fec(&self) -> (u8, u8) {
        (self.data_fec_k, self.data_fec_n)
    }

    /// The data plane's currently-running MCS index.
    pub fn data_mcs(&self) -> u8 {
        self.data_mcs_index
    }

    /// How the live data-plane retunes have been applied since this group came
    /// up. Surfaced through the bitrate snapshot onto `wfb-stats.json`.
    pub fn apply_counters(&self) -> ApplyCounters {
        self.applies
    }
    /// The retained `link_id` every transmitter in this group is keyed to (this
    /// drone's fleet slot).
    pub fn own_link_id(&self) -> u32 {
        self.own_link_id
    }

    /// The retained `link_id` the two uplink receivers are keyed to (the ground
    /// station's slot 0).
    pub fn uplink_link_id(&self) -> u32 {
        self.uplink_link_id
    }
}
