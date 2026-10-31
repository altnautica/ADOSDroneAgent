//! Group respawn, teardown and the stats readers.

use super::*;

impl RadioProcesses {
    /// Kill the whole radio group and respawn it, REUSING the live data-plane
    /// tunables (`data_fec_k`/`data_fec_n`/`data_mcs_index`) rather than the
    /// boot-time `cfg` values. A channel hop / return-home restarts the entire
    /// group (data + both control planes follow the channel), and the naive path
    /// spawned from `cfg` alone — silently reverting any operator-pinned manual
    /// link tier or adaptive FEC/MCS the data plane had applied. This rebuilds the
    /// data plane from the retained trio and keeps the control planes on the
    /// boot-time control rate, so a hop preserves the running data tier.
    ///
    /// Returns `false` if the group respawn fails (the radio group is then dead;
    /// the supervisor's outer loop respawns from scratch).
    pub async fn respawn_group(
        &mut self,
        cfg: &WfbConfig,
        link: std::sync::Arc<tokio::sync::Mutex<crate::link_quality::LinkStats>>,
    ) -> bool {
        self.kill_all().await;
        // The data plane spawns from the retained trio; the control planes keep
        // the boot-time control rate (their own fixed FEC + the management MCS).
        let data_cfg =
            data_cfg_from_retained(cfg, self.data_fec_k, self.data_fec_n, self.data_mcs_index);
        let key_path = self.tx_key_path.clone();
        let data_tx = match WfbProcess::spawn_data_tx(
            &self.iface,
            &data_cfg,
            &key_path,
            self.own_link_id,
            self.data_stats.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "respawn_group_data_tx_failed");
                return false;
            }
        };
        let tx_control = match WfbProcess::spawn_tx_control(
            &self.iface,
            cfg,
            &key_path,
            self.own_link_id,
            self.control_stats.clone(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "respawn_group_tx_control_failed");
                return false;
            }
        };
        let mut rx_control =
            match WfbProcess::spawn_rx_control(&self.iface, &key_path, self.uplink_link_id).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "respawn_group_rx_control_failed");
                    return false;
                }
            };
        self.uplink = UplinkFeed::new(link);
        let stats_reader = Self::spawn_stats_reader(
            &mut rx_control,
            self.uplink.clone(),
            self.rx_stats_lines.clone(),
        );
        self.data_tx = data_tx;
        self.tx_control = tx_control;
        self.rx_control = rx_control;
        self.stats_reader = stats_reader;
        // kill_all cleared the aux processes but kept aux_settings; re-open the
        // aux pair on the NEW channel iff it was open before the hop. A re-open
        // failure leaves the pair down with the settings KEPT, so the aux
        // watchdog keeps retrying it, and does NOT fail the whole group — the
        // data + control planes are already up, so the primary link is healthy
        // regardless of the additive aux pair.
        if let Some(settings) = self.aux_settings {
            let aux_cfg = settings.to_cfg();
            let aux_tx = WfbProcess::spawn_aux_tx(
                &self.iface,
                &aux_cfg,
                &key_path,
                self.own_link_id,
                self.aux_stats.clone(),
            )
            .await;
            let aux_rx = WfbProcess::spawn_aux_rx(
                &self.iface,
                &aux_cfg,
                &key_path,
                self.uplink_link_id,
                self.uplink.clone(),
            )
            .await;
            match (aux_tx, aux_rx) {
                (Ok(tx), Ok(rx)) => {
                    self.aux_tx = Some(tx);
                    self.aux_rx = Some(rx);
                }
                (tx, rx) => {
                    if let Err(e) = &tx {
                        tracing::warn!(error = %e, "respawn_group_aux_tx_failed");
                    }
                    if let Err(e) = &rx {
                        tracing::warn!(error = %e, "respawn_group_aux_rx_failed");
                    }
                    // Drop any half that spawned so a partial re-open does not
                    // leave one process running; the watchdog retries the pair.
                    drop(tx);
                    drop(rx);
                    self.aux_tx = None;
                    self.aux_rx = None;
                }
            }
        }
        true
    }

    /// Kill ONLY the data-tx process and respawn it from the retained iface/key
    /// and current data tunables. Leaves the control planes + stats RX running.
    /// Returns `false` if the respawn fails (the data plane is then dead).
    ///
    /// The cfg view is a `WfbConfig::default()` overlay carrying only the data
    /// tunables, so the fleet identity CANNOT be re-derived from it — the
    /// retained `own_link_id` is passed explicitly. Deriving it from the overlay
    /// would key the respawned transmitter to the unprovisioned default
    /// `link_id(1, 0)`, i.e. onto the ground station's uplink `channel_id`.
    pub(super) async fn respawn_data_tx(&mut self) -> bool {
        let cfg = WfbConfig {
            fec_k: self.data_fec_k,
            fec_n: self.data_fec_n,
            mcs_index: self.data_mcs_index,
            ..WfbConfig::default()
        };
        self.data_tx.kill().await;
        match WfbProcess::spawn_data_tx(
            &self.iface,
            &cfg,
            &self.tx_key_path,
            self.own_link_id,
            self.data_stats.clone(),
        )
        .await
        {
            Ok(p) => {
                self.data_tx = p;
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "data_tx_respawn_failed");
                false
            }
        }
    }

    /// Kill every process group + stop the stats reader. This INCLUDES the
    /// auxiliary pair when it is open, so a respawn or shutdown never orphans the
    /// aux processes. `aux_settings` is deliberately left intact so a whole-group
    /// respawn (a channel hop) can re-open the aux pair on the new channel; a
    /// `close` is what clears the settings.
    pub async fn kill_all(&mut self) {
        // Abort the receiver's reader BEFORE killing its producer: it owns the
        // only read end of rx-control's stats pipe, and leaving it live against
        // a killed process just makes it return on EOF a moment later. The
        // transmitters' readers are owned by their `WfbProcess`.
        if let Some(r) = self.stats_reader.take() {
            r.abort();
        }
        self.data_tx.kill().await;
        self.tx_control.kill().await;
        self.rx_control.kill().await;
        if let Some(mut tx) = self.aux_tx.take() {
            tx.kill().await;
        }
        if let Some(mut rx) = self.aux_rx.take() {
            rx.kill().await;
        }
    }
}

impl UplinkFeed {
    pub(super) fn new(link: Arc<tokio::sync::Mutex<crate::link_quality::LinkStats>>) -> Arc<Self> {
        Arc::new(Self {
            agg: tokio::sync::Mutex::new(crate::link_quality::UplinkAggregator::new()),
            link,
        })
    }

    pub(super) async fn observe(
        &self,
        plane: crate::link_quality::UplinkPlane,
        stats: crate::link_quality::LinkStats,
    ) {
        let mut agg = self.agg.lock().await;
        let block = agg.observe(plane, stats, std::time::Instant::now());
        *self.link.lock().await = block;
    }

    pub(super) async fn plane_closed(&self, plane: crate::link_quality::UplinkPlane) {
        let mut agg = self.agg.lock().await;
        let block = agg.plane_closed(plane, std::time::Instant::now());
        *self.link.lock().await = block;
    }
}

/// Read one uplink `wfb_rx`'s stdout line-by-line, feed its link-quality
/// monitor, and fold every snapshot into the shared uplink block the sidecar and
/// reactive-hop logic read. Every stats line also advances `rx_stats_lines` when
/// given (the control plane's watchdog counter). Ends on EOF (process death) or
/// task abort; on EOF this receiver's measurement stops standing for the link,
/// so a dead receiver's last reading is never reported as live.
pub(super) async fn stats_reader_loop(
    stdout: tokio::process::ChildStdout,
    uplink: Arc<UplinkFeed>,
    plane: crate::link_quality::UplinkPlane,
    rx_stats_lines: Option<Arc<AtomicU64>>,
) {
    let mut lines = ados_proc::lines::BoundedLines::new(stdout);
    let mut mon = crate::link_quality::LinkQualityMonitor::new();
    while let Some(line) = lines.next_line().await {
        let now_iso = now_iso();
        if let Some(stats) = mon.feed_line(&line, &now_iso) {
            if let Some(n) = &rx_stats_lines {
                n.fetch_add(1, Ordering::Relaxed);
            }
            uplink.observe(plane, stats).await;
        }
    }
    uplink.plane_closed(plane).await;
}

/// Drain a `wfb_tx` plane's stdout and fold every stats line into its plane's
/// totals. Ends on EOF or task abort.
pub(super) async fn tx_stats_reader_loop(
    stdout: tokio::process::ChildStdout,
    stats: Arc<TxPlaneCounters>,
) {
    let mut lines = ados_proc::lines::BoundedLines::new(stdout);
    while let Some(line) = lines.next_line().await {
        if let Some(pkt) = crate::tx_liveness::parse_tx_pkt(line.trim_end()) {
            stats.record(&pkt);
        }
    }
}

/// Current ISO-8601 UTC timestamp for the link-stats `timestamp` field.
pub(super) fn now_iso() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
