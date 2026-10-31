use super::*;

/// The system id in a MAVLink frame's header.
///
/// The two versions put it in different places: v2 (`0xFD`) has STX, LEN,
/// INCOMPAT, COMPAT, SEQ, SYSID at byte 5; v1 (`0xFE`) has STX, LEN, SEQ, SYSID
/// at byte 3, and byte 5 is its message id. `None` for anything else or a runt.
pub(super) fn mavlink_system_id(frame: &[u8]) -> Option<u8> {
    match frame.first()? {
        0xFD => frame.get(5).copied(),
        0xFE => frame.get(3).copied(),
        _ => None,
    }
}

/// `MAV_TYPE_GCS`: a ground station's own HEARTBEAT, never a vehicle.
pub(super) const MAV_TYPE_GCS: u8 = 6;
/// `MAV_AUTOPILOT_INVALID`: a component that is not a flight controller (a
/// companion computer, a gimbal, the drone agent's own heartbeat).
pub(super) const MAV_AUTOPILOT_INVALID: u8 = 8;

/// The sender's system id when `frame` is a flight controller's HEARTBEAT.
///
/// Only an autopilot's own HEARTBEAT names the vehicle. The drone's tee also
/// carries frames the FC forwards from other components (a ground station on
/// a telemetry radio, a companion computer), and counting those would make a
/// slot's identity flicker and report two healthy drones that each carry a
/// local ground station on id 255 as a collision.
pub(super) fn autopilot_heartbeat_system_id(frame: &[u8]) -> Option<u8> {
    let (payload_at, msg_id) = match frame.first()? {
        0xFD => (
            10usize,
            u32::from_le_bytes([*frame.get(7)?, *frame.get(8)?, *frame.get(9)?, 0]),
        ),
        0xFE => (6usize, u32::from(*frame.get(5)?)),
        _ => return None,
    };
    if msg_id != 0 {
        return None;
    }
    let len = usize::from(*frame.get(1)?);
    let payload = frame.get(payload_at..payload_at + len)?;
    // A v2 sender trims trailing zero bytes from the payload, so a field past
    // the end reads as zero rather than as a runt.
    let byte = |i: usize| payload.get(i).copied().unwrap_or(0);
    // HEARTBEAT wire order: custom_mode u32, type u8, autopilot u8, ...
    if byte(4) == MAV_TYPE_GCS || byte(5) == MAV_AUTOPILOT_INVALID {
        return None;
    }
    mavlink_system_id(frame)
}

impl<'a> AuxSinks<'a> {
    /// Only the MAVLink republish seam is wired. Every other channel is decoded
    /// and counted, then dropped for want of a consumer.
    pub fn mavlink_only(mavlink: &'a MavlinkIngest) -> Self {
        Self {
            mavlink,
            rpc_response: None,
            config_tunnel: None,
            app_stream: None,
            local_plugins: None,
        }
    }
}

impl AuxSinksOwned {
    pub(super) fn borrow(&self) -> AuxSinks<'_> {
        AuxSinks {
            mavlink: &self.mavlink,
            rpc_response: self.rpc_response.as_ref(),
            config_tunnel: self.config_tunnel.as_deref(),
            app_stream: self.app_stream.as_deref(),
            local_plugins: self.local_plugins.as_deref(),
        }
    }
}

/// Decode one datagram and act on it, updating exactly one received-path
/// counter.
///
/// Split from the read loop so the whole decode-and-dispatch decision is
/// testable without a socket.
pub(super) async fn dispatch(
    slot: u8,
    datagram: &[u8],
    sinks: &AuxSinks<'_>,
    counters: &AuxCounters,
    gate: &mut SeamGate,
    peers: &AuxPeerCache,
) {
    let c = &counters.0;
    counters.bump(&c.datagrams_received);
    c.bytes_received
        .fetch_add(datagram.len() as u64, Ordering::Relaxed);

    let (channel, payload) = match aux_mux::decode(datagram) {
        Ok(v) => v,
        Err(e) => {
            match e {
                // Not ours. Normal on a shared lane, and deliberately not
                // grouped with the damage counters below.
                AuxDecodeError::BadMagic => counters.bump(&c.decode_foreign),
                AuxDecodeError::TooShort => counters.bump(&c.decode_runt),
                AuxDecodeError::UnsupportedVersion(v) => {
                    counters.bump(&c.decode_unsupported_version);
                    tracing::debug!(version = v, "ground_aux_unsupported_framing_version");
                }
                AuxDecodeError::UnknownChannel(ch) => {
                    counters.bump(&c.decode_unknown_channel);
                    tracing::debug!(channel = ch, "ground_aux_unknown_channel");
                }
                // Past the magic and version checks, so this claimed to be our
                // frame and arrived wrong. A real transport fault.
                AuxDecodeError::LengthMismatch { declared, actual } => {
                    counters.bump(&c.decode_damaged);
                    tracing::debug!(declared, actual, "ground_aux_frame_damaged");
                }
                AuxDecodeError::TooLong(declared) => {
                    counters.bump(&c.decode_overlong);
                    tracing::debug!(declared, "ground_aux_frame_overlong");
                }
            }
            return;
        }
    };

    match channel {
        AuxChannel::Mavlink => {
            // One datagram carries several frames: the lane's loss tracks
            // packets per second rather than bytes, so the drone batches. Split
            // on the header-derived boundary, which needs no dialect knowledge
            // and so cannot desynchronise on a message id this build lacks.
            let split = ados_protocol::aux_mux::split_frames(payload);
            // A payload that yields no whole frame is forwarded intact rather
            // than dropped. A drone build that predates batching sends exactly
            // one frame per datagram, and a frame this splitter cannot measure
            // is still a frame the router may be able to read. Dropping it here
            // would lose real traffic to make the split look tidy.
            let frames: Vec<&[u8]> = if split.is_empty() {
                vec![payload]
            } else {
                split
            };
            // Read who each frame's flight controller is, and say so when two
            // slots claim the same identity. Until now the slot was known at
            // the bind and thrown away before the send, so a fleet whose
            // aircraft shared a system id looked exactly like a fleet whose
            // aircraft did not -- the frames are byte-identical downstream and
            // the counters are fleet aggregates. Read before the seam gate: who
            // a slot's aircraft is does not depend on whether the router's
            // ingest socket is up right now.
            for frame in &frames {
                if let Some(system_id) = autopilot_heartbeat_system_id(frame) {
                    let colliding = peers.observe_system_id(slot, system_id);
                    if !colliding.is_empty() {
                        counters.bump(&c.system_id_collisions);
                        // Name what the aircraft SHOULD be carrying alongside
                        // what it is. A collision report that only says two
                        // aircraft clash leaves the operator to work out the
                        // remedy; the slot already determines the answer.
                        let expected = crate::fleet_identity_policy::system_id_for_slot(slot);
                        tracing::warn!(
                            slot,
                            system_id,
                            also_on = ?colliding,
                            expected_system_id = ?expected,
                            "ground_aux_system_id_collision"
                        );
                    }
                }
            }
            let now = Instant::now();
            if !gate.may_attempt(now) {
                // Count every frame the datagram carried, not the datagram, so
                // sent and republished totals stay comparable.
                for _ in &frames {
                    counters.bump(&c.mavlink_frames);
                    counters.bump(&c.republish_lane_down);
                }
                return;
            }
            for frame in frames {
                counters.bump(&c.mavlink_frames);
                match sinks.mavlink.send_frame(frame).await {
                    Ok(()) => {
                        gate.on_success();
                        counters.bump(&c.mavlink_republished);
                    }
                    Err(e) => {
                        gate.on_failure(now, &e);
                        counters.bump(&c.republish_errors);
                    }
                }
            }
        }
        // The relayed node describing itself. Decoded into the peer cache, whose
        // sidecar is what lets this node answer what it is relaying; a body that
        // does not decode is counted rather than dropped silently, because a
        // peer sending frames this build cannot read is a version mismatch worth
        // seeing.
        AuxChannel::Status => {
            counters.bump(&c.status_frames);
            match NodeStatus::decode(payload) {
                Some(status) => {
                    if !peers.record_status(status) {
                        counters.bump(&c.status_out_of_order);
                    }
                }
                None => counters.bump(&c.status_undecodable),
            }
        }
        AuxChannel::Identity => {
            counters.bump(&c.identity_frames);
            match NodeIdentity::decode(payload) {
                Some(identity) => {
                    // Compare the peer's advertised plugin contract versions
                    // against this node's own, for any plugin both run. A
                    // divergence means the two halves would silently disagree on
                    // the plugin wire (an AD05-class field reshape), so it is
                    // surfaced as a counter and a warning rather than left to
                    // climb as an unexplained decode statistic on one rig.
                    if let (Some(local), Some(pl)) = (sinks.local_plugins, identity.pl.as_deref()) {
                        for (id, peer_ver) in ados_protocol::node_status::parse_plugin_list(pl) {
                            if let Some(&own_ver) = local.get(&id) {
                                if own_ver != peer_ver {
                                    counters.bump(&c.identity_plugin_version_mismatch);
                                    tracing::warn!(
                                        plugin_id = %id,
                                        peer_contract_version = peer_ver,
                                        own_contract_version = own_ver,
                                        "ground_aux_plugin_contract_version_mismatch"
                                    );
                                }
                            }
                        }
                    }
                    peers.record_identity(identity);
                }
                None => counters.bump(&c.identity_undecodable),
            }
        }
        // A relay-proxy HTTP request. The ground never receives this channel
        // — Requests travel ground→drone only — so a non-zero count means a
        // peer is sending the wrong direction or a frame was misrouted.
        // Counted for visibility, not alarmed on, because the only thing the
        // ground could do with a Request it received is drop it.
        AuxChannel::Request => {
            counters.bump(&c.request_frames);
            tracing::debug!("ground_aux_unexpected_request_channel");
        }
        // A relay-proxy HTTP response fragment, drone → ground. Forwards the
        // payload to the proxy's process via the IPC ingest if one is
        // registered; without one the datagram is counted and dropped — a rig
        // running no proxy caller still receives these when a paired drone is
        // answering requests another GS instance sent over a shared link, and
        // silence would hide that the lane is alive.
        AuxChannel::Response => {
            counters.bump(&c.response_frames);
            // Decode to validate only: `payload` is already the exact wire
            // form the proxy's reader decodes, so it is forwarded verbatim
            // rather than round-tripped through the encoder.
            match ados_protocol::aux_rpc::decode_response(payload) {
                Ok(_) => {
                    if let Some(ingest) = sinks.rpc_response {
                        ingest.send(payload).await;
                        // Until now this was never bumped, so it read a
                        // permanent 0 and a working lane looked identical to a
                        // dead one — during exactly the kind of incident this
                        // counter exists to settle. `send` is best-effort and
                        // reports nothing back, so this counts what was handed
                        // to the proxy's ingest, not what the proxy read.
                        counters.bump(&c.response_dispatched);
                    } else {
                        counters.bump(&c.response_orphan);
                    }
                }
                Err(_) => counters.bump(&c.response_undecodable),
            }
        }
        // Ground-measured link quality, ground → drone. This node is the sender
        // of that channel, so receiving one means a peer sent the wrong
        // direction or a frame was misrouted. Counted and dropped, never acted
        // on: a ground station applying a link-quality report would be reacting
        // to its own measurement echoed back at it.
        AuxChannel::LinkFeedback => {
            counters.bump(&c.link_feedback_frames);
        }
        // A config-over-radio frame, drone → ground: the linked drone's answer
        // to a config request this ground station's injector sent. The
        // config-tunnel service cannot bind this lane's port — this consumer
        // holds it — so the frame is handed on to that service's own loopback
        // ingress, aux framing INTACT so it re-checks the channel itself rather
        // than trusting this hop.
        //
        // No ingress on a node that never opted the channel in: the frame is
        // counted and dropped, which is the honest reading of "a peer is
        // answering config requests we are not making".
        AuxChannel::ConfigTunnel => {
            counters.bump(&c.config_tunnel_frames);
            match sinks.config_tunnel {
                Some(ingest) if ingest.send(datagram).await => {
                    counters.bump(&c.config_tunnel_forwarded);
                }
                _ => counters.bump(&c.config_tunnel_undelivered),
            }
        }
        // A neutral application-stream frame, drone → ground: an on-board
        // application (e.g. a vision or analytics plugin) radiating its own
        // datagrams to the paired ground node. Handed to the registered
        // application sink verbatim; without one it is counted and dropped, the
        // honest reading of "an application is streaming to a consumer this node
        // is not running".
        AuxChannel::AppStream => {
            counters.bump(&c.app_stream);
            match &sinks.app_stream {
                Some(s) => s.deliver(payload),
                None => counters.bump(&c.app_stream_undelivered),
            }
        }
        // The uplink mirror of `AppStream`, ground → drone. A ground downlink
        // consumer is the SENDER of that channel, never a receiver, so a
        // non-zero count means a peer sent the wrong direction or a frame was
        // misrouted. Counted for visibility and dropped, never forwarded.
        AuxChannel::AppCommand => {
            counters.bump(&c.app_command_unexpected);
            tracing::debug!("ground_aux_unexpected_app_command_channel");
        }
    }
}
