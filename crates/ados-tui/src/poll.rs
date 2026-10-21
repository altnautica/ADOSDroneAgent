//! Background polling of the agent, and the render loop's view of the results.
//!
//! Two threads, so a slow per-tab route never delays the global status read:
//!
//! * the **global** poll reads setup status, native status, the consolidated
//!   status and the per-profile routes every refresh interval (or on demand);
//! * the **resource** poll reads whatever the active screen subscribed to via
//!   [`Poller::set_active`], each on its own period, plus one-off
//!   [`Poller::fetch_now`] reads.
//!
//! Both REST reads are blocking calls with a per-request timeout; results come
//! back over a channel and the render loop only renders and handles keys, so a
//! hung agent never freezes input.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use ados_protocol::rest::{RestClient, RestError};
use serde_json::Value;

use crate::model::{present, Dashboard, History, ProfileRoutes, RouteSet, CAPTURE_STATE_PATH};

/// One poll of the agent: the setup status the dashboard is built from, the
/// best-effort native status carrying the FC transport/heartbeat split, and the
/// best-effort consolidated status carrying the service list.
pub struct PollResult {
    /// The setup status, or the read error (kept typed so a 401/403 can open
    /// the auth prompt).
    pub setup: Result<Value, RestError>,
    pub fc_status: Option<Value>,
    pub status_full: Option<Value>,
    /// The per-profile optional routes (sensors, clock, storage, GNSS, LoRa,
    /// radios, capture), polled only for the profile that shows them.
    pub profile: ProfileRoutes,
}

/// A per-screen resource the resource poller can read.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceId {
    Telemetry,
    Snapshot,
    Battery,
    Wfb,
    Lora,
    LoraBeacons,
    Mesh,
    ReceiverCombined,
    ReceiverRelays,
    Gnss,
    Gamepads,
    BtPaired,
    Invites,
    GsDisplay,
    VideoConfig,
    VideoCameras,
    VideoLatency,
    Params,
    Plugins,
    PluginsCatalog,
    PluginDetail(String),
    PluginReadiness(String),
    PluginState(String),
    PluginConfig(String),
    /// A plugin's own `en` string bundle (labels of its declared parameters).
    PluginLocale(String),
    Services,
    System,
    Peripherals,
    Config,
    ConfigSchema,
    McpStatus,
    PairingInfo,
    PinStatus,
    WifiStatus,
    WifiConfigured,
    MacAdapters,
    ModemStatus,
    ModemConfig,
    DisplayOptions,
    /// The world-engine capture state (visor).
    CaptureState,
    /// `GET /api/logs` (the poll fallback when the stream is down).
    LogsPoll,
    /// `GET /api/v1/setup/hardware-check` (the System tab's item list).
    HardwareCheck,
    /// `GET /api/v1/setup/status` read by a settings page that needs fields
    /// the global dashboard model does not keep (profile source, cloud
    /// choice, remote access, regulatory posture, LAN host).
    SetupStatus,
    /// `GET /api/v1/sensors` (the Sensors settings page's hub card).
    SensorsStatus,
    /// `GET /api/v1/clock` (the Sensors settings page's clock sources).
    ClockStatus,
}

/// A subscription: read `path` into `id` every `period`; `Duration::ZERO`
/// reads once on activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sub {
    pub id: ResourceId,
    pub path: String,
    pub period: Duration,
}

impl Sub {
    pub fn new(id: ResourceId, path: impl Into<String>, period: Duration) -> Self {
        Self {
            id,
            path: path.into(),
            period,
        }
    }
    /// Read once when the screen activates.
    pub fn once(id: ResourceId, path: impl Into<String>) -> Self {
        Self::new(id, path, Duration::ZERO)
    }
    pub fn every_ms(id: ResourceId, path: impl Into<String>, ms: u64) -> Self {
        Self::new(id, path, Duration::from_millis(ms))
    }
}

/// What the poller sends back.
pub enum PollMsg {
    Global(Box<PollResult>),
    Resource(ResourceId, Result<Value, RestError>),
}

/// Fetch the optional routes the reported profile shows. A failed request or a
/// `present: false` body leaves the field `None`.
fn poll_profile(client: &RestClient, setup: &Result<Value, RestError>) -> ProfileRoutes {
    let profile = setup
        .as_ref()
        .ok()
        .and_then(|s| s.get("profile"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let set = RouteSet::for_profile(profile);
    let get = |want: bool, path: &str| {
        if want {
            client.get_json(path).ok().and_then(present)
        } else {
            None
        }
    };
    ProfileRoutes {
        sensors: get(set.sensors, "/api/v1/sensors"),
        clock: get(set.clock, "/api/v1/clock"),
        storage: get(set.storage, "/api/v1/storage"),
        gnss: get(set.gnss, "/api/v1/gnss"),
        lora: get(set.lora, "/api/v1/lora"),
        radio: get(set.radio, "/api/v1/radio/adapters"),
        capture: get(set.capture, CAPTURE_STATE_PATH),
    }
}

enum GlobalCtl {
    Refresh,
    Interval(Duration),
}

enum ResCtl {
    Active(Vec<Sub>),
    Now(ResourceId),
}

/// Handle to the poller threads. Both exit once the handle is dropped.
pub struct Poller {
    rx: Receiver<PollMsg>,
    global: Sender<GlobalCtl>,
    res: Sender<ResCtl>,
}

impl Poller {
    /// Start polling now, then every `interval`.
    pub fn spawn(client: RestClient, interval: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let (global, global_rx) = mpsc::channel();
        let (res, res_rx) = mpsc::channel();
        spawn_global(client.clone(), interval, tx.clone(), global_rx);
        spawn_resources(client, tx, res_rx);
        Self { rx, global, res }
    }

    /// Run the global poll now instead of at the next interval.
    pub fn refresh(&self) {
        let _ = self.global.send(GlobalCtl::Refresh);
    }

    /// Change the global poll interval.
    pub fn set_interval(&self, interval: Duration) {
        let _ = self.global.send(GlobalCtl::Interval(interval));
    }

    /// Replace the resource subscriptions. New ones are read at once.
    pub fn set_active(&self, subs: Vec<Sub>) {
        let _ = self.res.send(ResCtl::Active(subs));
    }

    /// Read one subscribed resource now.
    pub fn fetch_now(&self, id: ResourceId) {
        let _ = self.res.send(ResCtl::Now(id));
    }

    /// Every message that landed since the last call, oldest first. Never blocks.
    pub fn drain(&self) -> impl Iterator<Item = PollMsg> + '_ {
        self.rx.try_iter()
    }
}

fn spawn_global(client: RestClient, mut interval: Duration, tx: Sender<PollMsg>, ctl: Receiver<GlobalCtl>) {
    thread::spawn(move || loop {
        let setup = client.setup_status();
        let fc_status = client.status().ok();
        let status_full = client.get_json("/api/status/full").ok();
        let profile = poll_profile(&client, &setup);
        let result = PollResult {
            setup,
            fc_status,
            status_full,
            profile,
        };
        if tx.send(PollMsg::Global(Box::new(result))).is_err() {
            return;
        }
        let deadline = Instant::now() + interval;
        loop {
            let wait = deadline.saturating_duration_since(Instant::now());
            match ctl.recv_timeout(wait) {
                Ok(GlobalCtl::Refresh) | Err(RecvTimeoutError::Timeout) => break,
                Ok(GlobalCtl::Interval(i)) => interval = i,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        // Requests queued while this poll ran are served by the next one.
        while let Ok(msg) = ctl.try_recv() {
            if let GlobalCtl::Interval(i) = msg {
                interval = i;
            }
        }
    });
}

fn spawn_resources(client: RestClient, tx: Sender<PollMsg>, ctl: Receiver<ResCtl>) {
    thread::spawn(move || {
        let mut subs: Vec<Sub> = Vec::new();
        // Next due time per subscription; `None` = a once-read already done.
        let mut due: HashMap<ResourceId, Option<Instant>> = HashMap::new();
        loop {
            let now = Instant::now();
            // Read everything due, in subscription order.
            for sub in &subs {
                let Some(Some(at)) = due.get(&sub.id) else { continue };
                if *at > now {
                    continue;
                }
                let result = client.get_json(&sub.path);
                if tx.send(PollMsg::Resource(sub.id.clone(), result)).is_err() {
                    return;
                }
                let next = if sub.period.is_zero() {
                    None
                } else {
                    Some(Instant::now() + sub.period)
                };
                due.insert(sub.id.clone(), next);
            }
            let next_due = due.values().flatten().min().copied();
            let wait = next_due
                .map(|t| t.saturating_duration_since(Instant::now()))
                .unwrap_or(Duration::from_secs(3600));
            match ctl.recv_timeout(wait) {
                Ok(ResCtl::Active(new)) => {
                    let now = Instant::now();
                    let mut next_due = HashMap::new();
                    for sub in &new {
                        // A subscription carried over keeps its schedule (a
                        // once-read is not repeated); a new one is read now.
                        let kept = subs.iter().any(|s| s == sub);
                        let at = match (kept, due.get(&sub.id)) {
                            (true, Some(at)) => *at,
                            _ => Some(now),
                        };
                        next_due.insert(sub.id.clone(), at);
                    }
                    subs = new;
                    due = next_due;
                }
                Ok(ResCtl::Now(id)) => {
                    if due.contains_key(&id) {
                        due.insert(id, Some(Instant::now()));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// The status text while the agent refuses this session's credentials.
pub const LOCKED: &str = "Locked: API key or dashboard PIN required";

/// What the render loop knows about the agent's global status.
#[derive(Default)]
pub struct AgentView {
    /// The dashboard from the last successful poll.
    pub dash: Option<Dashboard>,
    /// Trend buffers of verified telemetry, one sample per successful poll.
    pub history: History,
    /// Why the latest poll failed; cleared by the next success.
    pub error: Option<String>,
    /// The HTTP status of the latest failed poll, when the agent answered.
    pub error_code: Option<u16>,
    /// Wall-clock time of the last successful poll. Advanced only on success.
    pub refreshed: Option<String>,
    last_success: Option<Instant>,
}

impl AgentView {
    /// Fold one poll result in. A success rebuilds the dashboard and moves the
    /// refreshed clock to `clock`; a failure only records the error and leaves
    /// the last snapshot, and the time it was taken, as they were.
    pub fn apply(&mut self, result: PollResult, now: Instant, clock: String) {
        match result.setup {
            Ok(setup) => {
                let mut dash = Dashboard::from_status(&setup);
                if let Some(fc) = &result.fc_status {
                    dash.merge_fc_status(fc);
                }
                if let Some(full) = &result.status_full {
                    dash.merge_services(full);
                    dash.merge_telemetry(full);
                }
                dash.merge_profile(&result.profile);
                self.history.record(&dash);
                self.dash = Some(dash);
                self.error = None;
                self.error_code = None;
                self.refreshed = Some(clock);
                self.last_success = Some(now);
            }
            Err(e) => {
                self.error_code = e.code();
                // The agent answered: a refusal is a lock, not an outage.
                self.error = Some(if matches!(e.code(), Some(401) | Some(403)) {
                    LOCKED.to_string()
                } else {
                    format!("Agent unreachable: {e}")
                });
            }
        }
    }

    /// Whether the last poll was refused for missing credentials.
    pub fn locked(&self) -> bool {
        matches!(self.error_code, Some(401) | Some(403))
    }

    /// Whether the shown snapshot is older than `after`.
    pub fn is_stale(&self, now: Instant, after: Duration) -> bool {
        self.last_success
            .is_some_and(|t| now.saturating_duration_since(t) > after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(battery: f64) -> PollResult {
        PollResult {
            setup: Ok(json!({
                "version": "0.99.108",
                "profile": "drone",
                "mavlink": {"connected": true},
                "telemetry": {"battery": {"remaining": battery}}
            })),
            fc_status: None,
            status_full: None,
            profile: ProfileRoutes::default(),
        }
    }

    #[test]
    fn a_failed_poll_keeps_the_snapshot_time_and_records_the_error() {
        let t0 = Instant::now();
        let mut view = AgentView::default();
        view.apply(ok(82.0), t0, "12:00:00".into());
        assert_eq!(view.refreshed.as_deref(), Some("12:00:00"));

        let failed = PollResult {
            setup: Err(RestError::Transport("connection refused".into())),
            fc_status: None,
            status_full: None,
            profile: ProfileRoutes::default(),
        };
        view.apply(failed, t0 + Duration::from_secs(2), "12:00:02".into());
        assert_eq!(
            view.refreshed.as_deref(),
            Some("12:00:00"),
            "the refreshed clock only moves when data actually refreshed"
        );
        assert_eq!(
            view.error.as_deref(),
            Some("Agent unreachable: transport error: connection refused")
        );
        assert_eq!(view.dash.as_ref().and_then(|d| d.battery), Some(82.0));
        assert!(!view.is_stale(t0 + Duration::from_secs(2), Duration::from_secs(6)));
        assert!(view.is_stale(t0 + Duration::from_secs(7), Duration::from_secs(6)));

        view.apply(ok(80.0), t0 + Duration::from_secs(8), "12:00:08".into());
        assert_eq!(view.error, None);
        assert_eq!(view.refreshed.as_deref(), Some("12:00:08"));
        assert_eq!(view.history.battery, vec![82.0, 80.0]);
    }

    #[test]
    fn an_auth_failure_keeps_its_status_code() {
        let mut view = AgentView::default();
        let failed = PollResult {
            setup: Err(RestError::Status {
                code: 401,
                body: Some(json!({"detail": "missing key"})),
            }),
            fc_status: None,
            status_full: None,
            profile: ProfileRoutes::default(),
        };
        view.apply(failed, Instant::now(), "12:00:00".into());
        assert_eq!(view.error_code, Some(401));
    }

    #[test]
    fn arm_state_and_mode_come_from_the_consolidated_status() {
        // The native setup status carries no telemetry; the router snapshot
        // rides `/api/status/full`.
        let mut view = AgentView::default();
        let result = PollResult {
            setup: Ok(json!({"profile": "drone", "mavlink": {"connected": true}})),
            fc_status: None,
            status_full: Some(json!({
                "services": [],
                "telemetry": {"armed": false, "mode": "STABILIZE", "battery": {"remaining": 100}}
            })),
            profile: ProfileRoutes::default(),
        };
        view.apply(result, Instant::now(), "12:00:00".into());
        let dash = view.dash.unwrap();
        assert_eq!(dash.armed, Some(false));
        assert_eq!(dash.mode.as_deref(), Some("STABILIZE"));
        assert_eq!(dash.battery, Some(100.0));

        // A dead FC link never shows the snapshot's leftovers.
        let mut view = AgentView::default();
        let result = PollResult {
            setup: Ok(json!({"profile": "drone", "mavlink": {"connected": false}})),
            fc_status: None,
            status_full: Some(json!({"telemetry": {"armed": false, "mode": "STABILIZE"}})),
            profile: ProfileRoutes::default(),
        };
        view.apply(result, Instant::now(), "12:00:00".into());
        assert_eq!(view.dash.unwrap().armed, None);
    }

    #[test]
    fn the_native_status_is_merged_into_the_built_dashboard() {
        let mut view = AgentView::default();
        let result = PollResult {
            setup: Ok(json!({"profile": "drone", "mavlink": {"connected": false}})),
            fc_status: Some(json!({"fcVariant": "inav"})),
            status_full: Some(json!({"services": [{"state": "failed"}]})),
            profile: ProfileRoutes::default(),
        };
        view.apply(result, Instant::now(), "12:00:00".into());
        let dash = view.dash.unwrap();
        assert_eq!(dash.fc_variant.as_deref(), Some("inav"));
        assert_eq!((dash.services_running, dash.services_total), (0, 1));
    }
}
