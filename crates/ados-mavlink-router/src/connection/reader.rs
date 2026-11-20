//! The FC connect loop, the demo source and the frame read loop.

use super::ingest::param_slot;
use super::*;

impl FcConnection {
    /// Fold a decoded `PARAM_VALUE` into the cache and decide whether the file is
    /// written now, returning the bytes plus their destination when it is.
    ///
    /// The on-disk cache became the ONLY parameter source an HTTP surface can
    /// read the moment the map left the 10 Hz state snapshot, and one of those
    /// surfaces (`POST /api/params/{name}`) polls it for two seconds waiting for
    /// the FC to echo the value it just wrote. Under a flat
    /// [`PARAM_SAVE_DEBOUNCE`] that ack would be a coin flip, so a genuine change
    /// arriving outside a sweep is written at once. During a sweep
    /// (`param_priming`) the debounce holds, which is where the write volume
    /// actually is.
    ///
    /// Pruning happens here too: `slot` is the frame's `(param_index,
    /// param_count)`, and the frame that completes a fetch drops every cached
    /// name the FC did not report and is written at once.
    ///
    /// Serialising happens under the params lock; the caller does the blocking
    /// write off-reactor through [`ParamPersister`], so neither the lock nor a
    /// worker thread is held across disk I/O.
    pub(super) async fn record_param(
        &self,
        name: &str,
        value: f64,
        param_type: i64,
        slot: (u16, u16),
        since_save: &mut Instant,
    ) -> Option<(std::path::PathBuf, Vec<u8>)> {
        let mut pc = self.params.lock().await;
        let changed = pc.set(name, value, param_type);
        let pruned = pc.note_fetched(name, slot.0, slot.1) > 0;
        let sweeping = self.param_priming.load(Ordering::Relaxed);
        let write_now =
            pruned || (changed && !sweeping) || since_save.elapsed() >= PARAM_SAVE_DEBOUNCE;
        if !write_now {
            return None;
        }
        *since_save = Instant::now();
        pc.serialize()
            .ok()
            .map(|body| (pc.path().to_path_buf(), body))
    }

    /// Bind the parameter cache to the FC heartbeating on this link. A
    /// different FC than the one the cache was recorded for clears it; the
    /// emptied cache is queued before the new identity, so a crash between the
    /// two writes leaves a mismatch that clears again rather than a stale cache
    /// filed under the new FC.
    pub(super) async fn adopt_fc(&self, id: crate::param_cache::FcIdentity) {
        let mut pc = self.params.lock().await;
        if !pc.adopt_fc(id) {
            return;
        }
        if let Ok(body) = pc.serialize() {
            self.param_persister.persist(pc.path().to_path_buf(), body);
        }
        if let Some(body) = pc.serialize_fc() {
            self.param_persister.persist(pc.fc_path(), body);
        }
    }

    /// Connect-and-read loop. Returns only on shutdown via `cancel`.
    ///
    /// Four things end a live session: the read half hits EOF/error (the FC
    /// went away), `cancel` fires (shutdown), a write failure raises the
    /// reconnect signal (a transient agent->FC write error), or a link opened
    /// without probe evidence shows neither a HEARTBEAT nor MSP traffic within
    /// [`UNPROVEN_LINK_TIMEOUT`] (a silent port never reaches EOF, so this is
    /// what moves the sweep on). All of them fall through to one teardown +
    /// re-open path so the writer is always replaced with a fresh half, after a
    /// fixed [`RECONNECT_INTERVAL`] that never grows and never gives up. The
    /// interval floor is also what keeps a persistently unwritable port from
    /// tight-looping on its failed writes.
    pub async fn run(&self, cancel: Shutdown) {
        loop {
            let stream = tokio::select! {
                s = self.open() => s,
                _ = cancel.wait() => return,
            };
            let Some((read_half, write_half, port, baud)) = stream else {
                // The transport could not be opened — the configured endpoint is
                // unreachable / absent. Record it so link_hint() can explain the
                // silent link instead of leaving the operator with a bare "not
                // connected".
                self.open_failed.store(true, Ordering::Relaxed);
                tokio::select! {
                    _ = tokio::time::sleep(RECONNECT_INTERVAL) => {}
                    _ = cancel.wait() => return,
                }
                continue;
            };
            // Captured once the writer is installed: only a failure recorded
            // against THIS link's generation tears it down.
            let link_gen = self.begin_session(write_half, &port, baud).await;
            // A board identified as MSP by its USB descriptor is proven by
            // identity: it stays silent until a ground station polls it.
            let unproven = self.opened_unproven.load(Ordering::Relaxed)
                && self.fc_variant.lock().await.is_none();

            tokio::select! {
                _ = self.read_loop(read_half) => {}
                // A write failure asks the loop to rebuild the link. Tear down
                // the read half and fall through to the re-open below, which
                // installs a fresh writer; do not declare the FC disconnected
                // permanently for what may be a transient write error.
                _ = self.reconnect_requested(link_gen) => {
                    tracing::warn!("fc_write_failed_reconnecting");
                }
                _ = self.unproven_link_expired(), if unproven => {
                    tracing::warn!(port = %port, "fc_unproven_link_silent_reopening");
                }
                _ = cancel.wait() => {
                    self.connected.store(false, Ordering::Relaxed);
                    *self.last_heartbeat_at.lock().await = None;
                    *self.writer.lock().await = None;
                    return;
                }
            }

            self.end_session().await;

            // Re-open on the fixed interval — no ladder, no cap. A port that
            // opens but never accepts a write (readable-but-unwritable) used to
            // select a doubling backoff here; the interval floor bounds that
            // tight loop on its own, and the `source_unreachable` link hint
            // (driven by `open_failed`) is what names the cause to the operator.
            tokio::select! {
                _ = tokio::time::sleep(RECONNECT_INTERVAL) => {}
                _ = cancel.wait() => return,
            }
        }
    }

    /// Install a freshly opened transport as the current link: record its
    /// port and baud, install `write_half` as the FC writer (none for a
    /// command-down-gated source), mark the transport open and start the
    /// session unproven. Returns the link generation a write failure on this
    /// link is recorded against. [`Self::run`] calls it for every link it
    /// opens; the in-process simulation (`tests/dst`) drives it directly.
    pub async fn begin_session(&self, write_half: BoxedWriteHalf, port: &str, baud: u32) -> u64 {
        // A transport opened — clear the unreachable flag.
        self.open_failed.store(false, Ordering::Relaxed);
        *self.port.lock().await = port.to_string();
        // Identify an MSP FC (Betaflight/iNav) by the opened port's USB
        // descriptor — the passive signal that survives the silent-until-
        // polled blind spot of the byte sniff.
        *self.fc_variant.lock().await = fc_variant_for_port(port);
        self.baud.store(baud, Ordering::Relaxed);
        *self.fc_device.lock().await = link_state::canonical_fc_device(port).await;
        self.fc_signed.store(false, Ordering::Relaxed);
        if self.command_down_gated() {
            // MAVLink-over-ELRS, telemetry-only: install NO writer. With no
            // writer every send path is a no-op (send_bytes early-returns),
            // so the companion heartbeat, stream-interval requests, param
            // sweep, and any forwarded client bytes are all suppressed and
            // the host->FC command direction stays closed over the RC lane
            // — while the read loop still ingests inbound telemetry.
            // Dropping the write half closes the send side outright, a
            // stronger guarantee than gating each send path since no code
            // can smuggle a byte through an absent writer. Enabling the
            // command marker restores the writer (the else branch) for the
            // bench-validated command lane.
            drop(write_half);
            let mut writer = self.writer.lock().await;
            *writer = None;
            self.next_link_gen();
            drop(writer);
            tracing::info!(port = %port, baud, "fc_connected_crsf_mavlink_command_down_gated");
        } else {
            let mut writer = self.writer.lock().await;
            *writer = Some(write_half);
            self.next_link_gen();
            drop(writer);
            tracing::info!(port = %port, baud, "fc_connected");
        }
        let link_gen = self.link_gen();
        self.connected.store(true, Ordering::Relaxed);
        self.stamp_last_msg();
        self.link_proven.store(false, Ordering::Relaxed);
        link_gen
    }

    /// The link dropped (read EOF/error, a write failure, a silent unproven
    /// port): reset its state before the next open. Clearing the heartbeat
    /// clock means the freshly re-opened link reads as not-alive until a real
    /// HEARTBEAT arrives again.
    pub async fn end_session(&self) {
        self.connected.store(false, Ordering::Relaxed);
        *self.last_heartbeat_at.lock().await = None;
        *self.last_msp_at.lock().await = None;
        *self.msp_armed.lock().await = None;
        *self.fc_variant.lock().await = None;
        *self.fc_device.lock().await = None;
        self.fc_signed.store(false, Ordering::Relaxed);
        self.msp_warned.store(false, Ordering::Relaxed);
        *self.writer.lock().await = None;
        self.param_priming.store(false, Ordering::Relaxed);
        *self.param_sweep_started.lock().await = None;
        *self.param_last_request.lock().await = None;
        self.param_last_cached_count.store(0, Ordering::Relaxed);
        self.sensor_streams.reset_link();
        tracing::warn!("fc_disconnected");
    }

    /// Resolves once a link opened without probe evidence has gone
    /// [`UNPROVEN_LINK_TIMEOUT`] without a vehicle HEARTBEAT or MSP traffic.
    /// Never resolves when the session proved itself inside the window.
    pub(super) async fn unproven_link_expired(&self) {
        tokio::time::sleep(UNPROVEN_LINK_TIMEOUT).await;
        if self.link_proven.load(Ordering::Relaxed) {
            std::future::pending::<()>().await;
        }
    }

    /// Hardware-free demo loop. Instead of opening a serial link, a synthetic
    /// source ([`crate::demo`]) generates the circular-flight telemetry at 10 Hz
    /// and pushes it through the SAME paths a real FC drives: every frame is
    /// fanned out to the MAVLink socket and the GCS proxies, and every message
    /// updates the shared [`VehicleState`] so the state snapshot the service
    /// publishes is shape- and value-compatible with the Python demo's. The
    /// link reports as connected (port `demo`, baud 0) for the run's lifetime.
    /// Returns only on shutdown via `cancel`.
    pub async fn run_demo(&self, cancel: Shutdown) {
        *self.port.lock().await = "demo".to_string();
        self.baud.store(0, Ordering::Relaxed);
        self.connected.store(true, Ordering::Relaxed);
        self.stamp_last_msg();
        tracing::info!("fc_demo_started");

        let start = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut since_save = Instant::now();
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let t = start.elapsed().as_secs_f64();
                    let now = now_iso();
                    for msg in crate::demo::demo_messages(t) {
                        // Fan the frame out exactly as a received FC frame would
                        // be: serialised once with a vehicle source identity and
                        // broadcast to every consumer (drop if no consumers).
                        let header = MavHeader {
                            system_id: crate::demo::DEMO_SYSTEM_ID,
                            component_id: crate::demo::DEMO_COMPONENT_ID,
                            sequence: self.next_seq(),
                        };
                        if let Ok(bytes) = mavlink::serialize_v2(header, &msg) {
                            self.fc_frames_decoded.fetch_add(1, Ordering::Relaxed);
                            let _ = self.frame_tx.send(RxFrame::now(bytes.into()));
                        }
                        // Drive the shared state through the normal aggregator.
                        let persist = {
                            let mut st = self.state.lock().await;
                            st.update_from_message(&msg, &now)
                        };
                        if let Some((name, value, ptype)) = persist {
                            // Same off-reactor persistence as the live read loop:
                            // serialise under the lock, then write off the reactor.
                            if let Some((path, body)) = self
                                .record_param(&name, value, ptype, param_slot(&msg), &mut since_save)
                                .await
                            {
                                self.param_persister.persist(path, body);
                            }
                        }
                    }
                    self.stamp_last_msg();
                }
                _ = cancel.wait() => {
                    self.connected.store(false, Ordering::Relaxed);
                    tracing::info!("fc_demo_stopped");
                    return;
                }
            }
        }
    }

    pub(super) async fn read_loop(&self, mut reader: BoxedReadHalf) {
        let mut session = self.link_session().await;
        let mut chunk = [0u8; 2048];
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) => return, // EOF: link gone
                Ok(n) => n,
                Err(_) => return,
            };
            self.ingest(&mut session, &chunk[..n]).await;
        }
    }
}
