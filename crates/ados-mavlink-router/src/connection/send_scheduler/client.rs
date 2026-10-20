//! Client send paths: origin-aware writes from the IPC sockets and the proxies.

use super::*;

impl FcConnection {
    /// Write raw bytes toward the flight controller on behalf of a connected
    /// GCS client (the three [`crate::proxies`] transports). Prefers the
    /// local FC writer exactly like [`Self::send_bytes`]; when none exists
    /// and an aux-uplink sender is installed (this node is relaying a linked
    /// drone rather than driving a local FC), forwards there instead of
    /// dropping the client's bytes silently.
    ///
    /// A linked ground agent holds full authority over the node it relays, so
    /// there is no per-message filter here. The message-id denylist that used to
    /// sit in this path was removed: it covered only this MAVLink lane and never
    /// the relay's HTTP lane, where the same caller could already reach the
    /// vehicle-command routes, so it filtered one door while the other stood
    /// open. Authority is settled at the relay's authentication boundary
    /// instead, which is the only place that can actually decide it.
    /// The one exception is a visor, whose flight controller is a sensor hub
    /// and never a vehicle: there every flight-action frame is dropped
    /// whoever sent it (see `visor_gate`).
    ///
    /// **That rationale holds for the HTTP lane and did not hold here.** The
    /// raw MAVLink sockets had no authentication boundary at all: they accepted
    /// the peer address and discarded it. So on a node with no local flight
    /// controller, bytes from an unauthenticated caller took the fallback below
    /// and were radiated to a *remote, possibly airborne* aircraft — a longer
    /// reach than the local case, since it needs no proximity to the vehicle.
    ///
    /// `caller` now carries the classification the socket made, so the fallback
    /// can tell an authorised relay client from an anonymous one. An anonymous
    /// caller is reported and still relayed by default, and refused when
    /// `mavlink.aux_uplink_enforce_origin` is set: this port is advertised as
    /// the path for a third-party ground station, and a ground station relaying
    /// a linked drone is exactly the shape that takes this fallback, so the
    /// refusal lands mid-flight on the operator's screen rather than at install
    /// time. [`ClientOrigin::Relayed`] is exempt from that flag in every case —
    /// see the match arm for why.
    ///
    /// **The claim that a boundary exists for every caller was not true of the
    /// on-box IPC lane either.** That lane was described as trusted because the
    /// socket cannot be reached off the node, and stamped
    /// [`ClientOrigin::Trusted`] on every payload without looking. But one of
    /// its writers is the cloud relay, whose whole job is to take bytes off a
    /// broker and put them on this socket, so the reach behind those bytes is
    /// the broker's, not the node's. On a ground station with no local flight
    /// controller they then took the fallback below and left over the radio.
    /// The socket now identifies its writers, a forwarder declares itself, and
    /// the classification says which it was — so this path can no longer report
    /// broker traffic as locally produced.
    ///
    /// Deliberately distinct from `send_bytes`, which this router's own
    /// heartbeat/stream-interval/param-sweep housekeeping also calls: those
    /// exist to talk to a directly-attached FC and must stay a silent no-op
    /// with none installed, not start radiating this router's own internal
    /// traffic onto a client's uplink.
    pub async fn send_client_bytes(
        &self,
        data: &[u8],
        caller: ClientOrigin,
        injector: Option<&ados_protocol::ipc::InjectorClaim>,
    ) -> bool {
        self.send_client_bytes_with_budget(data, caller, injector, None)
            .await
    }

    /// [`Self::send_client_bytes`] with a wall bound on the local-FC write.
    ///
    /// For a caller on a loop that must not stall behind a wedged or
    /// flow-controlled flight controller. The bound is applied INSIDE the writer
    /// lock, by [`Self::send_bytes_bounded`] — the caller must not wrap this (or
    /// the unbounded form) in `tokio::time::timeout`, because cancelling the
    /// future can drop it after `write_all` has already put an arbitrary prefix
    /// of the frame on the serial line, releasing the lock with the stream
    /// desynchronised so the next frame is written onto that prefix. The flight
    /// controller then reports a CRC failure that looks like line noise and gets
    /// attributed to the radio rather than to us.
    ///
    /// Returns `false` when the frame did not reach the flight controller in
    /// full (timed out, failed, or no writer and no aux uplink installed). A
    /// timeout also drops the writer and asks for a reconnect.
    ///
    /// The aux-uplink fallback is unbounded because it is a `send_to` on a
    /// loopback UDP socket, which does not block on a wedged FC.
    pub async fn send_client_bytes_bounded(
        &self,
        data: &[u8],
        caller: ClientOrigin,
        injector: Option<&ados_protocol::ipc::InjectorClaim>,
        budget: Duration,
    ) -> bool {
        self.send_client_bytes_with_budget(data, caller, injector, Some(budget))
            .await
    }

    /// The shared body of the two public client-write entry points. `budget`
    /// `None` is the unbounded write; `Some` bounds it inside the writer lock.
    pub(super) async fn send_client_bytes_with_budget(
        &self,
        data: &[u8],
        caller: ClientOrigin,
        injector: Option<&ados_protocol::ipc::InjectorClaim>,
        budget: Option<Duration>,
    ) -> bool {
        // PIC-arbiter gate. An autonomous injector — a writer that declared
        // itself one, or any writer on the plugin plane by its kernel
        // credentials, whatever it declared (see `ados_protocol::ipc::IpcPeer`)
        // — is refused whenever a human holds manual control (or the arbiter is
        // not reporting — fail closed). An operator writer (injector = None) is
        // the human path and is NEVER gated. Where no local arbiter runs the
        // gate is not built and nothing is refused.
        if let Some(claim) = injector {
            if self.injector_refused_now(claim) {
                tracing::warn!(
                    injector = %claim.client_id,
                    len = data.len(),
                    "injector_command_refused_operator_holds_pic"
                );
                return false;
            }
        }
        // On a visor the flight controller is a sensor hub: this is the one
        // choke point every client write toward it passes through, so its
        // flight-action frames are dropped here (an MSP board, which does not
        // parse MAVLink, gets the bytes untouched). Elsewhere the bytes are
        // unchanged.
        let admitted = self.visor_admit(data).await;
        if admitted.is_empty() && !data.is_empty() {
            return false;
        }
        let data: &[u8] = &admitted;
        let has_writer = { self.writer.lock().await.is_some() };
        if has_writer {
            return match budget {
                // Bounded INSIDE the writer lock. This is the whole point of the
                // budget: the caller cannot get the same effect by wrapping this
                // future in a timeout, because cancelling it can leave a partial
                // frame on the serial line.
                Some(budget) => self.send_bytes_bounded(data, budget).await,
                None => self.send_bytes(data).await,
            };
        }
        if let Some(uplink) = self.aux_uplink.lock().await.as_ref() {
            // The amplifying case: no local vehicle, so these bytes leave over
            // the radio to one that may be flying. Both non-local readings are
            // reported, and reported apart, because they are different reaches:
            // one is a caller on this LAN that presented nothing, the other is a
            // caller anywhere the broker is reachable from.
            match caller {
                ClientOrigin::Unauthenticated => {
                    if self.cfg.aux_uplink_enforce_origin {
                        tracing::warn!(
                            len = data.len(),
                            "relay_uplink_refused_unauthenticated_client"
                        );
                        return false;
                    }
                    tracing::warn!(len = data.len(), "relay_uplink_from_unauthenticated_client");
                }
                // NEVER gated, under either setting, and not an oversight to be
                // tidied up later. `Relayed` is not a guess about a peer: it is
                // the cloud MQTT relay's own sticky self-declaration on its IPC
                // socket, and that relay runs on the very ground-station profile
                // where this aux uplink is installed. So the frames it labels
                // are the production remote-command path for a drone this node
                // is relaying, and refusing them would delete remote piloting
                // rather than close an exposure. Its authority comes from the
                // relay's own authentication boundary; what this arm adds is
                // that the reach is recorded.
                ClientOrigin::Relayed => {
                    tracing::warn!(len = data.len(), "relay_uplink_from_off_box_source");
                }
                ClientOrigin::Trusted => {}
            }
            return uplink.send(data);
        }
        false
    }

    /// Write raw bytes toward an MSP flight controller on behalf of a client,
    /// recording where they came from.
    ///
    /// Deliberately has no aux-uplink fallback, unlike
    /// [`Self::send_client_bytes`]. That fallback frames and radiates on the
    /// aux MAVLink lane, and MSP is a different protocol on a raw byte pipe; a
    /// ground station with no local flight controller has nothing to do with
    /// these bytes, so they stop here rather than being put on a lane that
    /// cannot carry them. What this does add is the classification the MSP path
    /// never had: it called the plain FC writer directly, so an MSP command that
    /// arrived over the broker was indistinguishable from one produced on the
    /// node.
    pub async fn send_client_raw(
        &self,
        data: &[u8],
        caller: ClientOrigin,
        injector: Option<&ados_protocol::ipc::InjectorClaim>,
    ) {
        // Same PIC-arbiter gate as send_client_bytes; MSP is a second way to
        // command an airframe, so it is gated identically.
        if let Some(claim) = injector {
            if self.injector_refused_now(claim) {
                tracing::warn!(
                    injector = %claim.client_id,
                    len = data.len(),
                    "injector_msp_refused_operator_holds_pic"
                );
                return;
            }
        }
        // A visor with a MAVLink flight controller filters this lane like the
        // MAVLink one; an MSP board does not parse MAVLink frames.
        let admitted = self.visor_admit(data).await;
        if admitted.is_empty() && !data.is_empty() {
            return;
        }
        let data: &[u8] = &admitted;
        if caller != ClientOrigin::Trusted {
            tracing::debug!(
                len = data.len(),
                origin = ?caller,
                "msp_command_from_non_local_source"
            );
        }
        let _ = self.send_bytes(data).await;
    }
}
