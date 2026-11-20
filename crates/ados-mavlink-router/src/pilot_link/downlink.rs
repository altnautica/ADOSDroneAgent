//! The pilot state downlink (drone profile): the pilot's `state`, trimmed to
//! one aux frame, goes to the ground every [`DOWNLINK_PERIOD_MS`] on the aux
//! `Pilot` channel, and its tally is published in the router status as
//! `pilot_downlink`.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use ados_protocol::aux_egress::AuxEgress;
use ados_protocol::aux_mux::AuxChannel;

/// The pilot state goes down the aux link this often.
pub const DOWNLINK_PERIOD_MS: u64 = 500;

/// The largest state body one aux frame carries: the payload's first byte is
/// the flight controller's system id.
pub const DOWNLINK_MAX_BODY: usize = ados_protocol::aux_mux::AUX_MAX_PAYLOAD - 1;

/// The lane a downlinked state leaves on: the aux egress's `Pilot` channel on
/// a drone, an in-process radio in the simulation (`tests/dst`).
pub trait StateEgress: Send + Sync {
    /// Send one downlink payload; `Err` says why it did not leave.
    fn send_state<'a>(
        &'a self,
        payload: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

impl StateEgress for AuxEgress {
    fn send_state<'a>(
        &'a self,
        payload: &'a [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.send(AuxChannel::Pilot, payload)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Who the downlinked state is from, and the lane it leaves on.
pub struct Downlink {
    pub egress: Arc<dyn StateEgress>,
    /// The flight controller's system id, once learned.
    pub fc_sysid: Box<dyn Fn() -> Option<u8> + Send + Sync>,
}

/// A due state on its way down: the payload (the FC's system id, then the
/// trimmed `state` body) and the lane it leaves on.
pub struct StateSend {
    pub(super) egress: Arc<dyn StateEgress>,
    pub payload: Vec<u8>,
}

impl StateSend {
    /// Send it, counting the outcome in `stats`.
    pub async fn send(self, stats: &DownlinkStats) {
        match self.egress.send_state(&self.payload).await {
            Ok(()) => stats.sent(),
            Err(e) => stats.send_failed(&e),
        }
    }
}

/// What the downlink did with each due state.
#[derive(Default)]
pub struct DownlinkStats {
    /// State frames the aux egress took.
    pub sent: AtomicU64,
    /// States dropped because even the trimmed body exceeds one aux frame.
    pub oversize_dropped: AtomicU64,
    /// Sends the aux egress refused or failed.
    pub send_failed: AtomicU64,
    /// Encoded bytes of the last due state's body, sent or dropped.
    pub last_bytes: AtomicU64,
    /// The last due state was dropped as oversized: the next drop does not warn.
    oversize_warned: AtomicBool,
    /// The last send failed: the next failure does not warn.
    send_warned: AtomicBool,
}

impl DownlinkStats {
    pub(super) fn body(&self, bytes: usize) {
        self.last_bytes.store(bytes as u64, Ordering::Relaxed);
        self.oversize_warned.store(false, Ordering::Relaxed);
    }

    pub(super) fn oversize(&self, bytes: usize) {
        self.oversize_dropped.fetch_add(1, Ordering::Relaxed);
        self.last_bytes.store(bytes as u64, Ordering::Relaxed);
        if !self.oversize_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                bytes,
                max = DOWNLINK_MAX_BODY,
                "pilot_state_downlink_oversize_dropped"
            );
        }
    }

    pub(super) fn sent(&self) {
        self.sent.fetch_add(1, Ordering::Relaxed);
        self.send_warned.store(false, Ordering::Relaxed);
    }

    pub(super) fn send_failed(&self, error: &dyn std::fmt::Display) {
        self.send_failed.fetch_add(1, Ordering::Relaxed);
        if !self.send_warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(error = %error, "pilot_state_downlink_failed");
        }
    }

    /// The router status entry.
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "sent": self.sent.load(Ordering::Relaxed),
            "oversize_dropped": self.oversize_dropped.load(Ordering::Relaxed),
            "send_failed": self.send_failed.load(Ordering::Relaxed),
            "last_bytes": self.last_bytes.load(Ordering::Relaxed),
            "max_bytes": DOWNLINK_MAX_BODY,
        })
    }
}

/// The `state` fields a ground surface reads off the radio downlink (the
/// phone's `PilotWire.parseState`, plus the `op` that names the frame), and
/// for a nested map the keys read inside it. A state that does not fit one
/// aux frame goes down as these alone.
const GROUND_FIELDS: &[(&str, Option<&[&str]>)] = &[
    ("op", None),
    ("owner", None),
    ("owner_label", None),
    ("skill", None),
    ("control_mode", None),
    ("profile", None),
    ("avoid", None),
    ("fc_mode", None),
    ("fc_mode_name", None),
    ("armed", None),
    ("landed", None),
    ("input_src", None),
    ("input_age_ms", None),
    ("hold_reason", None),
    ("avoid_state", None),
    ("min_clearance_m", None),
    ("goal_ned", None),
    ("plan_ned", None),
    ("setpoint_ned", None),
    ("readiness", None),
    ("link_loss_in_s", None),
    ("mission", None),
    ("vehicle_class", None),
    ("input_session", None),
    ("gate", None),
    ("sequence", None),
    ("banner", None),
    ("rth_countdown_s", None),
    ("surface_ops", None),
    ("agl_m", None),
    ("agl_source", None),
    ("statustext", None),
    ("link_loss_action", None),
    ("vtol_transitioning", None),
    ("setup_step", None),
    ("fc_failsafe", None),
    ("failsafe_cause", None),
    ("setup_abort", None),
    ("low_battery_action", None),
    ("speed_cap", None),
    ("position_unverified", None),
    ("mission_stall", None),
    ("place_class", None),
    ("route_check", None),
    ("rc_receiver", None),
    ("link_loss_s", None),
    ("profile_limits", Some(&["profile", "v_h_mps", "margin_m"])),
];

/// Only the [`GROUND_FIELDS`] of a state map.
fn ground_fields(m: Vec<(rmpv::Value, rmpv::Value)>) -> Vec<(rmpv::Value, rmpv::Value)> {
    m.into_iter()
        .filter_map(|(k, v)| {
            let (_, inner) = GROUND_FIELDS
                .iter()
                .find(|(name, _)| k.as_str() == Some(name))?;
            let v = match (inner, v) {
                (Some(keys), rmpv::Value::Map(e)) => rmpv::Value::Map(
                    e.into_iter()
                        .filter(|(ik, _)| ik.as_str().is_some_and(|s| keys.contains(&s)))
                        .collect(),
                ),
                (_, v) => v,
            };
            Some((k, v))
        })
        .collect()
}

/// The `state` body trimmed to fit one aux frame: the per-tick onboard
/// diagnostics (`command`, `shadow`, `stick`; the frame's own
/// `input_age_ms` still goes down) never go down; then only the
/// [`GROUND_FIELDS`] go, then the plan polyline goes first, then the
/// readiness items that pass, then every readiness item.
///
/// `Err` carries the encoded size of the most trimmed body that still did not
/// fit (0 for a state that is not a map).
pub fn downlink_body(state: &rmpv::Value) -> Result<Vec<u8>, usize> {
    let max = DOWNLINK_MAX_BODY;
    let rmpv::Value::Map(map) = state else {
        return Err(0);
    };
    let encode = |m: &Vec<(rmpv::Value, rmpv::Value)>| {
        let mut out = Vec::new();
        rmpv::encode::write_value(&mut out, &rmpv::Value::Map(m.clone()))
            .ok()
            .map(|_| out)
            .ok_or(0usize)
    };
    let mut m: Vec<(rmpv::Value, rmpv::Value)> = map
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), Some("command" | "shadow" | "stick")))
        .cloned()
        .collect();
    let mut out = encode(&m)?;
    let set = |m: &mut Vec<(rmpv::Value, rmpv::Value)>,
               key: &str,
               f: &dyn Fn(&rmpv::Value) -> rmpv::Value| {
        for (k, v) in m.iter_mut() {
            if k.as_str() == Some(key) {
                *v = f(v);
            }
        }
    };
    if out.len() > max {
        m = ground_fields(m);
        out = encode(&m)?;
    }
    if out.len() > max {
        set(&mut m, "plan_ned", &|_| rmpv::Value::Array(Vec::new()));
        out = encode(&m)?;
    }
    if out.len() > max {
        set(&mut m, "readiness", &|v| {
            let items = v.as_array().cloned().unwrap_or_default();
            rmpv::Value::Array(
                items
                    .into_iter()
                    .filter(|i| {
                        i.as_map()
                            .and_then(|e| e.iter().find(|(k, _)| k.as_str() == Some("ok")))
                            .and_then(|(_, v)| v.as_bool())
                            == Some(false)
                    })
                    .collect(),
            )
        });
        out = encode(&m)?;
    }
    if out.len() > max {
        set(&mut m, "readiness", &|_| rmpv::Value::Array(Vec::new()));
        out = encode(&m)?;
    }
    if out.len() > max {
        Err(out.len())
    } else {
        Ok(out)
    }
}
