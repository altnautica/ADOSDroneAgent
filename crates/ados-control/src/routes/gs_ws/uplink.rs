//! `/ws/uplink`: the active-uplink event stream.

use super::*;

/// The `/ws/uplink` upgrade entry point. Resolves the handshake auth and the
/// profile gate, then drives the polling loop on the upgraded socket.
pub async fn ws_uplink(
    State(state): State<AppState>,
    headers: HeaderMap,
    caller: Option<Extension<CallerClass>>,
    ws: WebSocketUpgrade,
) -> Response {
    let auth = decide_ws_auth(&state, &headers, SCOPE_UPLINK_EVENTS, caller_of(caller));
    let Some((ws, auth)) = upgrade_with(ws, auth) else {
        return ws_reject();
    };
    ws.on_upgrade(move |socket| uplink_loop(socket, state, auth))
}

/// Drive the uplink stream: profile-gate after accept (so a wrong-profile node
/// closes 1008 the way the residual handler did), then poll the sentinel and emit
/// on change, with the keepalive beat, until the client disconnects.
pub(super) async fn uplink_loop(mut socket: WebSocket, state: AppState, _auth: WsAuth) {
    if !is_ground_station(&state) {
        close_profile_mismatch(&mut socket).await;
        return;
    }

    let mut source = UplinkSource::new(state.run_dir.join("uplink-active"));
    let mut last_sent: Option<Value> = None;
    let mut poll = tokio::time::interval(UPLINK_POLL_INTERVAL);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut keepalive = keepalive_ticker();
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                if !client_still_open(&incoming) {
                    return;
                }
                last_heard = Instant::now();
            }
            _ = keepalive.tick() => {
                if !keepalive_beat(&mut socket, last_heard).await {
                    return;
                }
            }
            _ = poll.tick() => {
                let Some(payload) = source.snapshot().await else {
                    continue;
                };
                if last_sent.as_ref() != Some(&payload) {
                    if !send_json(&mut socket, &payload).await {
                        return;
                    }
                    last_sent = Some(payload);
                }
            }
        }
    }
}

/// The live uplink source: the router's sentinel plus a cached answer to whether
/// the router unit is running.
pub(super) struct UplinkSource {
    pub(super) flag: PathBuf,
    pub(super) router_running: Option<bool>,
    pub(super) router_checked_at: Option<Instant>,
}

impl UplinkSource {
    pub(super) fn new(flag: PathBuf) -> Self {
        Self {
            flag,
            router_running: None,
            router_checked_at: None,
        }
    }

    /// The current uplink frame, or `None` when the sentinel cannot be read.
    pub(super) async fn snapshot(&mut self) -> Option<Value> {
        let due = self
            .router_checked_at
            .is_none_or(|at| at.elapsed() >= ROUTER_RECHECK_INTERVAL);
        if due {
            self.router_running = crate::probe::unit_state(UPLINK_ROUTER_UNIT).await;
            self.router_checked_at = Some(Instant::now());
        }
        uplink_ws_payload(&read_uplink_flag(&self.flag).await, self.router_running)
    }
}

/// What the active-uplink sentinel holds right now.
pub(super) enum UplinkFlag {
    /// The router has an active uplink; the body it wrote.
    Present(Map<String, Value>),
    /// No file: the router has no uplink (or has never run).
    Absent,
    /// The file exists but cannot be read or is not a JSON object.
    Unreadable,
}

pub(super) async fn read_uplink_flag(path: &Path) -> UplinkFlag {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(body)) => UplinkFlag::Present(body),
            _ => UplinkFlag::Unreadable,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => UplinkFlag::Absent,
        Err(_) => UplinkFlag::Unreadable,
    }
}

/// Shape the sentinel into the uplink WS frame. An absent sentinel is the
/// "no uplink" frame; an unreadable one yields nothing this poll. `stale` is true
/// unless systemd confirms the router unit is running: a stopped or crashed
/// router leaves its last sentinel behind, which is not the current state.
pub(super) fn uplink_ws_payload(flag: &UplinkFlag, router_running: Option<bool>) -> Option<Value> {
    let no_uplink = Map::new();
    let body = match flag {
        UplinkFlag::Present(body) => body,
        UplinkFlag::Absent => &no_uplink,
        UplinkFlag::Unreadable => return None,
    };
    let field = |key: &str, keep: fn(&Value) -> bool| {
        body.get(key)
            .filter(|v| keep(v))
            .cloned()
            .unwrap_or(Value::Null)
    };
    Some(json!({
        "kind": "health_changed",
        "active_uplink": field("active_uplink", Value::is_string),
        "available": body
            .get("available")
            .filter(|v| v.is_array())
            .cloned()
            .unwrap_or_else(|| json!([])),
        "internet_reachable": body
            .get("internet_reachable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        "data_cap_state": field("data_cap_state", Value::is_string),
        "timestamp_ms": field("timestamp_ms", Value::is_u64),
        "stale": router_running != Some(true),
    }))
}
