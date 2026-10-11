//! Per-datagram dispatch: decode, target check and hand-off by channel.

use super::*;

/// Whether an FC-bound uplink frame is addressed to this drone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetVerdict {
    /// Broadcast, no target field, or addressed to our own flight controller.
    Deliver,
    /// Addressed to a different aircraft's system id.
    ForeignTarget,
    /// Addressed to a specific system id, but ours is not latched yet.
    IdentityUnknown,
}

/// Decide whether `frame` may go to the local flight controller.
///
/// The target is read from the decoded message, so every message type that
/// carries a `target_system` field is covered without a hand-kept offset table.
/// A frame that does not decode against the dialect carries no target this
/// router can read; it passes, as it did before, and the autopilot applies its
/// own CRC and id checks.
pub(super) fn fc_bound_verdict(frame: &[u8], fc_identity: Option<(u8, u8)>) -> TargetVerdict {
    match crate::aux_uplink::frame_target_system(frame) {
        None | Some(0) => TargetVerdict::Deliver,
        Some(target) => match fc_identity {
            Some((own, _)) if own == target => TargetVerdict::Deliver,
            Some(_) => TargetVerdict::ForeignTarget,
            None => TargetVerdict::IdentityUnknown,
        },
    }
}

pub(super) async fn dispatch(
    payload: &[u8],
    deps: &UplinkDeps,
    counters: &AuxUplinkConsumerCounters,
) {
    let (channel, inner) = match aux_mux::decode(payload) {
        Ok(v) => v,
        Err(AuxDecodeError::BadMagic) => {
            counters.0.decode_foreign.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(_) => {
            counters.0.decode_damaged.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    match channel {
        AuxChannel::Mavlink => {
            // A sender may batch several frames into one datagram (see
            // `aux_uplink::run`'s own batching); split on the header-derived
            // boundary, which needs no dialect knowledge. A payload that yields no
            // whole frame is injected intact rather than dropped — an older sender
            // that predates batching sends exactly one frame per datagram.
            let split = aux_mux::split_frames(inner);
            let frames: Vec<&[u8]> = if split.is_empty() { vec![inner] } else { split };
            counters
                .0
                .mavlink_frames
                .fetch_add(frames.len() as u64, Ordering::Relaxed);
            for frame in frames {
                // Item 13 — a private-payload TUNNEL frame (payload_type above
                // the reserved max) is plugin-addressed APPLICATION traffic (an
                // AD05-style cross-link frame), not an FC command. Publish it to
                // the router's frame fan-out so an on-board plugin subscribed to
                // TUNNEL actually receives the ground→drone frame. It must NEVER
                // go down the FC serial cable — where, before this, it was handed
                // to the FC and silently disappeared (the drone-side plugin never
                // saw the operator's command).
                if let Some(pt) = ados_protocol::mavlink::tunnel_payload_type(frame) {
                    if pt > ados_protocol::mavlink::TUNNEL_RESERVED_PAYLOAD_TYPE_MAX {
                        deps.fc.inject_frame(bytes::Bytes::copy_from_slice(frame));
                        counters.0.mavlink_injected.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
                // Item 14 (safety-critical) — a genuine FC-bound uplink frame is
                // routed through the ARBITRATED client path, not the ungated
                // serial write it used to take. `send_client_bytes_bounded` is
                // the single PIC choke point the drone's own autonomous injector
                // also passes through, so a relayed operator command and an
                // autonomous write are adjudicated by one decision function and
                // cannot drift. The relayed frame carries no injector claim — it
                // is the human path, and the operator beats the injector by
                // construction; the autonomous guidance write declares its
                // injector and is gated.
                // A wedged or flow-controlled FC must not stall this recv loop,
                // which also feeds the relay-proxy Request lane. The bound is
                // FC_WRITE_TIMEOUT.
                //
                // It is applied INSIDE the writer lock, by
                // `send_bytes_bounded`. Wrapping the write in
                // `tokio::time::timeout` out here was the bug: on expiry the
                // future was dropped while `write_all` may already have put an
                // arbitrary PREFIX of this frame on the serial line, and the
                // writer mutex was released with the stream in that unknown
                // state and the writer still installed. The next frame was then
                // written straight onto the prefix, so the flight controller saw
                // a corrupt frame, reported it as a CRC failure, and the blame
                // landed on the radio. A bounded write instead drops the writer
                // and asks for a reconnect: one frame lost and the link
                // re-opened, which is the honest outcome.
                // Fleet addressing. Every drone hears the one uplink, so a
                // frame naming another aircraft's system id must never reach
                // this FC. Broadcast (0) and frames with no target field pass.
                match fc_bound_verdict(frame, deps.fc.learned_fc_identity()) {
                    TargetVerdict::Deliver => {}
                    TargetVerdict::ForeignTarget => {
                        counters
                            .0
                            .mavlink_foreign_target_dropped
                            .fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    TargetVerdict::IdentityUnknown => {
                        counters
                            .0
                            .mavlink_target_unknown_dropped
                            .fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                }
                // Route-aware forwarding: the radio-linked ground station is
                // the `aux` endpoint; learn it and hand its frame to the
                // other clients.
                let routes = deps.fc.routes();
                if routes.enabled() {
                    routes.ingress(&routes.register("aux"), frame);
                }
                if deps
                    .fc
                    .send_client_bytes_bounded(frame, ClientOrigin::Relayed, None, FC_WRITE_TIMEOUT)
                    .await
                {
                    counters.0.mavlink_injected.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters
                        .0
                        .mavlink_write_timeouts
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        AuxChannel::Request => {
            // A relay-proxy HTTP request from the ground station. Forward it to
            // the drone's own HTTP API and send the response back over the aux
            // downlink. Runs in a spawned task so the uplink consumer's read
            // loop never stalls behind a slow HTTP call.
            counters.0.rpc_requests.fetch_add(1, Ordering::Relaxed);
            match ados_protocol::aux_rpc::decode_request_fragment(inner) {
                Ok(request) => {
                    // An unresolved local id cannot adjudicate a target, so accept
                    // everything and let the operator see it in the counter —
                    // dropping every request on a drone that simply has not been
                    // provisioned yet is a far worse failure. A named target that
                    // is not us is still dropped WITHOUT an answer, so the ground
                    // never mistakes this drone's response for the one it asked for.
                    let addressed_elsewhere = !deps.own_device_id.is_empty()
                        && !request.target.is_empty()
                        && request.target != deps.own_device_id.as_bytes();
                    if addressed_elsewhere {
                        counters
                            .0
                            .rpc_requests_not_for_us
                            .fetch_add(1, Ordering::Relaxed);
                        warn_target_mismatch_once(request.target, &deps.own_device_id);
                        return;
                    }
                    // Fragments of a request accumulate until it is whole; only
                    // a complete request takes an in-flight slot.
                    let Some(request) = crate::aux_rpc_handler::accept_fragment(&request) else {
                        return;
                    };
                    let Ok(permit) = REQUEST_SLOTS.clone().try_acquire_owned() else {
                        // Already at the in-flight ceiling. Dropping is safe
                        // and cheap here: the ground retransmits an unanswered
                        // request, and the dedupe cache serves the retry from
                        // the first attempt's work.
                        counters.0.rpc_requests_shed.fetch_add(1, Ordering::Relaxed);
                        return;
                    };
                    if let Some(egress) = &deps.egress {
                        let egress = Arc::clone(egress);
                        let dedupe = Arc::clone(&deps.dedupe);
                        let counters = counters.clone();
                        let sender = deps.own_device_id.to_string();
                        tokio::spawn(async move {
                            let _permit = permit;
                            crate::aux_rpc_handler::handle(
                                &request, &egress, &dedupe, &counters, &sender,
                            )
                            .await;
                        });
                    } else {
                        tracing::debug!("aux_rpc_request_no_egress");
                    }
                }
                Err(_) => {
                    counters.0.rpc_undecodable.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        AuxChannel::LinkFeedback => {
            // The ground station reporting what it actually decoded of our
            // downlink. This is the only honest loss measurement a transmitting
            // drone can obtain — its own radio is in monitor mode and cannot
            // capture its own injected frames — so publish it for the radio
            // service's adaptive bitrate ladder to read.
            counters
                .0
                .link_feedback_frames
                .fetch_add(1, Ordering::Relaxed);
            match ados_protocol::link_feedback::LinkFeedback::decode(inner) {
                Ok(fb) => {
                    // The uplink is ONE transmission for the whole fleet, so
                    // every drone receives every record. Acting on one that
                    // measures a different slot would steer this drone's video
                    // rate from a neighbour's link — a false measurement, and
                    // worse than having none, because a no-sample ladder at
                    // least holds honestly.
                    //
                    // An unprovisioned node (no slot in config) accepts
                    // nothing: it cannot prove a record is about itself, and
                    // guessing is the failure this check exists to prevent.
                    let own_slot = ados_protocol::fleet_identity::local_slot();
                    if own_slot != Some(fb.target_slot) {
                        counters
                            .0
                            .link_feedback_not_for_us
                            .fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    let sidecar = ados_protocol::link_feedback::LinkFeedbackSidecar::now(&fb);
                    let path = ados_protocol::link_feedback::sidecar_path();
                    if let Err(e) = ados_protocol::link_feedback::write_sidecar_to(&path, &sidecar)
                    {
                        counters
                            .0
                            .link_feedback_write_errors
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::debug!(error = %e, "link_feedback_sidecar_write_failed");
                    }
                }
                Err(_) => {
                    // Undecodable is counted separately from "not received":
                    // a lane delivering damage is a different fault from a
                    // silent one, and the ladder must not act on either.
                    counters
                        .0
                        .link_feedback_undecodable
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // A config-over-radio frame from the ground station. The config-tunnel
        // service cannot bind this lane's port — this consumer holds it — so the
        // frame is handed to that service's own loopback ingress with its aux
        // framing INTACT, so it re-checks the channel itself instead of trusting
        // this hop. The service is the only thing that ever interprets a config
        // frame; nothing here reads or acts on the config payload.
        //
        // Forwarded, never spawned: one small datagram per chunk, so the read
        // loop pays a loopback write rather than a task.
        AuxChannel::ConfigTunnel => {
            counters
                .0
                .config_tunnel_frames
                .fetch_add(1, Ordering::Relaxed);
            match deps.config_tunnel.as_deref() {
                Some(ingest) if ingest.send(payload).await => {
                    counters
                        .0
                        .config_tunnel_forwarded
                        .fetch_add(1, Ordering::Relaxed);
                }
                // The normal case with the channel opted out: nothing is bound
                // to the ingress, so the frame is counted and dropped rather
                // than reported as a fault.
                _ => {
                    counters
                        .0
                        .config_tunnel_undelivered
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // Ground → drone APPLICATION traffic, addressed to an on-board plugin
        // rather than to the flight controller. It must never reach the FC
        // serial cable, and until it was routed here it fell through to the
        // unknown-channel tally and was dropped — so an operator-side plugin
        // could send to its on-board counterpart and the counterpart never saw
        // it, with nothing anywhere naming the loss.
        //
        // The radio service owns the socket plugins already subscribe to, so the
        // decoded payload goes BACK over that command socket's `publish` op
        // rather than being re-radiated. The egress handle is the same one the
        // RPC responder uses, so this adds no new seam and no new socket.
        AuxChannel::AppStream | AuxChannel::AppCommand => {
            counters.0.app_datagrams.fetch_add(1, Ordering::Relaxed);
            let Some(egress) = deps.egress.as_ref() else {
                counters
                    .0
                    .app_datagrams_undelivered
                    .fetch_add(1, Ordering::Relaxed);
                return;
            };
            match egress.publish_app(channel, inner).await {
                // `delivered == 0` is the normal state of a node with no plugin
                // subscribed, NOT a drop — counting it would make a healthy lane
                // report total loss.
                Ok(delivered) => {
                    tracing::debug!(
                        channel = channel as u8,
                        len = inner.len(),
                        delivered,
                        "aux_app_datagram_published"
                    );
                }
                // Debug, not warn: the dead-switch case is an operator choice,
                // and a warn per datagram would flood a deliberately disabled
                // deployment's log.
                Err(e) => {
                    counters
                        .0
                        .app_datagrams_undelivered
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(error = %e, channel = channel as u8, "aux_app_publish_failed");
                }
            }
        }
        _ => {
            counters
                .0
                .non_mavlink_channel
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}
