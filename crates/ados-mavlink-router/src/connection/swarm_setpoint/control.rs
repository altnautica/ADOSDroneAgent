//! The swarm socket reader, the control loop and the setpoint send.

use super::*;

/// Feed the newest swarm payload into a watch channel.
///
/// A watch rather than an mpsc on purpose: the control loop wants the NEWEST
/// table, never a backlog, so a loop that fell behind must skip stale frames
/// rather than work through them. Reading in its own task also keeps the byte
/// reader out of the control loop's `select!`, where a cancelled read would drop
/// buffered bytes and mis-frame the next line.
pub(super) async fn read_swarm_socket(
    path: String,
    tx: watch::Sender<Option<(Value, Instant)>>,
    cancel: Shutdown,
) {
    loop {
        let Ok(mut stream) = connect_with_retry(&path, 1, SWARM_RECONNECT).await else {
            tokio::select! {
                _ = cancel.wait() => return,
                _ = tokio::time::sleep(SWARM_RECONNECT) => continue,
            }
        };
        loop {
            let line = tokio::select! {
                _ = cancel.wait() => return,
                r = read_newline_line(&mut stream, MAX_SWARM_LINE) => r,
            };
            match line {
                Ok(Some(buf)) => match serde_json::from_slice::<Value>(&buf) {
                    Ok(v) => {
                        // A send failure means the control loop is gone; so is the
                        // reason to keep reading.
                        if tx.send(Some((v, Instant::now()))).is_err() {
                            return;
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "swarm_payload_parse_failed"),
                },
                // EOF or an IO error: the publisher restarted. Reconnect rather
                // than giving up, and do NOT clear the last payload — the
                // controller's own staleness gate decides when it has gone off,
                // and it is the only place that decision belongs.
                Ok(None) | Err(_) => break,
            }
        }
        tokio::select! {
            _ = cancel.wait() => return,
            _ = tokio::time::sleep(SWARM_RECONNECT) => {}
        }
    }
}

/// Tick the controller and send what it asks for, re-reading the block every
/// [`CONFIG_POLL`].
pub(super) async fn control_loop(
    fc: Arc<FcConnection>,
    state: Arc<Mutex<VehicleState>>,
    mut live: LiveSwarm,
    source: &mut SwarmConfigSource,
    rx: watch::Receiver<Option<(Value, Instant)>>,
    status: Arc<SwarmSetpointStatus>,
    cancel: Shutdown,
) -> LoopExit {
    let mut fixes: Vec<NeighborFix> = Vec::new();
    let mut tick = tokio::time::interval(CONTROL_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut reload = tokio::time::interval(CONFIG_POLL);
    reload.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.wait() => return LoopExit::Cancelled,
            _ = reload.tick() => {
                if let Some(cfg) = source.poll() {
                    if !eligible(&cfg) {
                        return LoopExit::Ineligible(cfg);
                    }
                    live.reconfigure(cfg);
                }
                if signing_holds(&fc, &status).await {
                    return LoopExit::Ineligible(live.cfg.clone());
                }
                continue;
            }
            _ = tick.tick() => {}
        }
        let now = Instant::now();

        fixes.clear();
        if let Some((payload, at)) = rx.borrow().as_ref() {
            fixes_from_payload(payload, now.saturating_duration_since(*at), &mut fixes);
        }

        let own = {
            let s = state.lock().await;
            OwnState {
                slot: live.cfg.fleet_slot,
                lat_deg: s.lat,
                lon_deg: s.lon,
                alt_rel_m: s.alt_rel,
                vn: s.vx,
                ve: s.vy,
                vd: s.vz,
                armed: s.armed,
                // The FC's OWN report, never what this layer asked for. A
                // setpoint sent in a mode that does not accept one is either
                // ignored or, worse, latched. Which mode that is depends on the
                // firmware, so the name is checked against every commandable
                // one rather than against ArduPilot's alone -- comparing to
                // "GUIDED" meant a PX4 vehicle could never be commanded, since
                // PX4 has no mode by that name.
                guided: ados_protocol::accepts_offboard_setpoints(&s.mode),
                // How stale our OWN fix is. Read from the position stamp
                // specifically, never from `last_update`, which every frame
                // refreshes: a heartbeat arriving while the position has frozen
                // is precisely the case the controller has to refuse, and
                // `last_update` reports it as healthy.
                fix_age: s
                    .position_at
                    .map(|t| std::time::Instant::now().saturating_duration_since(t)),
            }
        };

        // The fleet gives each aircraft its own system id, so the target comes
        // from the autopilot's own HEARTBEAT. Before one has latched there is no
        // address to command and the layer does not run: ticking anyway would
        // count setpoints that went nowhere.
        let Some(target) = fc.learned_fc_identity() else {
            status.stand_down();
            continue;
        };
        // A human holds PIC on the arbiter: stand down and say so.
        if fc.swarm_refused_by_pic() {
            status.stand_down_for_operator();
            continue;
        }
        let out = live.tick(own, &fixes, now);
        status.publish(out.precedence, out.emergency, live.controller.counters());
        if let Some(setpoint) = out.setpoint {
            send_setpoint(&fc, target, &setpoint).await;
        }
    }
}

/// Turn a control-layer setpoint into MAVLink 86 and send it to `target`, the
/// autopilot's latched `(system, component)` id.
///
/// The message is built through `ados_protocol::mavlink::GuidedSetpoint`, which
/// validates the `type_mask` and the coordinate frame and refuses a NaN on an
/// active axis. That validation is the reason this goes through the shared builder
/// instead of constructing the payload here: a malformed setpoint must be refused
/// on this side of the wire, not diagnosed from the vehicle's behaviour.
pub(super) async fn send_setpoint(fc: &Arc<FcConnection>, target: (u8, u8), setpoint: &Setpoint) {
    let msg = match setpoint_message(target, setpoint) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, kind = ?setpoint.kind, "swarm_setpoint_rejected");
            return;
        }
    };
    fc.send_msg(&msg).await;
}

/// The validated `SET_POSITION_TARGET_GLOBAL_INT` for `setpoint`, addressed to
/// `target`.
pub(super) fn setpoint_message(
    target: (u8, u8),
    setpoint: &Setpoint,
) -> Result<ados_protocol::mavlink::MavMessage, ados_protocol::mavlink::SetpointError> {
    let wire = GuidedSetpoint {
        kind: WireSetpointKind::GlobalInt,
        coordinate_frame: setpoint.coordinate_frame(),
        type_mask: setpoint.type_mask(),
        // The global message carries lat/lon already scaled by 1e7.
        x: setpoint.lat_e7 as f64,
        y: setpoint.lon_e7 as f64,
        z: setpoint.alt_m as f64,
        vx: setpoint.vn,
        vy: setpoint.ve,
        vz: setpoint.vd,
        afx: 0.0,
        afy: 0.0,
        afz: 0.0,
        yaw: 0.0,
        yaw_rate: 0.0,
    };
    // Target ids are the autopilot's, not ours: the router's own identity goes
    // in the header `send_msg` builds.
    wire.build_message(target.0, target.1)
}
