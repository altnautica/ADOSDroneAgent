//! The uplink operations the command socket serves and the presence watcher
//! falls back through: take, give-back, take-back, and the holder-absent
//! return. Each one holds the uplink's op lock for its whole run, so a stop and
//! a start never interleave.

use std::time::Instant;

use serde_json::{json, Value};

use super::{Holder, Uplink, UplinkState, TX_SWITCH_DEADLINE};

/// The longest device id or name a take may carry.
const FIELD_MAX: usize = 64;

/// A phone's request to take the uplink, after the LAN route authenticated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TakeRequest {
    pub holder_device_id: String,
    pub holder_name: String,
    /// Fingerprint of the approved phone key the route verified.
    pub phone_key: String,
    pub yield_url: Option<String>,
}

/// What the box knows about itself when a take arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeContext {
    /// A bind session owns the radios.
    pub bind_active: bool,
    /// This box runs the standalone (direct) role, the only one that
    /// transmits for a fleet.
    pub direct_role: bool,
}

pub fn refusal(code: &str, detail: &str) -> Value {
    json!({"ok": false, "error": code, "detail": detail})
}

fn done(u: &Uplink) -> Value {
    json!({"ok": true, "state": u.state().as_str(), "uplink": u.sidecar_json()})
}

/// A device id the beacon and the registry can both carry.
pub fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= FIELD_MAX
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Field checks, before anything moves.
pub fn validate_take(req: &TakeRequest) -> Result<(), Value> {
    if !valid_device_id(&req.holder_device_id) {
        return Err(refusal(
            "E_BAD_HOLDER",
            "The holder device id must be 1 to 64 letters, digits, dashes or underscores.",
        ));
    }
    if req.holder_name.trim().is_empty() || req.holder_name.chars().count() > FIELD_MAX {
        return Err(refusal(
            "E_BAD_HOLDER",
            "The holder name must be 1 to 64 characters.",
        ));
    }
    Ok(())
}

/// Transition 1: stop both transmitters, then stand by. Replies only after
/// both transmitter processes exited.
pub async fn take(u: &Uplink, req: TakeRequest, ctx: TakeContext) -> Value {
    if let Err(r) = validate_take(&req) {
        return r;
    }
    if ctx.bind_active {
        return refusal(
            "E_BIND_ACTIVE",
            "A bind session is running on this ground station, and its radios belong to the bind.",
        );
    }
    if !ctx.direct_role {
        return refusal(
            "E_NOT_DIRECT",
            "This ground station runs as a mesh relay or receiver, not as its fleet's transmitting ground.",
        );
    }
    let _op = u.op().await;
    if let UplinkState::Standby(h) = u.state() {
        if h.device_id == req.holder_device_id && h.phone_key == req.phone_key {
            u.refresh_holder(holder_of(req));
            return done(u);
        }
        return refusal(
            "E_UPLINK_HELD",
            &format!("The uplink is already held by {}.", h.name),
        );
    }
    if !u.stop_transmitters(TX_SWITCH_DEADLINE).await {
        tracing::error!("uplink_take_failed: the ground transmitters did not stop");
        return refusal(
            "E_TX_STOP_FAILED",
            "The ground transmitters did not stop in time, so this box keeps the uplink.",
        );
    }
    tracing::info!(
        holder_device_id = %req.holder_device_id,
        holder_name = %req.holder_name,
        "uplink_standby: both ground transmitters stopped for the phone"
    );
    u.enter_standby(holder_of(req));
    done(u)
}

fn holder_of(req: TakeRequest) -> Holder {
    Holder {
        device_id: req.holder_device_id,
        name: req.holder_name.trim().to_string(),
        phone_key: req.phone_key,
        yield_url: req.yield_url,
    }
}

/// Transition 2: the holder stopped its transmitters and hands the uplink
/// back. Only the holder may.
pub async fn give_back(u: &Uplink, holder_device_id: &str, phone_key: &str) -> Value {
    let _op = u.op().await;
    let holder = match u.state() {
        UplinkState::Transmitting => {
            return refusal(
                "E_NOT_HELD",
                "No phone holds the uplink; this box is transmitting.",
            )
        }
        UplinkState::Standby(h) => h,
    };
    if holder.device_id != holder_device_id || holder.phone_key != phone_key {
        return refusal(
            "E_NOT_HOLDER",
            &format!(
                "Only the phone that holds the uplink ({}) can give it back.",
                holder.name
            ),
        );
    }
    resume(u, &holder, "uplink_given_back").await;
    done(u)
}

/// Transition 3: the box's own operator takes the uplink back. The LAN route
/// told the phone first and waited for it; `phone_confirmed` says whether it
/// answered. The box transmits either way.
pub async fn take_back(u: &Uplink, phone_confirmed: bool) -> Value {
    let _op = u.op().await;
    if let UplinkState::Standby(holder) = u.state() {
        tracing::info!(phone_confirmed, "uplink_take_back");
        resume(u, &holder, "uplink_taken_back").await;
    }
    done(u)
}

/// Transition 4: no beacon from the holder for the stale window. True when the
/// box went back to transmitting.
pub async fn fall_back_if_absent(u: &Uplink, now: Instant) -> bool {
    if !u.holder_absent(now) {
        return false;
    }
    let _op = u.op().await;
    // Re-checked under the lock: a give-back may have landed meanwhile.
    let Some(holder) = u.holder().filter(|_| u.holder_absent(now)) else {
        return false;
    };
    tracing::warn!(
        holder_device_id = %holder.device_id,
        holder_name = %holder.name,
        "uplink_holder_absent: no beacon from the holder, transmitting again"
    );
    resume(u, &holder, "uplink_holder_absent").await;
    true
}

/// Start the transmitters, then record the return. The state reads standby
/// until they run, so nothing on the box treats the uplink as its own early.
async fn resume(u: &Uplink, holder: &Holder, event: &'static str) {
    if !u.start_transmitters(TX_SWITCH_DEADLINE).await {
        tracing::warn!(
            event,
            "uplink_transmitters_not_running: no receive generation is up; the next one starts them"
        );
    }
    u.enter_transmitting();
    tracing::info!(
        event,
        holder_device_id = %holder.device_id,
        holder_name = %holder.name,
        "uplink_transmitting"
    );
}

/// The command socket's read: the sidecar field plus the take-back callback.
pub fn status(u: &Uplink) -> Value {
    let holder = u.holder();
    json!({
        "ok": true,
        "state": u.state().as_str(),
        "uplink": u.sidecar_json(),
        "holder_phone_key": holder.as_ref().map(|h| h.phone_key.clone()),
        "yield_url": holder.and_then(|h| h.yield_url),
    })
}
