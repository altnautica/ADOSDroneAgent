//! The beacon gate: which client `FOLLOW_TARGET` fixes become beacon events.
//!
//! Pure over its inputs (the decoded message, the client's origin, the two
//! clocks), so every rule is testable without a socket.

use std::collections::HashMap;

use ados_protocol::follow_beacon::{
    FollowBeacon, EST_POS, EST_VEL, FOLLOW_BEACON_V1_VERSION, MAX_FIX_AGE_MS, MAX_HORIZ_ACC_M,
    MIN_INTERVAL_MS, SOURCE_GCS_PHONE,
};
use ados_protocol::mavlink::ardupilotmega::FOLLOW_TARGET_DATA;
use serde::Serialize;

use crate::connection::ClientOrigin;

/// A sender silent this long starts a new session: its clock baseline is
/// learnt again.
const SESSION_GAP_MS: u64 = 30_000;

/// A fix timestamp this far behind the sender's last one is a restarted
/// sender, not a late fix.
const SENDER_RESTART_MS: u64 = 10_000;

/// Most senders tracked at once; the longest-silent one is forgotten first.
const MAX_SENDERS: usize = 16;

/// Why a fix was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// Not from an authorised ground link: an unauthenticated network peer,
    /// or an autonomous writer (a plugin, the pilot).
    Origin,
    /// No valid position.
    Position,
    /// Horizontal accuracy unknown or worse than [`MAX_HORIZ_ACC_M`].
    Accuracy,
    /// Older than [`MAX_FIX_AGE_MS`] against the sender's freshest fix, or not
    /// newer than its last one.
    Stale,
    /// Within [`MIN_INTERVAL_MS`] of the last published fix.
    Rate,
}

/// Fixes published and dropped, by reason. Never reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counters {
    pub published: u64,
    pub dropped_origin: u64,
    pub dropped_position: u64,
    pub dropped_accuracy: u64,
    pub dropped_stale: u64,
    pub dropped_rate: u64,
}

/// One sender's clock relation: the smallest receipt-minus-fix offset seen
/// (the least-delayed fix), its last fix time and when it was last heard.
#[derive(Debug, Clone, Copy)]
struct Sender {
    base_offset_ms: i64,
    last_fix_ms: u64,
    last_heard_ms: u64,
}

/// The gate's state: per-sender clock baselines, the rate limit and the counts.
#[derive(Debug, Default)]
pub struct BeaconGate {
    senders: HashMap<(u8, u8), Sender>,
    last_published_ms: Option<u64>,
    pub counters: Counters,
}

/// Who wrote the frame, as the client path classified it.
#[derive(Debug, Clone, Copy)]
pub struct Writer {
    pub origin: ClientOrigin,
    /// The writer is an autonomous injector (declared, or on the plugin plane).
    pub injector: bool,
    pub sysid: u8,
    pub compid: u8,
}

impl BeaconGate {
    /// Judge one fix. `wall_ms` is steady wall time (the sender's clock runs
    /// on wall time, so fix age is measured on it); `ados_ns` is ADOS time,
    /// stamped on the published beacon.
    pub fn admit(
        &mut self,
        writer: Writer,
        msg: &FOLLOW_TARGET_DATA,
        wall_ms: u64,
        ados_ns: u64,
    ) -> Result<FollowBeacon, Dropped> {
        let verdict = self.judge(writer, msg, wall_ms, ados_ns);
        let c = &mut self.counters;
        match verdict {
            Ok(_) => c.published += 1,
            Err(Dropped::Origin) => c.dropped_origin += 1,
            Err(Dropped::Position) => c.dropped_position += 1,
            Err(Dropped::Accuracy) => c.dropped_accuracy += 1,
            Err(Dropped::Stale) => c.dropped_stale += 1,
            Err(Dropped::Rate) => c.dropped_rate += 1,
        }
        verdict
    }

    fn judge(
        &mut self,
        writer: Writer,
        msg: &FOLLOW_TARGET_DATA,
        wall_ms: u64,
        ados_ns: u64,
    ) -> Result<FollowBeacon, Dropped> {
        if writer.injector || writer.origin == ClientOrigin::Unauthenticated {
            return Err(Dropped::Origin);
        }
        let (lat, lon) = position(msg).ok_or(Dropped::Position)?;
        let horiz_acc_m = known_positive(msg.position_cov[0]).ok_or(Dropped::Accuracy)?;
        if horiz_acc_m > MAX_HORIZ_ACC_M {
            return Err(Dropped::Accuracy);
        }
        self.check_fresh((writer.sysid, writer.compid), msg.timestamp, wall_ms)?;
        if self
            .last_published_ms
            .is_some_and(|last| wall_ms.saturating_sub(last) < MIN_INTERVAL_MS)
        {
            return Err(Dropped::Rate);
        }
        self.last_published_ms = Some(wall_ms);
        let vel_ned = (msg.est_capabilities & EST_VEL != 0
            && msg.vel.iter().all(|v| v.is_finite()))
        .then_some(msg.vel);
        Ok(FollowBeacon {
            v: FOLLOW_BEACON_V1_VERSION,
            lat,
            lon,
            alt_m: msg.alt.is_finite().then_some(f64::from(msg.alt)),
            vel_ned,
            horiz_acc_m,
            vert_acc_m: known_positive(msg.position_cov[1]),
            stamp_ns: ados_ns,
            source: SOURCE_GCS_PHONE.to_string(),
            sysid: writer.sysid,
            compid: writer.compid,
        })
    }

    /// Track the sender's clock against wall time and refuse a fix that is
    /// late against its least-delayed one, or not newer than its last. The
    /// first fix of a session sets the baseline; the sender refuses to send a
    /// fix older than [`MAX_FIX_AGE_MS`] on its own side, so that one is fresh.
    fn check_fresh(&mut self, key: (u8, u8), fix_ms: u64, wall_ms: u64) -> Result<(), Dropped> {
        let offset = wall_ms as i64 - fix_ms as i64;
        let fresh = Sender {
            base_offset_ms: offset,
            last_fix_ms: fix_ms,
            last_heard_ms: wall_ms,
        };
        let Some(s) = self.senders.get_mut(&key) else {
            if self.senders.len() >= MAX_SENDERS {
                if let Some(oldest) = self
                    .senders
                    .iter()
                    .min_by_key(|(_, s)| s.last_heard_ms)
                    .map(|(k, _)| *k)
                {
                    self.senders.remove(&oldest);
                }
            }
            self.senders.insert(key, fresh);
            return Ok(());
        };
        if wall_ms.saturating_sub(s.last_heard_ms) > SESSION_GAP_MS
            || fix_ms + SENDER_RESTART_MS < s.last_fix_ms
        {
            *s = fresh;
            return Ok(());
        }
        s.last_heard_ms = wall_ms;
        if fix_ms <= s.last_fix_ms {
            return Err(Dropped::Stale);
        }
        s.last_fix_ms = fix_ms;
        s.base_offset_ms = s.base_offset_ms.min(offset);
        if offset.saturating_sub(s.base_offset_ms) > MAX_FIX_AGE_MS as i64 {
            return Err(Dropped::Stale);
        }
        Ok(())
    }
}

/// Latitude and longitude in degrees, when the message carries a valid position.
fn position(msg: &FOLLOW_TARGET_DATA) -> Option<(f64, f64)> {
    if msg.est_capabilities & EST_POS == 0 || (msg.lat == 0 && msg.lon == 0) {
        return None;
    }
    let (lat, lon) = (f64::from(msg.lat) * 1e-7, f64::from(msg.lon) * 1e-7);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// `v` when it is a finite positive accuracy; ≤ 0 or NaN means unknown.
fn known_positive(v: f32) -> Option<f32> {
    (v.is_finite() && v > 0.0).then_some(v)
}
