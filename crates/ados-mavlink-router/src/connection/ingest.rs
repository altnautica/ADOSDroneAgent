//! The read loop's per-read work: frame reassembly, the fan-out, and the
//! link clocks, vehicle identity and vehicle state driven by the vehicle's
//! own frames.

use super::*;

impl FcConnection {
    /// A fresh [`LinkSession`] for the link [`Self::begin_session`] opened.
    ///
    /// An MSP FC (identified by its USB descriptor at open) speaks MSP, not
    /// MAVLink, on FC->host, so extract_frames yields nothing and its
    /// responses never reach a GCS proxy on the frame lane. The variant is
    /// fixed for the session (set before it starts, cleared only after it
    /// ends), so it is captured once here and [`Self::ingest`] forwards the
    /// raw bytes. A MAVLink FC captures none, so the frame path is
    /// byte-unchanged.
    pub async fn link_session(&self) -> LinkSession {
        let msp_variant = self.fc_variant.lock().await.clone();
        LinkSession {
            msp_variant,
            msp_sniffer: super::msp_armed::MspResponseSniffer::default(),
            buf: Vec::with_capacity(4096),
            since_save: Instant::now(),
            msp_count: 0,
            msp_window: Instant::now(),
            vehicle: VehicleSource::default(),
            fc_adopted: false,
        }
    }

    /// One read off the FC link: reassemble its frames, fan each out, and
    /// drive the link clocks, the vehicle identity and the vehicle state
    /// from the vehicle's own. The read loop calls it per read; the
    /// in-process simulation (`tests/dst`) per delivered chunk.
    pub async fn ingest(&self, s: &mut LinkSession, chunk: &[u8]) {
        // Host receive time of every frame completed by this read: the
        // earliest point this process saw the bytes.
        let rx_ns = ados_sensorbus::time::mono_ns();
        // An MSP FC speaks MSP (not MAVLink) on FC->host: forward the raw chunk
        // verbatim so a polling MSP GCS receives the FC's responses, and skip
        // MAVLink framing entirely: an MSP stream carries no MAVLink, so
        // scanning it only spends CPU on 0xFD/0xFE payload bytes and risks a
        // chance checksum match putting a bogus frame on the frame lane
        // alongside the raw ones. The variant is none for a MAVLink FC, so the
        // framing path below is byte-unchanged; the MSP link hint comes from the
        // USB-descriptor variant, so the MSP-start sniff is unnecessary here too.
        if let Some(variant) = s.msp_variant.as_deref() {
            self.link_proven.store(true, Ordering::Relaxed);
            let _ = self.raw_tx.send(Bytes::copy_from_slice(chunk));
            let mut latest = None;
            s.msp_sniffer.feed(chunk, |cmd, payload| {
                if let Some(armed) = super::msp_armed::armed_from_frame(variant, cmd, payload) {
                    latest = Some(armed);
                }
            });
            if let Some(armed) = latest {
                *self.msp_armed.lock().await = Some((armed, Instant::now()));
            }
            return;
        }
        s.buf.extend_from_slice(chunk);
        // Cap the reassembly buffer so a stream of junk cannot grow it.
        if s.buf.len() > 1 << 20 {
            s.buf.clear();
        }
        // Sniff the raw chunk for MSP frame starts before reassembly drains
        // the leading junk where they live. An FC whose serial port is set to
        // MSP instead of MAVLink emits these; arming the hint lets the
        // operator be told the link speaks the wrong protocol. Observe-only:
        // MSP bytes are never decoded, acted on, or forwarded to the FC.
        let starts = count_msp_frame_starts(chunk);
        if starts > 0 {
            if s.msp_window.elapsed() > MSP_WINDOW {
                s.msp_count = 0;
                s.msp_window = Instant::now();
            }
            s.msp_count += starts;
            // Two starts inside the window, and only while no HEARTBEAT is
            // live — the alive gate is what guards against a stray `$M`/`$X`
            // landing inside a MAVLink payload on a healthy link.
            if s.msp_count >= 2 && !self.mavlink_alive().await {
                self.link_proven.store(true, Ordering::Relaxed);
                *self.last_msp_at.lock().await = Some(Instant::now());
                if !self.msp_warned.swap(true, Ordering::Relaxed) {
                    let port = self.port.lock().await.clone();
                    tracing::warn!(port = %port, "fc_link_msp_detected_not_mavlink");
                }
            }
        }
        let frames = extract_frames(&mut s.buf);
        if frames.is_empty() {
            return;
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
            let _ = self.frame_tx.send(RxFrame::new(rx_ns, frame.clone()));
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
                if !s.vehicle.admit(header.system_id, header.component_id, &msg) {
                    continue;
                }
                self.note_vehicle_frame(&frame);
                // The stream cadence's activity clock counts the vehicle's
                // streamed data, not its HEARTBEAT: an FC that rebooted keeps
                // heartbeating but forgot every requested interval, and a
                // link that carries heartbeats alone must still read stalled
                // so the streams are asked for again.
                if !matches!(msg, MavMessage::HEARTBEAT(_)) {
                    self.stamp_last_msg();
                }
                // Which requested stream this frame answers.
                if let Some(id) = framing::frame_msg_id(&frame) {
                    self.stream_watch.seen(id, self.clock.now());
                }
                if let MavMessage::HEARTBEAT(hb) = &msg {
                    self.latch_fc_identity(header.system_id, header.component_id);
                    if !s.fc_adopted {
                        s.fc_adopted = true;
                        self.adopt_fc(crate::param_cache::FcIdentity {
                            system_id: header.system_id,
                            autopilot: hb.autopilot as u8,
                            vehicle_type: hb.mavtype as u8,
                        })
                        .await;
                    }
                    self.link_proven.store(true, Ordering::Relaxed);
                    *self.last_heartbeat_at.lock().await = Some(steady_now());
                    // A real FC HEARTBEAT clears any MSP suspicion so a link
                    // that started noisy (or recovered) reads as healthy at
                    // once, and re-arms the once-per-episode warning for any
                    // future MSP episode.
                    *self.last_msp_at.lock().await = None;
                    self.msp_warned.store(false, Ordering::Relaxed);
                    s.msp_count = 0;
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
                        .record_param(&name, value, ptype, param_slot(&msg), &mut s.since_save)
                        .await
                    {
                        self.param_persister.persist(path, body);
                    }
                }
            }
        }
    }
}

/// The read loop's per-link state: the reassembly buffer, the MSP evidence
/// window, which component on the link is the vehicle, and whether the
/// parameter cache is bound to it. One per opened link, from
/// [`FcConnection::link_session`]; [`FcConnection::ingest`] feeds it.
pub struct LinkSession {
    /// The MSP firmware family of the link, `None` for MAVLink.
    msp_variant: Option<String>,
    /// Observe-only reader of the FC's status replies to a proxy poller.
    msp_sniffer: super::msp_armed::MspResponseSniffer,
    buf: Vec<u8>,
    since_save: Instant,
    /// Rolling MSP-evidence accumulator for the link-hint detector.
    msp_count: usize,
    msp_window: Instant,
    /// Which component on this link is the vehicle. Per session: a re-opened
    /// link latches afresh from its first autopilot HEARTBEAT.
    vehicle: VehicleSource,
    /// The cache is bound to the FC at the first vehicle HEARTBEAT of each
    /// session: a different FC can only appear across a reconnect.
    fc_adopted: bool,
}

/// The `(param_index, param_count)` of a `PARAM_VALUE`; any other message has
/// no slot, which [`ParamCache::note_fetched`] ignores.
pub(super) fn param_slot(msg: &MavMessage) -> (u16, u16) {
    match msg {
        MavMessage::PARAM_VALUE(m) => (m.param_index, m.param_count),
        _ => (u16::MAX, 0),
    }
}
