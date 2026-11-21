//! The beacon gate: which client `FOLLOW_TARGET` fixes become beacon events.
//!
//! Pure over its inputs (the decoded message, the writer as the router
//! classified it, the two clocks), so every rule is testable without a socket.

use ados_protocol::follow_beacon::{
    FollowBeacon, EST_POS, EST_VEL, FOLLOW_BEACON_V1_VERSION, LOCK_SILENCE_MS, MAX_FIX_AGE_MS,
    MAX_HORIZ_ACC_M, MIN_INTERVAL_MS,
};
use ados_protocol::mavlink::ardupilotmega::FOLLOW_TARGET_DATA;
use serde::Serialize;

use super::link::{BeaconLink, Origin};

/// A fix timestamp this far behind the link's last one is a restarted sender,
/// not a late fix.
const SENDER_RESTART_MS: u64 = 10_000;

/// Why a fix was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dropped {
    /// Not from an authorised ground link: an unauthenticated network peer,
    /// a plugin, the plugin host's link, or an autonomous injector.
    Origin,
    /// No valid position.
    Position,
    /// Horizontal accuracy unknown or worse than [`MAX_HORIZ_ACC_M`].
    Accuracy,
    /// A fix time no clock can hold (beyond `i64::MAX` ms).
    Time,
    /// Another link holds the lock and has been heard within
    /// [`LOCK_SILENCE_MS`].
    OtherLink,
    /// Older than [`MAX_FIX_AGE_MS`] against the link's least-delayed fix, or
    /// not newer than its last.
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
    pub dropped_time: u64,
    pub dropped_other_link: u64,
    pub dropped_stale: u64,
    pub dropped_rate: u64,
}

/// The locked link: its clock relation to wall time (the smallest
/// receipt-minus-fix offset, from its least-delayed fix), its last fix time,
/// when it was last heard and when it was last published.
#[derive(Debug, Clone, Copy)]
struct Lock {
    link: BeaconLink,
    base_offset_ms: i128,
    last_fix_ms: u64,
    last_heard_ms: u64,
    last_published_ms: Option<u64>,
}

/// The gate's state: the locked link and the counts.
#[derive(Debug, Default)]
pub struct BeaconGate {
    lock: Option<Lock>,
    pub counters: Counters,
}

/// The sender ids a frame header claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claimed {
    pub sysid: u8,
    pub compid: u8,
}

impl BeaconGate {
    /// Judge one fix from `link`. `wall_ms` is steady wall time (the sender's
    /// clock runs on wall time, so fix age is measured on it); `ados_ns` is
    /// ADOS time, stamped on the published beacon.
    pub fn admit(
        &mut self,
        link: BeaconLink,
        claimed: Claimed,
        msg: &FOLLOW_TARGET_DATA,
        wall_ms: u64,
        ados_ns: u64,
    ) -> Result<FollowBeacon, Dropped> {
        let verdict = self.judge(link, claimed, msg, wall_ms, ados_ns);
        let c = &mut self.counters;
        match verdict {
            Ok(_) => c.published += 1,
            Err(Dropped::Origin) => c.dropped_origin += 1,
            Err(Dropped::Position) => c.dropped_position += 1,
            Err(Dropped::Accuracy) => c.dropped_accuracy += 1,
            Err(Dropped::Time) => c.dropped_time += 1,
            Err(Dropped::OtherLink) => c.dropped_other_link += 1,
            Err(Dropped::Stale) => c.dropped_stale += 1,
            Err(Dropped::Rate) => c.dropped_rate += 1,
        }
        verdict
    }

    fn judge(
        &mut self,
        link: BeaconLink,
        claimed: Claimed,
        msg: &FOLLOW_TARGET_DATA,
        wall_ms: u64,
        ados_ns: u64,
    ) -> Result<FollowBeacon, Dropped> {
        let origin = link.origin.label().ok_or(Dropped::Origin)?;
        let (lat, lon) = position(msg).ok_or(Dropped::Position)?;
        let horiz_acc_m = known_positive(msg.position_cov[0]).ok_or(Dropped::Accuracy)?;
        if horiz_acc_m > MAX_HORIZ_ACC_M {
            return Err(Dropped::Accuracy);
        }
        if i64::try_from(msg.timestamp).is_err() {
            return Err(Dropped::Time);
        }
        let lock = self.lock_for(link, msg.timestamp, wall_ms)?;
        if lock
            .last_published_ms
            .is_some_and(|last| wall_ms.saturating_sub(last) < MIN_INTERVAL_MS)
        {
            return Err(Dropped::Rate);
        }
        lock.last_published_ms = Some(wall_ms);
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
            origin: origin.to_string(),
            link: link.id.label(),
            sysid: claimed.sysid,
            compid: claimed.compid,
        })
    }

    /// The lock for `link` with this fix applied, or why the fix is refused.
    ///
    /// One link holds the lock until it has been silent for
    /// [`LOCK_SILENCE_MS`]; meanwhile every other link's fixes are refused, so
    /// a second sender can neither interleave with nor crowd out the first.
    /// The first fix of a lock sets the clock baseline (the sender refuses to
    /// send a fix older than [`MAX_FIX_AGE_MS`] on its own side, so that one is
    /// fresh); after that a fix later than the baseline by more than the age
    /// limit, or not newer than the last, is stale. A fix far behind the last
    /// one is a restarted sender and starts a new baseline.
    fn lock_for(
        &mut self,
        link: BeaconLink,
        fix_ms: u64,
        wall_ms: u64,
    ) -> Result<&mut Lock, Dropped> {
        let offset = i128::from(wall_ms) - i128::from(fix_ms);
        let fresh = Lock {
            link,
            base_offset_ms: offset,
            last_fix_ms: fix_ms,
            last_heard_ms: wall_ms,
            last_published_ms: None,
        };
        let held = self
            .lock
            .filter(|l| wall_ms.saturating_sub(l.last_heard_ms) <= LOCK_SILENCE_MS);
        let Some(mut l) = held else {
            return Ok(self.lock.insert(fresh));
        };
        if l.link.id != link.id {
            return Err(Dropped::OtherLink);
        }
        l.last_heard_ms = wall_ms;
        if fix_ms.saturating_add(SENDER_RESTART_MS) < l.last_fix_ms {
            let published = l.last_published_ms;
            l = Lock {
                last_published_ms: published,
                ..fresh
            };
            return Ok(self.lock.insert(l));
        }
        if fix_ms <= l.last_fix_ms {
            self.lock = Some(l);
            return Err(Dropped::Stale);
        }
        l.last_fix_ms = fix_ms;
        l.base_offset_ms = l.base_offset_ms.min(offset);
        let late = offset - l.base_offset_ms > i128::from(MAX_FIX_AGE_MS);
        let l = self.lock.insert(l);
        if late {
            return Err(Dropped::Stale);
        }
        Ok(l)
    }
}

impl Origin {
    /// The published origin label, or `None` for a writer whose fixes are
    /// never accepted.
    fn label(self) -> Option<&'static str> {
        use ados_protocol::follow_beacon::{ORIGIN_GROUND_LINK, ORIGIN_ON_BOX, ORIGIN_RELAY};
        match self {
            Origin::GroundLink => Some(ORIGIN_GROUND_LINK),
            Origin::OnBox => Some(ORIGIN_ON_BOX),
            Origin::Relay => Some(ORIGIN_RELAY),
            Origin::Plugin | Origin::Anonymous => None,
        }
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
