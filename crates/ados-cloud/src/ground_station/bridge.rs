//! The uplink-aware cloud relay bridge.
//!
//! Lifecycle + decision logic for the ground-station cloud relay. The decision
//! surface (what to tear down / bring up / forward on each uplink, health, and
//! data-cap transition) is factored into pure methods so it is unit-testable
//! with no MQTT and no network; the live supervision (`start`/`run`) drives the
//! MAVLink relay task and the 30 s GS status heartbeat.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::mqtt::transport::TransportConfig;
use crate::mqtt::{MavlinkMqttRelay, MspMqttRelay};

mod run;
use run::*;

mod state_reader;
pub use state_reader::StateIpcReader;

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
    // The WebRTC signaling lane, spawned and torn down with the MAVLink relay.
    // `None` while no relay is up.
    signaling_task: Option<tokio::task::JoinHandle<()>>,
    // Whether the data cap currently lets the signaling lane open a video
    // stream. Refreshed from the throttle level on every poll tick; an offer
    // arriving while it is false is refused rather than streamed.
    video_allowed: Arc<AtomicBool>,
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
            signaling_task: None,
            video_allowed: Arc::new(AtomicBool::new(true)),
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
}

#[cfg(test)]
mod tests;
