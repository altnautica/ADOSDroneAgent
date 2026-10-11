//! The bridge run loop, relay bring-up and relay-state publishing.

use super::*;

impl CloudRelayBridge {
    /// Run the bridge until `shutdown` fires. Polls the active-flag file on a
    /// short cadence and reconciles MQTT on each transition (explicit
    /// teardown/reconnect on uplink change), and republishes the relay state
    /// onto the heartbeat sink every 30 s. Best-effort throughout: a transport
    /// failure schedules a backoff retry, never a crash.
    pub async fn run(&mut self, mut shutdown: watch::Receiver<bool>) {
        info!(drone_id = ?self.drone_id, "cloud_relay.start");
        // The live relay task handle + its shutdown.
        let mut relay_task: Option<tokio::task::JoinHandle<()>> = None;
        let mut relay_shutdown: Option<watch::Sender<bool>> = None;

        let mut poll = tokio::time::interval(Duration::from_secs(2));
        let mut heartbeat = tokio::time::interval(STATUS_HEARTBEAT_INTERVAL);
        // Skip the immediate heartbeat tick (the contract sleeps first).
        heartbeat.tick().await;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
                _ = poll.tick() => {
                    // Reap a relay task that exited on its own (a dropped IPC
                    // socket while mavlink-router restarts, a broker reject). A
                    // finished task is `Some(finished_handle)`, so without this
                    // the relay would never respawn while the heartbeat kept
                    // reporting mqtt_connected:true. Tie liveness to the task and
                    // advance the backoff on a fast exit.
                    if relay_task.as_ref().is_some_and(|h| h.is_finished()) {
                        relay_task.take();
                        relay_shutdown.take();
                        let ran_for = self
                            .relay_started_at
                            .map(|t| t.elapsed())
                            .unwrap_or(Duration::ZERO);
                        self.on_relay_exit(ran_for, std::time::Instant::now());
                    }

                    let snap = UplinkSnapshot::read(&self.flag_path);
                    let action = self.reconcile_uplink(snap.as_ref());
                    match action {
                        MqttAction::TeardownThenReconnect => {
                            teardown_relay(&mut relay_task, &mut relay_shutdown).await;
                            self.mark_relay_down();
                            self.relay_started_at = None;
                            // A deliberate uplink switch is not a relay failure;
                            // do not hold the next connect behind the retry gate.
                            self.relay_retry_at = None;
                            tokio::time::sleep(UPLINK_SETTLE).await;
                            self.bring_up_relay(&mut relay_task, &mut relay_shutdown);
                        }
                        MqttAction::TeardownIdle => {
                            teardown_relay(&mut relay_task, &mut relay_shutdown).await;
                            self.mark_relay_down();
                            self.relay_started_at = None;
                            self.relay_retry_at = None;
                        }
                        MqttAction::BringUp => {
                            self.bring_up_relay(&mut relay_task, &mut relay_shutdown);
                        }
                        MqttAction::Noop => {
                            // Restore the relay whenever it is down but should be
                            // up (video-off→ok, or a reaped task), honouring the
                            // retry gate so a fast-exiting relay waits.
                            if self.throttle.forward_telemetry()
                                && relay_task.is_none()
                                && self.current_uplink.is_some()
                                && self.internet_reachable
                            {
                                self.bring_up_relay(&mut relay_task, &mut relay_shutdown);
                            }
                            // At 100 % drop the relay, keep the heartbeat.
                            if self.throttle == ThrottleState::Blocked {
                                teardown_relay(&mut relay_task, &mut relay_shutdown).await;
                                self.mark_relay_down();
                                self.relay_started_at = None;
                            }
                        }
                    }

                    // Fold the live relay connection flag into mqtt_connected so
                    // the heartbeat reports a broker session only once the
                    // transport has confirmed a ConnAck (and drops it on a
                    // disconnect the relay's own loop has not yet reaped).
                    self.refresh_mqtt_connected();
                    // The signaling lane reads the live data-cap verdict per offer.
                    self.video_allowed
                        .store(self.throttle.forward_video(), Ordering::Release);
                }
                _ = heartbeat.tick() => {
                    // Read the live connection state at publish time so the
                    // mqttConnected the GS reports tracks the broker session
                    // even if the relay flipped between poll ticks.
                    self.refresh_mqtt_connected();
                    let ts_ms = now_ms();
                    self.publish_relay_state(self.build_heartbeat(ts_ms));
                }
            }
        }

        teardown_relay(&mut relay_task, &mut relay_shutdown).await;
        self.mark_relay_down();
        // The bridge is gone: withdraw the relay block rather than leaving the
        // last forwarding state to be folded onto every later heartbeat.
        self.publish_relay_state(None);
        info!("cloud_relay.stop");
    }

    /// Spawn the MAVLink relay task when the throttle allows telemetry. The relay
    /// owns its own broker connection over the bound uplink (the kernel route was
    /// re-programmed by the failover before this reconnect).
    pub(super) fn bring_up_relay(
        &mut self,
        relay_task: &mut Option<tokio::task::JoinHandle<()>>,
        relay_shutdown: &mut Option<watch::Sender<bool>>,
    ) {
        if relay_task.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        if !self.throttle.forward_telemetry() {
            debug!("cloud_relay.relay_suppressed_by_data_cap");
            return;
        }
        // Hold the spawn off until the retry gate allows it so a relay that
        // keeps exiting fast cannot be respawned every poll tick.
        if !self.relay_spawn_allowed(std::time::Instant::now()) {
            debug!("cloud_relay.relay_respawn_deferred");
            return;
        }
        let (tx, rx) = watch::channel(false);
        // The connection-flag channel: the relay publishes the transport's
        // confirmed-connection atomic onto this; the poll loop reads it to set
        // mqtt_connected. Until the broker ConnAck arrives the flag is false.
        let (conn_tx, conn_rx) = watch::channel::<Option<Arc<AtomicBool>>>(None);
        let relay = MavlinkMqttRelay::new(self.device_id.clone(), self.relay_transport.clone());
        let sock = self.mavlink_sock.clone();
        // The MSP byte plane runs alongside the MAVLink frame plane over the same
        // broker so a cloud GCS reaches an MSP FC (Betaflight/iNav) too. It shares
        // the MAVLink relay's shutdown watch (a second receiver) and is aborted on
        // teardown by mark_relay_down, so it never outlives its MAVLink sibling.
        let msp_shutdown = tx.subscribe();
        let msp_relay = MspMqttRelay::new(self.device_id.clone(), self.relay_transport.clone());
        let msp_sock = self.msp_sock.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = relay.run_observed(&sock, rx, Some(&conn_tx)).await {
                warn!(error = %e, "cloud_relay.relay_exited");
            }
        });
        let msp_handle = tokio::spawn(async move {
            if let Err(e) = msp_relay.run(&msp_sock, msp_shutdown).await {
                warn!(error = %e, "cloud_relay.msp_relay_exited");
            }
        });
        // The WebRTC signaling lane rides the same broker on its own ClientID and
        // answers offers only while the data cap allows video.
        let signaling_shutdown = tx.subscribe();
        let signaling_cfg = self.relay_transport.clone();
        let signaling_id = self.device_id.clone();
        let video_allowed = self.video_allowed.clone();
        video_allowed.store(self.throttle.forward_video(), Ordering::Release);
        let signaling_handle = tokio::spawn(async move {
            if let Err(e) = crate::mqtt::run_webrtc_signaling(
                &signaling_id,
                &signaling_cfg,
                video_allowed,
                signaling_shutdown,
            )
            .await
            {
                warn!(error = %e, "cloud_relay.webrtc_signaling_exited");
            }
        });
        *relay_task = Some(handle);
        *relay_shutdown = Some(tx);
        self.msp_relay_task = Some(msp_handle);
        self.signaling_task = Some(signaling_handle);
        // The relay is starting but the broker is NOT confirmed connected yet;
        // mqtt_connected stays false until the transport reports a ConnAck (read
        // each poll tick from the connection flag).
        self.mqtt_connected = false;
        self.relay_connected_flag = None;
        self.relay_conn_rx = Some(conn_rx);
        self.relay_started_at = Some(std::time::Instant::now());
        info!("cloud_relay.mavlink_relay_started");
    }

    /// Fold the live relay connection flag into `mqtt_connected`. Reads the
    /// transport's confirmed-connection atomic (published by the current relay
    /// run) so the heartbeat reports a broker session only when one is actually
    /// up. With no relay/flag the link is down.
    pub(super) fn refresh_mqtt_connected(&mut self) {
        // Pull the latest published flag handle (the relay sets it once dialed).
        if self.relay_connected_flag.is_none() {
            if let Some(rx) = &self.relay_conn_rx {
                if let Some(flag) = rx.borrow().clone() {
                    self.relay_connected_flag = Some(flag);
                }
            }
        }
        self.mqtt_connected = self
            .relay_connected_flag
            .as_ref()
            .map(|f| f.load(Ordering::Acquire))
            .unwrap_or(false);
    }

    /// Mark the relay down: clear the confirmed-connection state so the next
    /// heartbeat cannot report a stale broker session after a teardown/reap, and
    /// abort the MSP byte-plane relay that was spawned alongside the MAVLink one so
    /// a wedged publisher cannot outlive its sibling and keep writing to the FC.
    pub(super) fn mark_relay_down(&mut self) {
        self.mqtt_connected = false;
        self.relay_connected_flag = None;
        self.relay_conn_rx = None;
        if let Some(handle) = self.msp_relay_task.take() {
            handle.abort();
        }
        if let Some(handle) = self.signaling_task.take() {
            handle.abort();
        }
    }

    /// Publish the relay state for the single `/agent/status` producer to fold.
    ///
    /// A `watch` send, so a heartbeat tick always reads the LATEST state rather
    /// than a queued backlog, and a bridge running with no sink wired (tests) is
    /// a no-op. `None` withdraws the relay block.
    pub(super) fn publish_relay_state(&self, state: Option<GsHeartbeat>) {
        if let Some(sink) = &self.relay_state_sink {
            // A closed channel means the heartbeat loop is gone, which is not
            // the bridge's problem to escalate — it is already shutting down.
            let _ = sink.send(state);
        } else {
            debug!("cloud_relay.relay_state_unwired");
        }
    }
}

/// Tear the relay task down: signal its shutdown, await with a timeout, abort on stall.
pub(super) async fn teardown_relay(
    relay_task: &mut Option<tokio::task::JoinHandle<()>>,
    relay_shutdown: &mut Option<watch::Sender<bool>>,
) {
    if let Some(tx) = relay_shutdown.take() {
        let _ = tx.send(true);
    }
    if let Some(mut handle) = relay_task.take() {
        // Await by &mut so a timeout still leaves us owning the handle to abort
        // it. `timeout(_, handle)` would consume the JoinHandle and merely DROP
        // it on timeout — and dropping a JoinHandle detaches the task, it does
        // not stop it, so a stalled relay (publisher wedged on a dead broker)
        // would leak and could keep writing to the FC socket alongside its
        // replacement.
        if tokio::time::timeout(Duration::from_secs(5), &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            debug!("cloud_relay.relay_teardown_timeout_aborted");
        }
    }
}

/// Fold a raw vehicle-state JSON snapshot into the heartbeat telemetry block.
/// The router publishes a nested snapshot (`position.*`, `battery.*`), so each
/// field is read from where the producer puts it; a missing field is `null`
/// ("not reported"), never a default.
pub(super) fn fold_telemetry(state: serde_json::Value) -> serde_json::Value {
    let Some(obj) = state.as_object() else {
        return serde_json::json!({});
    };
    let pick = |path: &[&str]| -> serde_json::Value {
        let mut cur = obj.get(path[0]);
        for key in &path[1..] {
            cur = cur.and_then(|v| v.get(*key));
        }
        cur.cloned().unwrap_or(serde_json::Value::Null)
    };
    serde_json::json!({
        "armed": pick(&["armed"]),
        "mode": pick(&["mode"]),
        "lat": pick(&["position", "lat"]),
        "lon": pick(&["position", "lon"]),
        "alt_rel": pick(&["position", "alt_rel"]),
        "battery_voltage": pick(&["battery", "voltage"]),
        "battery_remaining": pick(&["battery", "remaining"]),
        "last_heartbeat": pick(&["last_heartbeat"]),
    })
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
