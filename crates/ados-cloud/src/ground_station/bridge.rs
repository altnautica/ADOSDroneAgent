//! The uplink-aware cloud relay bridge.
//!
//! Lifecycle + decision logic for the ground-station cloud relay. The decision
//! surface (what to tear down / bring up / forward on each uplink, health, and
//! data-cap transition) is factored into pure methods so it is unit-testable
//! with no MQTT and no network; the live supervision (`start`/`run`) drives the
//! MQTT gateway + MAVLink relay tasks and the 30 s GS status heartbeat.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::mqtt::transport::TransportConfig;
use crate::mqtt::{MavlinkMqttRelay, MspMqttRelay};

/// The active-uplink sidecar the `ados-net` uplink router writes. Presence ==
/// an active uplink; the body carries the live uplink + reachability + data-cap
/// level. The bridge reads this each tick as the cross-process replacement for
/// the old in-process uplink event bus.
pub const UPLINK_ACTIVE_FLAG: &str = "/run/ados/uplink-active";

/// The MAVLink IPC socket the relay bridges FC frames over.
pub const MAVLINK_SOCK: &str = "/run/ados/mavlink.sock";

/// The MSP IPC socket the relay bridges raw MSP bytes over. The byte plane for an
/// MSP FC (Betaflight/iNav), sibling to the MAVLink frame plane above.
pub const MSP_SOCK: &str = "/run/ados/msp.sock";

/// The vehicle-state IPC socket the GS heartbeat enriches from.
pub const STATE_SOCK: &str = "/run/ados/state.sock";

// ── Reconnect tunables ───────────────────────────────────────────────────
/// Fixed delay before a relay that exited fast is respawned.
///
/// Flat, with no ceiling and no attempt cap. The ladder this replaces doubled
/// 2 s → 300 s, so a broker that rejected for the first few minutes of a cold
/// boot (clock skew, DNS not yet up, a captive-portal uplink) left the node
/// invisible to the cloud fleet view for up to five minutes after the uplink
/// genuinely recovered, with no way for an operator to force a retry short of
/// restarting the unit. The 5 s floor is what the ladder was really there for:
/// it stops a broken seam hot-looping a respawn on every poll tick.
const RELAY_RETRY_INTERVAL: Duration = Duration::from_secs(5);
const UPLINK_SETTLE: Duration = Duration::from_secs(2);
const STATUS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// The minimum the relay task must stay alive before its run counts as
/// "healthy". A relay that exits sooner than this (a flapping IPC socket, an
/// immediate broker reject) is treated as a fast exit and waits
/// [`RELAY_RETRY_INTERVAL`] before the respawn; a run that lasts at least this
/// long is respawned immediately on the next reap.
const RELAY_HEALTHY_AFTER: Duration = Duration::from_secs(30);

/// The data-cap throttle level driving what the bridge forwards to the cloud.
/// Maps the active-flag `data_cap_state` strings to forwarding decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleState {
    /// Forward video + telemetry.
    None,
    /// Approaching cap: still forward everything (warning only).
    Warn,
    /// 95 %: stop video forwarding, keep telemetry.
    VideoOff,
    /// 100 %: drop everything except the minimal status heartbeat.
    Blocked,
}

impl ThrottleState {
    /// Parse the active-flag `data_cap_state` string. Unknown / absent → None.
    pub fn from_cap_str(s: &str) -> Self {
        match s {
            "warn_80" => ThrottleState::Warn,
            "throttle_95" => ThrottleState::VideoOff,
            "blocked_100" => ThrottleState::Blocked,
            _ => ThrottleState::None,
        }
    }

    /// The wire string carried on the GS heartbeat (`throttle_state`). Matches
    /// the Python `_THROTTLE_*` values.
    pub fn as_str(&self) -> &'static str {
        match self {
            ThrottleState::None => "none",
            ThrottleState::Warn => "warn_80",
            ThrottleState::VideoOff => "throttle_95",
            ThrottleState::Blocked => "blocked_100",
        }
    }

    /// Whether video is forwarded to the cloud at this level.
    pub fn forward_video(&self) -> bool {
        matches!(self, ThrottleState::None | ThrottleState::Warn)
    }

    /// Whether telemetry is forwarded to the cloud at this level.
    pub fn forward_telemetry(&self) -> bool {
        !matches!(self, ThrottleState::Blocked)
    }
}

/// One read of the active-uplink sidecar. `present` is the file's existence
/// (the legacy mesh signal); the rest is the parsed body.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct UplinkSnapshot {
    #[serde(default)]
    pub active_uplink: String,
    #[serde(default)]
    pub internet_reachable: bool,
    #[serde(default = "default_cap")]
    pub data_cap_state: String,
}

fn default_cap() -> String {
    "ok".to_string()
}

impl UplinkSnapshot {
    /// Read the active-uplink sidecar. Returns `None` when the file is absent
    /// (no active uplink) — the bridge treats that as "idle".
    pub fn read(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// The ground station's relay-state slice, republished every 30 s for the
/// node's single `/agent/status` producer to fold onto its heartbeat. A smaller
/// document than the drone heartbeat: it adds the relay's own forwarding state
/// on top of the minimum status-mutation contract, not the full board/service
/// enrichment.
///
/// This used to be POSTed straight to `{convex}/agent/status` alongside the
/// heartbeat loop's own 5 s POST. Two producers on one row is not a merge —
/// each tick's absences overwrite the other's readings, so the same
/// `cmd_droneStatus` row alternated between a full board/radio/service shape
/// and this small one. It is now folded in-process instead, so there is exactly
/// one producer.
///
/// The whole struct serializes `camelCase` so every field lands in the status
/// mutation in its canonical shape (`mqttConnected`, `throttleState`,
/// `uptimeSeconds`, …) — the `/agent/status` handler passes top-level fields
/// through un-remapped, so a snake_case field would be rejected by the
/// validator. `device_id` → `deviceId` is the auth key the handler reads (the
/// ground station's OWN device id, never the cloud owner id a prior cut wrongly
/// put in an unread `drone_id` field). `version`, `uptimeSeconds`, and `profile`
/// satisfy the same status-mutation contract a drone meets, so the ground
/// station registers as a first-class `ground-station` node in the fleet.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GsHeartbeat {
    pub device_id: String,
    /// The agent version (this crate's package version), so the GS row carries
    /// an agent version like a drone. Required by the status mutation.
    pub version: String,
    /// Seconds since the bridge started. Required by the status mutation.
    pub uptime_seconds: i64,
    /// Wire-contract profile (`ground-station`) so the fleet discriminates this
    /// row as a ground-station node. The GS bridge runs only on that profile.
    pub profile: String,
    /// The mesh role (`direct` | `relay` | `receiver`) when known; omitted while
    /// the bridge has no role to report (the receive plane owns that signal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub uplink: String,
    pub mqtt_connected: bool,
    pub throttle_state: String,
    pub forwarding_video: bool,
    pub forwarding_telemetry: bool,
    pub ts_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<serde_json::Value>,
}

/// A reader of the live vehicle state for heartbeat enrichment. The production
/// impl connects to the state IPC socket; tests inject a fake snapshot.
pub trait StateSnapshotSource: Send + Sync {
    /// The latest vehicle-state snapshot, or `None` when no snapshot has arrived
    /// yet (or no reader is wired). The bridge folds it into the heartbeat's
    /// optional `telemetry` block.
    fn latest(&self) -> Option<serde_json::Value>;
}

/// What the bridge should do in response to an uplink/health transition. The
/// pure decision the live loop then executes. Keeping this as data makes the
/// reconcile logic testable without driving real MQTT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MqttAction {
    /// Tear MQTT down, settle, then (re)connect over the new uplink.
    TeardownThenReconnect,
    /// Tear MQTT down and stay idle (no uplink or not reachable).
    TeardownIdle,
    /// Bring MQTT up (health restored while it was down).
    BringUp,
    /// Nothing to do.
    Noop,
}

/// The uplink-aware cloud relay bridge.
pub struct CloudRelayBridge {
    device_id: String,
    drone_id: Option<String>,
    /// The in-process sink the bridge publishes its relay state onto for the
    /// single `/agent/status` producer to fold. `None` leaves the relay block
    /// unreported (tests, and a bridge run with no heartbeat loop).
    relay_state_sink: Option<watch::Sender<Option<GsHeartbeat>>>,
    relay_transport: TransportConfig,
    flag_path: PathBuf,
    mavlink_sock: PathBuf,
    msp_sock: PathBuf,
    state_source: Option<std::sync::Arc<dyn StateSnapshotSource>>,

    // Live reconciliation state.
    current_uplink: Option<String>,
    internet_reachable: bool,
    throttle: ThrottleState,
    /// The CONFIRMED broker connection state reported on the heartbeat. Driven
    /// only from the relay transport's confirmed-connection flag (set on a
    /// successful broker ConnAck, cleared on disconnect/teardown) — never set
    /// optimistically on spawn, because the relay task surviving is not proof
    /// the broker session is up (rumqttc retries a down broker forever).
    mqtt_connected: bool,
    /// The live confirmed-connection flag published by the current relay run.
    /// `None` while no relay is up; `Some(flag)` carries the transport's atomic
    /// that the event loop drives. The poll loop folds it into `mqtt_connected`.
    relay_connected_flag: Option<Arc<AtomicBool>>,
    /// The receiver the current relay run publishes its transport connection
    /// flag onto. The relay sets it once the transport is dialed (the flag then
    /// flips to true on the broker ConnAck). `None` between relay runs.
    relay_conn_rx: Option<watch::Receiver<Option<Arc<AtomicBool>>>>,
    // When the current relay task was spawned, used to tell a healthy run from a
    // fast exit when the task is reaped.
    relay_started_at: Option<std::time::Instant>,
    // The earliest time a fresh relay may be spawned: one fixed
    // RELAY_RETRY_INTERVAL after a fast exit, so a broken seam does not hot-loop
    // the respawn on every poll tick.
    relay_retry_at: Option<std::time::Instant>,
    // The MSP byte-plane relay task, spawned alongside the MAVLink relay and torn
    // down with it (both share the MAVLink relay's shutdown watch; this handle is
    // the belt-and-braces abort for a wedged publisher). `None` while no relay is up.
    msp_relay_task: Option<tokio::task::JoinHandle<()>>,
    // When this bridge was constructed, the source of the heartbeat's
    // `uptimeSeconds` (the status mutation requires it, the same as a drone).
    started: std::time::Instant,
}

impl CloudRelayBridge {
    /// Build a bridge. `relay_transport` is the dial config for the MAVLink
    /// relay (`ados-{id}` username, broker host/port/password); `drone_id` is
    /// the paired drone id reported on the heartbeat.
    pub fn new(
        device_id: impl Into<String>,
        drone_id: Option<String>,
        relay_transport: TransportConfig,
    ) -> Self {
        Self {
            device_id: device_id.into(),
            drone_id,
            relay_transport,
            flag_path: PathBuf::from(UPLINK_ACTIVE_FLAG),
            mavlink_sock: PathBuf::from(MAVLINK_SOCK),
            msp_sock: PathBuf::from(MSP_SOCK),
            state_source: None,
            relay_state_sink: None,
            current_uplink: None,
            internet_reachable: false,
            throttle: ThrottleState::None,
            mqtt_connected: false,
            relay_connected_flag: None,
            relay_conn_rx: None,
            relay_started_at: None,
            relay_retry_at: None,
            msp_relay_task: None,
            started: std::time::Instant::now(),
        }
    }

    /// Wire the in-process sink the bridge publishes its relay state onto.
    ///
    /// The bridge does NOT POST `/agent/status` itself. Two producers per node
    /// (this one every 30 s with the relay block, the heartbeat loop every 5 s
    /// with the board/radio/service enrichment) meant each tick overwrote the
    /// other's columns with its own absences, so the same row alternated between
    /// two incompatible shapes. There is exactly ONE producer — the heartbeat
    /// loop — and it folds what is published here.
    pub fn with_relay_state_sink(mut self, sink: watch::Sender<Option<GsHeartbeat>>) -> Self {
        self.relay_state_sink = Some(sink);
        self
    }

    /// Override the active-flag path (tests).
    pub fn with_flag_path(mut self, path: PathBuf) -> Self {
        self.flag_path = path;
        self
    }

    /// Wire a live vehicle-state source for heartbeat enrichment.
    pub fn with_state_source(mut self, src: std::sync::Arc<dyn StateSnapshotSource>) -> Self {
        self.state_source = Some(src);
        self
    }

    /// Whether video is currently forwarded (derived from the throttle level).
    pub fn forwarding_video(&self) -> bool {
        self.throttle.forward_video()
    }

    /// Whether telemetry is currently forwarded.
    pub fn forwarding_telemetry(&self) -> bool {
        self.throttle.forward_telemetry()
    }

    /// The current throttle level.
    pub fn throttle_state(&self) -> ThrottleState {
        self.throttle
    }

    // ── Pure decision logic (testable) ──────────────────────────────────

    /// Decide the MQTT action for a new uplink snapshot, updating the bridge's tracked
    /// uplink + reachability.
    pub fn reconcile_uplink(&mut self, snap: Option<&UplinkSnapshot>) -> MqttAction {
        let (new_uplink, reachable) = match snap {
            Some(s) if !s.active_uplink.is_empty() => {
                (Some(s.active_uplink.clone()), s.internet_reachable)
            }
            _ => (None, false),
        };

        let uplink_changed = new_uplink != self.current_uplink;
        let reach_changed = reachable != self.internet_reachable;

        // Fold the data-cap level on every read so a cap transition that rides
        // in on the same file is applied.
        if let Some(s) = snap {
            self.throttle = ThrottleState::from_cap_str(&s.data_cap_state);
        }

        if uplink_changed {
            self.current_uplink = new_uplink.clone();
            self.internet_reachable = reachable;
            return match new_uplink {
                Some(_) if reachable => MqttAction::TeardownThenReconnect,
                _ => MqttAction::TeardownIdle,
            };
        }

        if reach_changed {
            // Same uplink, reachability flipped: mirror `_on_health_changed`.
            self.internet_reachable = reachable;
            if reachable && !self.mqtt_connected {
                return MqttAction::BringUp;
            }
            if !reachable && self.mqtt_connected {
                return MqttAction::TeardownIdle;
            }
        }

        MqttAction::Noop
    }

    /// Apply a data-cap level, returning `true` when the relay must be torn down (the
    /// 100 % heartbeat-only case).
    pub fn apply_data_cap(&mut self, state: ThrottleState) -> bool {
        let previous = self.throttle;
        self.throttle = state;
        match state {
            ThrottleState::Blocked => {
                warn!(previous = previous.as_str(), "cloud_relay.data_cap_blocked");
                true // tear the relay down; heartbeat-only.
            }
            ThrottleState::VideoOff => {
                warn!(
                    previous = previous.as_str(),
                    "cloud_relay.data_cap_throttle"
                );
                false
            }
            _ => {
                info!(previous = previous.as_str(), "cloud_relay.data_cap_ok");
                false
            }
        }
    }

    /// Account for a relay task that has exited, given how long it ran and the
    /// current instant. A fast exit (`ran_for` < [`RELAY_HEALTHY_AFTER`])
    /// schedules the next attempt one fixed [`RELAY_RETRY_INTERVAL`] out, so a
    /// flapping IPC socket or an immediate broker reject cannot hot-loop a
    /// respawn every poll tick — and never waits longer than that, however many
    /// times it has failed. A healthy run (ran at least that long) clears the
    /// retry delay so the relay comes straight back. Either way the relay is no
    /// longer connected.
    ///
    /// Returns the delay before the next spawn is allowed (`0` after a healthy
    /// run). Pure over the bridge's own state so the schedule is unit-testable
    /// without driving real MQTT.
    fn on_relay_exit(&mut self, ran_for: Duration, now: std::time::Instant) -> Duration {
        // A dead relay carries no broker session; drop the confirmed-connection
        // state so a stale flag cannot keep mqtt_connected true after the exit.
        self.mark_relay_down();
        self.relay_started_at = None;
        if ran_for < RELAY_HEALTHY_AFTER {
            self.relay_retry_at = Some(now + RELAY_RETRY_INTERVAL);
            warn!(
                ran_ms = ran_for.as_millis() as u64,
                next_attempt_s = RELAY_RETRY_INTERVAL.as_secs(),
                "cloud_relay.relay_fast_exit"
            );
            RELAY_RETRY_INTERVAL
        } else {
            self.relay_retry_at = None;
            debug!(ran_s = ran_for.as_secs(), "cloud_relay.relay_clean_exit");
            Duration::ZERO
        }
    }

    /// Whether the retry schedule currently permits spawning a fresh relay. A
    /// healthy/first attempt has no pending retry; a fast exit holds the spawn
    /// off until the scheduled instant.
    fn relay_spawn_allowed(&self, now: std::time::Instant) -> bool {
        match self.relay_retry_at {
            Some(at) => now >= at,
            None => true,
        }
    }

    /// Build the GS heartbeat payload from the current relay state, folding the
    /// live vehicle-state telemetry when a state source is wired and has a
    /// snapshot. Returns `None` when there is no uplink to report on (the Python
    /// loop skips posting in that case).
    pub fn build_heartbeat(&self, ts_ms: i64) -> Option<GsHeartbeat> {
        let uplink = self.current_uplink.clone()?;
        let telemetry = self
            .state_source
            .as_ref()
            .and_then(|s| s.latest())
            .map(fold_telemetry);
        Some(GsHeartbeat {
            device_id: self.device_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.started.elapsed().as_secs() as i64,
            profile: "ground-station".to_string(),
            role: None,
            uplink,
            mqtt_connected: self.mqtt_connected,
            throttle_state: self.throttle.as_str().to_string(),
            forwarding_video: self.throttle.forward_video(),
            forwarding_telemetry: self.throttle.forward_telemetry(),
            ts_ms,
            telemetry,
        })
    }

    // ── Live supervision ────────────────────────────────────────────────

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
    fn bring_up_relay(
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
        *relay_task = Some(handle);
        *relay_shutdown = Some(tx);
        self.msp_relay_task = Some(msp_handle);
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
    fn refresh_mqtt_connected(&mut self) {
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
    fn mark_relay_down(&mut self) {
        self.mqtt_connected = false;
        self.relay_connected_flag = None;
        self.relay_conn_rx = None;
        if let Some(handle) = self.msp_relay_task.take() {
            handle.abort();
        }
    }

    /// Publish the relay state for the single `/agent/status` producer to fold.
    ///
    /// A `watch` send, so a heartbeat tick always reads the LATEST state rather
    /// than a queued backlog, and a bridge running with no sink wired (tests) is
    /// a no-op. `None` withdraws the relay block.
    fn publish_relay_state(&self, state: Option<GsHeartbeat>) {
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
async fn teardown_relay(
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
/// Picks the small set of fields the contract forwards; unknown shapes pass
/// through as the raw object so a schema change does not drop data.
fn fold_telemetry(state: serde_json::Value) -> serde_json::Value {
    let obj = match state.as_object() {
        Some(o) => o,
        None => return serde_json::json!({}),
    };
    let pick = |k: &str| obj.get(k).cloned().unwrap_or(serde_json::Value::Null);
    serde_json::json!({
        "armed": pick("armed"),
        "mode": pick("mode"),
        "lat": pick("lat"),
        "lon": pick("lon"),
        "alt_rel": pick("alt_rel"),
        "battery_voltage": pick("voltage_battery"),
        "battery_remaining": pick("battery_remaining"),
        "last_heartbeat": pick("last_heartbeat"),
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A live vehicle-state reader over the state IPC socket (`/run/ados/state.sock`).
/// Connects on demand, reads the newest state snapshot (auto-detecting the v1
/// newline-JSON and v2 length-prefixed msgpack wire forms per frame), and caches
/// it for the heartbeat. The reader holds the latest snapshot behind a mutex so
/// the synchronous [`StateSnapshotSource::latest`] never blocks the heartbeat tick.
pub struct StateIpcReader {
    latest: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

impl StateIpcReader {
    /// Build a reader and spawn its background poll against `path`
    /// (`/run/ados/state.sock` by default). The poll reconnects on any read
    /// error; absence of the socket simply leaves `latest` empty.
    pub fn spawn(path: PathBuf, mut shutdown: watch::Receiver<bool>) -> Self {
        let latest = std::sync::Arc::new(std::sync::Mutex::new(None));
        let store = latest.clone();
        tokio::spawn(async move {
            loop {
                if *shutdown.borrow() {
                    return;
                }
                match tokio::net::UnixStream::connect(&path).await {
                    Ok(stream) => {
                        let mut reader = tokio::io::BufReader::new(stream);
                        loop {
                            tokio::select! {
                                _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
                                read = ados_protocol::state::read_state_value(&mut reader) => {
                                    match read {
                                        // One decoded snapshot (v1 newline-JSON or
                                        // v2 length-prefixed msgpack): cache it.
                                        Ok(Some(v)) => {
                                            if let Ok(mut g) = store.lock() {
                                                *g = Some(v);
                                            }
                                        }
                                        // Clean EOF at a frame boundary → reconnect.
                                        Ok(None) => break,
                                        // Unrecoverable framing/IO error → reconnect.
                                        Err(_) => break,
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => {
                        // No socket yet; wait and retry.
                        tokio::select! {
                            _ = shutdown.changed() => { if *shutdown.borrow() { return; } }
                            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        }
                    }
                }
            }
        });
        Self { latest }
    }
}

impl StateSnapshotSource for StateIpcReader {
    fn latest(&self) -> Option<serde_json::Value> {
        self.latest.lock().ok().and_then(|g| g.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport() -> TransportConfig {
        TransportConfig {
            client_id: "ados-dev1".to_string(),
            host: "mqtt.example".to_string(),
            port: 443,
            ws_path: "/mqtt".to_string(),
            username: "ados-dev1".to_string(),
            password: "k".to_string(),
            inflight: 1000,
            keep_alive: Duration::from_secs(30),
        }
    }

    fn bridge() -> CloudRelayBridge {
        CloudRelayBridge::new("dev1", Some("paired-drone".to_string()), transport())
    }

    struct FixedState(serde_json::Value);
    impl StateSnapshotSource for FixedState {
        fn latest(&self) -> Option<serde_json::Value> {
            Some(self.0.clone())
        }
    }

    #[test]
    fn throttle_string_round_trips_and_full_forwarding_truth_table() {
        // `as_str()` is the wire string carried on the GS heartbeat; it must be
        // the exact inverse of `from_cap_str()` for every level so a level that
        // rides in on the active-flag file round-trips back out on the heartbeat.
        for (cap, level) in [
            ("ok", ThrottleState::None),
            ("warn_80", ThrottleState::Warn),
            ("throttle_95", ThrottleState::VideoOff),
            ("blocked_100", ThrottleState::Blocked),
        ] {
            assert_eq!(ThrottleState::from_cap_str(cap), level);
        }
        // `as_str()` emits the canonical cap string for each non-default level,
        // and `from_cap_str(as_str())` is the identity (the default level emits
        // "none", which parses back to None).
        assert_eq!(ThrottleState::None.as_str(), "none");
        assert_eq!(ThrottleState::Warn.as_str(), "warn_80");
        assert_eq!(ThrottleState::VideoOff.as_str(), "throttle_95");
        assert_eq!(ThrottleState::Blocked.as_str(), "blocked_100");
        for level in [
            ThrottleState::None,
            ThrottleState::Warn,
            ThrottleState::VideoOff,
            ThrottleState::Blocked,
        ] {
            assert_eq!(
                ThrottleState::from_cap_str(level.as_str()),
                level,
                "as_str() round-trips through from_cap_str()"
            );
        }

        // The full forward truth table across every level.
        assert!(ThrottleState::None.forward_video());
        assert!(ThrottleState::None.forward_telemetry());
        assert!(ThrottleState::Warn.forward_video());
        assert!(ThrottleState::Warn.forward_telemetry());
        assert!(!ThrottleState::VideoOff.forward_video());
        assert!(ThrottleState::VideoOff.forward_telemetry());
        assert!(!ThrottleState::Blocked.forward_video());
        assert!(!ThrottleState::Blocked.forward_telemetry());
    }

    #[test]
    fn throttle_maps_cap_strings_and_forwarding_rules() {
        assert_eq!(ThrottleState::from_cap_str("ok"), ThrottleState::None);
        assert_eq!(ThrottleState::from_cap_str("warn_80"), ThrottleState::Warn);
        assert_eq!(
            ThrottleState::from_cap_str("throttle_95"),
            ThrottleState::VideoOff
        );
        assert_eq!(
            ThrottleState::from_cap_str("blocked_100"),
            ThrottleState::Blocked
        );
        // 95 %: video off, telemetry on.
        assert!(!ThrottleState::VideoOff.forward_video());
        assert!(ThrottleState::VideoOff.forward_telemetry());
        // 100 %: both off.
        assert!(!ThrottleState::Blocked.forward_video());
        assert!(!ThrottleState::Blocked.forward_telemetry());
        // ok / warn: both on.
        assert!(ThrottleState::None.forward_video());
        assert!(ThrottleState::Warn.forward_video());
    }

    #[test]
    fn the_relay_retry_interval_is_flat_and_inside_the_recovery_band() {
        // The ladder this replaces doubled 2 s → 300 s, which left a node
        // invisible to the cloud fleet view for up to five minutes after the
        // uplink recovered, with no operator-reachable way to force a retry.
        assert!(RELAY_RETRY_INTERVAL >= Duration::from_secs(2));
        assert!(RELAY_RETRY_INTERVAL <= Duration::from_secs(5));
    }

    #[test]
    fn first_uplink_with_reachability_triggers_teardown_then_reconnect() {
        let mut br = bridge();
        let snap = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        assert_eq!(
            br.reconcile_uplink(Some(&snap)),
            MqttAction::TeardownThenReconnect
        );
        // Same snapshot again → no change.
        assert_eq!(br.reconcile_uplink(Some(&snap)), MqttAction::Noop);
    }

    #[test]
    fn uplink_change_always_tears_down_then_reconnects_over_new_iface() {
        let mut br = bridge();
        let eth = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        br.reconcile_uplink(Some(&eth));
        // Failover to cellular: a different uplink → explicit teardown+reconnect
        // (the whole point — rumqttc would NOT re-bind to wwan0 on its own).
        let wwan = UplinkSnapshot {
            active_uplink: "wwan0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        assert_eq!(
            br.reconcile_uplink(Some(&wwan)),
            MqttAction::TeardownThenReconnect
        );
    }

    #[test]
    fn losing_all_uplinks_tears_down_to_idle() {
        let mut br = bridge();
        let eth = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        br.reconcile_uplink(Some(&eth));
        // File gone → no uplink → idle.
        assert_eq!(br.reconcile_uplink(None), MqttAction::TeardownIdle);
    }

    #[test]
    fn health_flip_on_same_uplink_brings_up_or_tears_down() {
        let mut br = bridge();
        // Start with a reachable uplink and mark MQTT connected.
        let up = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        br.reconcile_uplink(Some(&up));
        br.mqtt_connected = true;
        // Reachability lost on the same uplink → teardown.
        let down = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: false,
            data_cap_state: "ok".to_string(),
        };
        assert_eq!(br.reconcile_uplink(Some(&down)), MqttAction::TeardownIdle);
        br.mqtt_connected = false;
        // Reachability restored → bring up.
        assert_eq!(br.reconcile_uplink(Some(&up)), MqttAction::BringUp);
    }

    #[test]
    fn data_cap_downshift_blocks_and_recovers() {
        let mut br = bridge();
        // 95 %: video off, telemetry kept, relay stays up.
        assert!(!br.apply_data_cap(ThrottleState::VideoOff));
        assert!(!br.forwarding_video());
        assert!(br.forwarding_telemetry());
        // 100 %: heartbeat-only, relay torn down.
        assert!(br.apply_data_cap(ThrottleState::Blocked));
        assert!(!br.forwarding_video());
        assert!(!br.forwarding_telemetry());
        // Recover to ok.
        assert!(!br.apply_data_cap(ThrottleState::None));
        assert!(br.forwarding_video());
        assert!(br.forwarding_telemetry());
    }

    #[test]
    fn cap_only_change_on_a_stable_uplink_is_noop_but_updates_the_throttle() {
        let mut br = bridge();
        // Establish a stable, reachable uplink at "ok".
        let ok = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        br.reconcile_uplink(Some(&ok));
        assert_eq!(br.throttle_state(), ThrottleState::None);
        // The SAME uplink + reachability, only the cap level rises: no MQTT
        // action (the uplink did not change), but the throttle is folded in so
        // the next heartbeat + the forwarding decisions track the cap.
        let warn = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "warn_80".to_string(),
        };
        assert_eq!(br.reconcile_uplink(Some(&warn)), MqttAction::Noop);
        assert_eq!(br.throttle_state(), ThrottleState::Warn);
        // Rise further to the video-off threshold on the same stable uplink.
        let throttle95 = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "throttle_95".to_string(),
        };
        assert_eq!(br.reconcile_uplink(Some(&throttle95)), MqttAction::Noop);
        assert_eq!(br.throttle_state(), ThrottleState::VideoOff);
        assert!(!br.forwarding_video());
        assert!(br.forwarding_telemetry());
    }

    #[test]
    fn data_cap_rides_in_on_the_uplink_snapshot() {
        let mut br = bridge();
        let snap = UplinkSnapshot {
            active_uplink: "wwan0".to_string(),
            internet_reachable: true,
            data_cap_state: "throttle_95".to_string(),
        };
        br.reconcile_uplink(Some(&snap));
        // The cap level carried on the same file is applied.
        assert_eq!(br.throttle_state(), ThrottleState::VideoOff);
        assert!(!br.forwarding_video());
        assert!(br.forwarding_telemetry());
    }

    #[test]
    fn heartbeat_is_none_without_an_uplink_and_carries_relay_state_with_one() {
        let mut br = bridge();
        // No uplink yet → no heartbeat.
        assert!(br.build_heartbeat(1).is_none());
        // After an uplink, the heartbeat carries the relay's forwarding state.
        let snap = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "throttle_95".to_string(),
        };
        br.reconcile_uplink(Some(&snap));
        let hb = br.build_heartbeat(1234).unwrap();
        assert_eq!(hb.uplink, "eth0");
        // The heartbeat carries the ground station's OWN device id, serialized
        // as `deviceId` to match the /agent/status auth contract (not the cloud
        // owner id the prior cut wrongly sent).
        assert_eq!(hb.device_id, "dev1");
        assert_eq!(hb.throttle_state, "throttle_95");
        assert!(!hb.forwarding_video);
        assert!(hb.forwarding_telemetry);
        assert_eq!(hb.ts_ms, 1234);
        // No state source → no telemetry block.
        assert!(hb.telemetry.is_none());
        // The wire payload keys the device on `deviceId` (the /agent/status auth
        // contract the handler reads), never the old unread `drone_id`, and the
        // whole document is camelCase so it lands in the status mutation in its
        // canonical shape (the handler does not remap top-level fields).
        let wire = serde_json::to_value(&hb).unwrap();
        assert_eq!(wire["deviceId"], "dev1");
        assert!(wire.get("drone_id").is_none());
        assert_eq!(wire["throttleState"], "throttle_95");
        assert_eq!(wire["forwardingVideo"], false);
        assert_eq!(wire["forwardingTelemetry"], true);
        assert!(wire.get("mqtt_connected").is_none());
        // The status-mutation contract a drone meets: version + uptimeSeconds +
        // a discriminating profile, so the GS registers as a ground-station node.
        assert_eq!(wire["profile"], "ground-station");
        assert!(wire.get("version").and_then(|v| v.as_str()).is_some());
        assert!(wire.get("uptimeSeconds").and_then(|v| v.as_i64()).is_some());
        // No role to report yet → the field is omitted, not null.
        assert!(wire.get("role").is_none());
    }

    #[test]
    fn the_relay_state_is_published_for_the_single_status_producer() {
        // The bridge must not be a second /agent/status producer: it hands its
        // relay block to the heartbeat loop, which folds it onto the one POST.
        let (tx, rx) = watch::channel(None);
        let mut br = bridge().with_relay_state_sink(tx);
        let snap = UplinkSnapshot {
            active_uplink: "wlan0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        br.reconcile_uplink(Some(&snap));
        br.publish_relay_state(br.build_heartbeat(99));
        let published = rx.borrow().clone().expect("relay state is published");
        assert_eq!(published.uplink, "wlan0");
        assert_eq!(published.profile, "ground-station");
        assert_eq!(published.ts_ms, 99);
        // Stopping withdraws the block, so a dead bridge's last forwarding
        // state is not folded onto every later heartbeat.
        br.publish_relay_state(None);
        assert!(rx.borrow().is_none());
    }

    #[test]
    fn heartbeat_folds_live_telemetry_when_a_state_source_is_wired() {
        let state = serde_json::json!({
            "armed": true, "mode": "GUIDED", "lat": 12.97, "lon": 77.59,
            "alt_rel": 30.0, "voltage_battery": 16.2, "battery_remaining": 88,
            "last_heartbeat": 1700000000.0
        });
        let mut br = bridge().with_state_source(std::sync::Arc::new(FixedState(state)));
        let snap = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        br.reconcile_uplink(Some(&snap));
        let hb = br.build_heartbeat(1).unwrap();
        let t = hb.telemetry.unwrap();
        assert_eq!(t["armed"], true);
        assert_eq!(t["mode"], "GUIDED");
        assert_eq!(t["battery_voltage"], 16.2);
        assert_eq!(t["battery_remaining"], 88);
    }

    #[test]
    fn every_fast_relay_exit_waits_the_same_fixed_interval_with_no_cap() {
        let mut br = bridge();
        // Simulate a reachable uplink + an "up" relay.
        let snap = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        br.reconcile_uplink(Some(&snap));
        br.mqtt_connected = true;
        br.relay_started_at = Some(std::time::Instant::now());

        // The relay exits almost immediately (a flapping IPC socket): the
        // respawn is deferred by exactly one interval.
        let mut now = std::time::Instant::now();
        let delay = br.on_relay_exit(Duration::from_millis(5), now);
        assert_eq!(delay, RELAY_RETRY_INTERVAL);
        assert!(!br.mqtt_connected, "a dead relay is not connected");
        assert!(
            !br.relay_spawn_allowed(now),
            "a respawn is held off until the scheduled instant"
        );
        // The schedule clears once the interval has elapsed.
        assert!(br.relay_spawn_allowed(now + RELAY_RETRY_INTERVAL));

        // And it NEVER grows, however long the seam stays broken: a broker that
        // rejects for the first minutes of a cold boot must not push the node
        // out of the fleet view for minutes after the uplink recovers.
        for attempt in 0..60 {
            now += RELAY_RETRY_INTERVAL;
            assert_eq!(
                br.on_relay_exit(Duration::from_millis(5), now),
                RELAY_RETRY_INTERVAL,
                "attempt {attempt} must wait the same fixed interval"
            );
            assert!(
                br.relay_spawn_allowed(now + RELAY_RETRY_INTERVAL),
                "attempt {attempt} must be retried, not capped out"
            );
        }
    }

    #[test]
    fn a_healthy_relay_run_clears_the_retry_hold() {
        let mut br = bridge();
        let t0 = std::time::Instant::now();
        br.on_relay_exit(Duration::from_millis(1), t0);
        br.on_relay_exit(Duration::from_millis(1), t0);
        assert!(
            !br.relay_spawn_allowed(t0),
            "a fast exit holds the respawn off"
        );

        // A run that lasted past the healthy threshold clears the retry hold so
        // the relay comes straight back.
        let delay = br.on_relay_exit(RELAY_HEALTHY_AFTER + Duration::from_secs(1), t0);
        assert_eq!(delay, Duration::ZERO);
        assert!(
            br.relay_spawn_allowed(t0),
            "no retry hold after a clean run"
        );
        assert!(!br.mqtt_connected);
    }

    #[test]
    fn relay_spawn_allowed_with_no_schedule_is_true() {
        let br = bridge();
        // A fresh bridge has no pending retry, so the first spawn is allowed.
        assert!(br.relay_spawn_allowed(std::time::Instant::now()));
    }

    #[test]
    fn snapshot_reads_the_active_flag_file_with_cap_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uplink-active");
        // Absent → None (idle).
        assert!(UplinkSnapshot::read(&path).is_none());
        // The ados-net writer's body shape, including the additive cap field.
        std::fs::write(
            &path,
            r#"{"active_uplink":"wwan0","internet_reachable":true,"timestamp_ms":1700000000000,"data_cap_state":"warn_80"}"#,
        )
        .unwrap();
        let s = UplinkSnapshot::read(&path).unwrap();
        assert_eq!(s.active_uplink, "wwan0");
        assert!(s.internet_reachable);
        assert_eq!(s.data_cap_state, "warn_80");
        // A legacy body without the cap field defaults to "ok".
        std::fs::write(
            &path,
            r#"{"active_uplink":"eth0","internet_reachable":true,"timestamp_ms":1}"#,
        )
        .unwrap();
        let s = UplinkSnapshot::read(&path).unwrap();
        assert_eq!(s.data_cap_state, "ok");
    }

    /// Publish a connection flag onto a bridge as if a relay run just dialed,
    /// returning the atomic the (fake) transport event loop would drive. The
    /// flag starts `false` exactly like a freshly-dialed rumqttc transport that
    /// has not yet seen a ConnAck.
    fn arm_relay_flag(br: &mut CloudRelayBridge) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        let (tx, rx) = watch::channel::<Option<Arc<AtomicBool>>>(Some(flag.clone()));
        // Keep the sender alive for the channel's lifetime (the real relay task
        // holds it); leaking it here mirrors that ownership in a unit test.
        std::mem::forget(tx);
        br.relay_conn_rx = Some(rx);
        flag
    }

    #[test]
    fn dialing_relay_does_not_report_connected_until_the_broker_acks() {
        // The core connect-lie fix: a relay that has spawned and is DIALING a
        // down / never-acking broker must NOT report mqtt_connected:true. The
        // transport's confirmed-connection flag stays false (no ConnAck), so the
        // bridge keeps mqtt_connected false.
        let mut br = bridge();
        let flag = arm_relay_flag(&mut br); // flag == false (broker not acked)

        br.refresh_mqtt_connected();
        assert!(
            !br.mqtt_connected,
            "a dialing relay with no broker ConnAck is not connected"
        );

        // The broker finally accepts the session (ConnAck → flag true): now the
        // bridge reports connected.
        flag.store(true, Ordering::Release);
        br.refresh_mqtt_connected();
        assert!(br.mqtt_connected, "a confirmed ConnAck reports connected");

        // The broker drops the session (Disconnect/poll-error → flag false):
        // the bridge stops reporting connected on the next refresh.
        flag.store(false, Ordering::Release);
        br.refresh_mqtt_connected();
        assert!(
            !br.mqtt_connected,
            "a dropped broker session is no longer connected"
        );
    }

    #[test]
    fn no_relay_flag_means_not_connected() {
        // With no relay run at all there is no flag; the link is down.
        let mut br = bridge();
        assert!(br.relay_conn_rx.is_none());
        br.refresh_mqtt_connected();
        assert!(!br.mqtt_connected);
    }

    #[test]
    fn mark_relay_down_clears_a_confirmed_connection() {
        // A previously-confirmed connection must be cleared on teardown/reap so
        // the next heartbeat cannot report a stale broker session.
        let mut br = bridge();
        let flag = arm_relay_flag(&mut br);
        flag.store(true, Ordering::Release);
        br.refresh_mqtt_connected();
        assert!(br.mqtt_connected, "confirmed connection before teardown");

        br.mark_relay_down();
        assert!(!br.mqtt_connected, "teardown clears the connection");
        assert!(br.relay_connected_flag.is_none());
        assert!(br.relay_conn_rx.is_none());
        // Even if the (now-detached) flag is still true, a refresh stays down:
        // the bridge no longer holds a handle to it.
        flag.store(true, Ordering::Release);
        br.refresh_mqtt_connected();
        assert!(
            !br.mqtt_connected,
            "a detached flag cannot revive the report"
        );
    }

    #[test]
    fn relay_exit_reports_disconnected_even_if_the_stale_flag_lingers() {
        // A relay task that exits (fast or clean) must report disconnected. The
        // transport's flag may briefly linger true after the task ends, but the
        // bridge drops its handle on exit, so the heartbeat goes false.
        let mut br = bridge();
        let flag = arm_relay_flag(&mut br);
        flag.store(true, Ordering::Release);
        br.refresh_mqtt_connected();
        assert!(br.mqtt_connected);

        // The relay task is reaped; the flag is still (stale) true.
        let now = std::time::Instant::now();
        br.on_relay_exit(Duration::from_millis(5), now);
        assert!(!br.mqtt_connected, "a reaped relay is not connected");
        // A subsequent refresh cannot resurrect the report from the stale flag.
        br.refresh_mqtt_connected();
        assert!(!br.mqtt_connected);
    }

    #[test]
    fn heartbeat_reports_mqtt_disconnected_while_the_broker_is_down() {
        // End to end on the payload: a dialing relay over a down broker yields a
        // heartbeat carrying mqttConnected:false, never a connect-lie.
        let mut br = bridge();
        let snap = UplinkSnapshot {
            active_uplink: "eth0".to_string(),
            internet_reachable: true,
            data_cap_state: "ok".to_string(),
        };
        br.reconcile_uplink(Some(&snap));
        // Relay dialing a down broker: flag present but false (no ConnAck).
        arm_relay_flag(&mut br);
        br.refresh_mqtt_connected();

        let hb = br.build_heartbeat(1).unwrap();
        assert!(!hb.mqtt_connected);
        let wire = serde_json::to_value(&hb).unwrap();
        assert_eq!(wire["mqttConnected"], false);
    }
}
