//! The FC connect loop, the demo source and the frame read loop.

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
    /// Serialising happens under the params lock; the caller does the blocking
    /// write off-reactor through [`ParamPersister`], so neither the lock nor a
    /// worker thread is held across disk I/O.
    pub(super) async fn record_param(
        &self,
        name: &str,
        value: f64,
        param_type: i64,
        since_save: &mut Instant,
    ) -> Option<(std::path::PathBuf, Vec<u8>)> {
        let mut pc = self.params.lock().await;
        let changed = pc.set(name, value, param_type);
        let sweeping = self.param_priming.load(Ordering::Relaxed);
        let write_now = (changed && !sweeping) || since_save.elapsed() >= PARAM_SAVE_DEBOUNCE;
        if !write_now {
            return None;
        }
        *since_save = Instant::now();
        pc.serialize()
            .ok()
            .map(|body| (pc.path().to_path_buf(), body))
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
            // A transport opened — clear the unreachable flag.
            self.open_failed.store(false, Ordering::Relaxed);
            *self.port.lock().await = port.clone();
            // Identify an MSP FC (Betaflight/iNav) by the opened port's USB
            // descriptor — the passive signal that survives the silent-until-
            // polled blind spot of the byte sniff.
            *self.fc_variant.lock().await = fc_variant_for_port(&port);
            self.baud.store(baud, Ordering::Relaxed);
            *self.fc_device.lock().await = link_state::canonical_fc_device(&port).await;
            self.fc_signed.store(false, Ordering::Relaxed);
            if self.command_down_gated() {
                // MAVLink-over-ELRS, telemetry-only: install NO writer. With no
                // writer every send path is a no-op (send_bytes early-returns),
                // so the companion heartbeat, stream-interval requests, param
                // sweep, and any forwarded client bytes are all suppressed and
                // the host->FC command direction stays closed over the RC lane
                // — while the read loop below still ingests inbound telemetry.
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
            // Captured once the writer is installed: only a failure recorded
            // against THIS link's generation tears it down.
            let link_gen = self.link_gen();
            self.connected.store(true, Ordering::Relaxed);
            self.stamp_last_msg();
            self.link_proven.store(false, Ordering::Relaxed);
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

            // Link dropped (read EOF/error or a write failure): reset state and
            // reconnect. Clearing the heartbeat clock means the freshly re-opened
            // link reads as not-alive until a real HEARTBEAT arrives again.
            self.connected.store(false, Ordering::Relaxed);
            *self.last_heartbeat_at.lock().await = None;
            *self.last_msp_at.lock().await = None;
            *self.fc_variant.lock().await = None;
            *self.fc_device.lock().await = None;
            self.fc_signed.store(false, Ordering::Relaxed);
            self.msp_warned.store(false, Ordering::Relaxed);
            *self.writer.lock().await = None;
            self.param_priming.store(false, Ordering::Relaxed);
            *self.param_sweep_started.lock().await = None;
            *self.param_last_request.lock().await = None;
            self.param_last_cached_count.store(0, Ordering::Relaxed);
            tracing::warn!("fc_disconnected");

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
                            let _ = self.frame_tx.send(bytes.into());
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
                                .record_param(&name, value, ptype, &mut since_save)
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
        // An MSP FC (identified by its USB descriptor at open) speaks MSP, not
        // MAVLink, on FC->host, so extract_frames yields nothing and its responses
        // never reach a GCS proxy on the frame lane. The variant is fixed for this
        // read loop (set before it starts, cleared only after it returns), so
        // capture it once and forward the raw bytes below. A MAVLink FC captures
        // false here, so the frame path is byte-unchanged.
        let is_msp = self.fc_variant.lock().await.is_some();
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 2048];
        let mut since_save = Instant::now();
        // Rolling MSP-evidence accumulator for the link-hint detector.
        let mut msp_count: usize = 0;
        let mut msp_window = Instant::now();
        // Which component on this link is the vehicle. Per session: a re-opened
        // link latches afresh from its first autopilot HEARTBEAT.
        let mut vehicle = VehicleSource::default();
        loop {
            let n = match reader.read(&mut chunk).await {
                Ok(0) => return, // EOF: link gone
                Ok(n) => n,
                Err(_) => return,
            };
            // An MSP FC speaks MSP (not MAVLink) on FC->host: forward the raw chunk
            // verbatim so a polling MSP GCS receives the FC's responses, and skip
            // MAVLink framing entirely: an MSP stream carries no MAVLink, so
            // scanning it only spends CPU on 0xFD/0xFE payload bytes and risks a
            // chance checksum match putting a bogus frame on the frame lane
            // alongside the raw ones. is_msp is false for a MAVLink FC, so the
            // framing path below is byte-unchanged; the MSP link hint comes from the
            // USB-descriptor variant, so the MSP-start sniff is unnecessary here too.
            if is_msp {
                self.link_proven.store(true, Ordering::Relaxed);
                let _ = self.raw_tx.send(Bytes::copy_from_slice(&chunk[..n]));
                continue;
            }
            buf.extend_from_slice(&chunk[..n]);
            // Cap the reassembly buffer so a stream of junk cannot grow it.
            if buf.len() > 1 << 20 {
                buf.clear();
            }
            // Sniff the raw chunk for MSP frame starts before reassembly drains
            // the leading junk where they live. An FC whose serial port is set to
            // MSP instead of MAVLink emits these; arming the hint lets the
            // operator be told the link speaks the wrong protocol. Observe-only:
            // MSP bytes are never decoded, acted on, or forwarded to the FC.
            let starts = count_msp_frame_starts(&chunk[..n]);
            if starts > 0 {
                if msp_window.elapsed() > MSP_WINDOW {
                    msp_count = 0;
                    msp_window = Instant::now();
                }
                msp_count += starts;
                // Two starts inside the window, and only while no HEARTBEAT is
                // live — the alive gate is what guards against a stray `$M`/`$X`
                // landing inside a MAVLink payload on a healthy link.
                if msp_count >= 2 && !self.mavlink_alive().await {
                    self.link_proven.store(true, Ordering::Relaxed);
                    *self.last_msp_at.lock().await = Some(Instant::now());
                    if !self.msp_warned.swap(true, Ordering::Relaxed) {
                        let port = self.port.lock().await.clone();
                        tracing::warn!(port = %port, "fc_link_msp_detected_not_mavlink");
                    }
                }
            }
            let frames = extract_frames(&mut buf);
            if frames.is_empty() {
                continue;
            }
            // Work proof for the supervisor: every checksum-valid frame off the
            // link counts, whatever component sent it.
            self.fc_frames_decoded
                .fetch_add(frames.len() as u64, Ordering::Relaxed);
            // The arrival timestamp is taken here, before any state lock and
            // before the fan-out, so the measurement describes when the bytes
            // reached this process rather than when they won a contended mutex.
            // The state mutex is fair, not priority-ordered, so that wait is
            // exactly the thing a loop-rate question is asking about and must
            // not be folded into the answer.
            //
            // One formatted stamp per `read()`, not per frame: every frame in a
            // chunk arrived in the same `read()`, so a per-frame stamp bought no
            // extra resolution while allocating an RFC3339 `String` each time —
            // at the measured 66 frames/s ArduPilot ingest that was the largest
            // avoidable allocation left in this loop.
            let now = now_iso();
            for frame in frames {
                // Fan the raw frame out verbatim (drop if no consumers / lagging).
                // The original bytes are forwarded unchanged for both protocol
                // versions; nothing here re-encodes a received frame. This same
                // fan-out is the plugin lane: a subscribed consumer (the MAVLink
                // socket) receives every frame, including a TUNNEL one, and
                // filters for its own private payload_type on its side.
                let _ = self.frame_tx.send(frame.clone());
                self.stamp_last_msg();
                // Classify a TUNNEL (385) carrying a private (application)
                // payload_type. The typed parser below rejects an unregistered
                // payload_type as an unknown enum value, so this reads the type
                // straight off the wire. This is observe-only: the frame is
                // already on the plugin lane (the fan-out above); a TUNNEL is a
                // transparent opaque pipe and is NEVER decoded, acted on, or
                // re-sent toward the flight controller from here. The read loop
                // only consumes FC->host frames, so there is no FC-injection path
                // at this point regardless.
                if let Some(payload_type) = mavlink::tunnel_payload_type(&frame) {
                    if payload_type > mavlink::TUNNEL_RESERVED_PAYLOAD_TYPE_MAX {
                        tracing::debug!(payload_type, "mavlink_tunnel_inbound_to_plugin_lane");
                    }
                }
                if let Ok((header, msg)) = mavlink::parse_any(&frame) {
                    // Only the vehicle's own frames drive the link and the state.
                    // The FC forwards every other component's broadcast traffic
                    // here too (a ground station's HEARTBEAT on a telemetry radio,
                    // a gimbal, a second companion); those stay on the fan-out
                    // above but never touch armed, mode, the alive clock, the
                    // target system or the parameter cache. Our own companion
                    // HEARTBEAT names no autopilot, so it is excluded the same way.
                    if !vehicle.admit(header.system_id, header.component_id, &msg) {
                        continue;
                    }
                    self.note_vehicle_frame(&frame);
                    if let MavMessage::HEARTBEAT(_) = &msg {
                        self.latch_fc_identity(header.system_id, header.component_id);
                        self.link_proven.store(true, Ordering::Relaxed);
                        *self.last_heartbeat_at.lock().await = Some(Instant::now());
                        // A real FC HEARTBEAT clears any MSP suspicion so a link
                        // that started noisy (or recovered) reads as healthy at
                        // once, and re-arms the once-per-episode warning for any
                        // future MSP episode.
                        *self.last_msp_at.lock().await = None;
                        self.msp_warned.store(false, Ordering::Relaxed);
                        msp_count = 0;
                    }
                    // `Instant::now()` rather than the hoisted `now`: the
                    // cadence tracker measures inter-arrival gaps and needs a
                    // monotonic clock, which the ISO stamp is not.
                    if matches!(msg, MavMessage::ATTITUDE(_)) {
                        self.attitude_cadence.lock().await.record(Instant::now());
                    }
                    let persist = {
                        let mut st = self.state.lock().await;
                        st.update_from_message(&msg, &now)
                    };
                    if let Some((name, value, ptype)) = persist {
                        // Serialise under the lock, release it, then do the disk
                        // write off-reactor so neither the params lock nor a
                        // worker thread is held across blocking I/O.
                        if let Some((path, body)) = self
                            .record_param(&name, value, ptype, &mut since_save)
                            .await
                        {
                            self.param_persister.persist(path, body);
                        }
                    }
                }
            }
        }
    }
}
