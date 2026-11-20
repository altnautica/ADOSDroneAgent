//! The invariants, checked every tick, and the bookkeeping they read.
//!
//! * Liveness: a silent FC is declared lost within the heartbeat timeout of
//!   its last decoded HEARTBEAT, and never while one is fresh.
//! * Streams: while the FC streams nothing but its HEARTBEAT on a live link
//!   (a reboot forgot the intervals, the link was lost), the router asks
//!   again at least every [`STREAM_REREQUEST_MS`]; while ATTITUDE flows, the
//!   router's stream requests are at least [`STREAM_MIN_MS`] apart.
//! * Mission mirror: its own download never waits longer than one reply
//!   timeout without a request and ends within its retry budget; a failed
//!   download is reported (`route_stale`) until one succeeds; a mirror it
//!   reports current is a mission the FC actually held.
//! * Command gates: a command-down-gated link puts no byte on the FC link; a
//!   visor never forwards a ground station's flight command.

use ados_mavlink_router::mission_mirror::wire::ItemInt;
use ados_mavlink_router::mission_mirror::{ITEM_TIMEOUT, RETRIES};
use ados_protocol::mavlink::ardupilotmega::MISSION_ITEM_INT_DATA;

use crate::router::MIRROR_MS;
use crate::sim::{World, STEP};

/// `FcConnection`'s heartbeat timeout.
pub const HEARTBEAT_TIMEOUT_MS: u64 = 4_000;
/// A stall shows within the stream stall window (5 s) on the 1 s request
/// tick, and a stalled link is re-asked every stream minimum (10 s).
pub const STREAM_REREQUEST_MS: u64 = 12_000;
/// The shortest stream refresh interval.
pub const STREAM_MIN_MS: u64 = 10_000;
/// The longest mission the scenarios hold, with its home slot.
const MAX_ITEMS: u64 = 64;
/// How long after the FC changed missions the mirror may still hold the old
/// one as current: the ACK's retry by the ground station.
const MIRROR_LAG_MS: u64 = 10_000;

/// A mission item as compared across the FC, the uploads and the mirror.
pub type Key = (u16, u16, i32, i32, u32);

pub fn key(i: &MISSION_ITEM_INT_DATA) -> Key {
    (i.seq, i.command as u16, i.x, i.y, i.z.to_bits())
}

fn mirror_key(i: &ItemInt) -> Key {
    (i.seq, i.command, i.x, i.y, i.z.to_bits())
}

/// The FC link as the harness saw it.
#[derive(Default)]
pub struct Live {
    pub opened_at: u64,
    /// The router's last decoded FC HEARTBEAT, ATTITUDE and streamed frame
    /// (any but a HEARTBEAT).
    pub hb: Option<u64>,
    pub att: Option<u64>,
    pub data: Option<u64>,
    pub alive: bool,
    pub alive_since: u64,
    /// The router's last stream request, and the last burst of them.
    pub req: Option<u64>,
    pub burst_at: Option<u64>,
    /// Every burst of stream requests.
    pub bursts: Vec<u64>,
    /// ATTITUDE flowed without a gap since the last burst, on one link.
    flowing: bool,
}

impl Live {
    pub fn attitude(&mut self, now: u64) {
        if self.att.is_none_or(|t| now - t > 1_000) {
            self.flowing = false;
        }
        self.att = Some(now);
    }

    pub fn closed(&mut self) {
        self.hb = None;
        self.flowing = false;
    }

    pub fn opened(&mut self, now: u64) {
        self.opened_at = now;
        self.flowing = false;
    }

    /// Whether ATTITUDE is flowing now.
    pub fn att_fresh(&self, now: u64) -> bool {
        self.att.is_some_and(|t| now - t <= 1_000)
    }
}

/// The mirror's own download, as the harness saw it.
#[derive(Default)]
pub struct Dl {
    /// `(started, last request)` while one is in flight.
    pub in_flight: Option<(u64, u64)>,
    ok: u32,
    failed: u32,
    /// A download failed since the last success.
    pub unresolved: bool,
}

/// A burst of stream requests: while ATTITUDE flowed since the previous
/// burst, it came too soon.
pub fn stream_burst(w: &mut World) {
    let now = w.now;
    if let Some(prev) = w.live.burst_at {
        if w.check_stream_spacing
            && w.live.flowing
            && w.live.att_fresh(now)
            && now - prev < STREAM_MIN_MS
        {
            w.fail(format_args!(
                "stream requests {} ms apart while ATTITUDE flowed: the router keeps asking once data flows",
                now - prev
            ));
        }
    }
    w.live.burst_at = Some(now);
    w.live.bursts.push(now);
    w.live.flowing = w.live.att_fresh(now);
    w.note(format_args!("stream request burst"));
}

impl World {
    pub async fn check(&mut self) {
        let now = self.now;
        let open = self.router.is_open();
        let alive = self.router.fc.mavlink_alive().await;
        if alive && !self.live.alive {
            self.live.alive_since = now;
        }
        if alive != self.live.alive {
            self.note(format_args!("alive {alive}"));
        }
        self.live.alive = alive;
        if !self.live.att_fresh(now) {
            self.live.flowing = false;
        }

        // A silent FC is declared lost within its bound, and only then.
        if let Some(hb) = self.live.hb {
            if alive && now - hb > HEARTBEAT_TIMEOUT_MS + STEP {
                self.fail(format_args!(
                    "no FC HEARTBEAT for {} ms, yet the link reads alive",
                    now - hb
                ));
            }
            if !alive && open && hb >= self.live.opened_at && now - hb + STEP < HEARTBEAT_TIMEOUT_MS
            {
                self.fail(format_args!(
                    "FC HEARTBEAT {} ms ago, yet the link reads lost",
                    now - hb
                ));
            }
        } else if alive {
            self.fail(format_args!(
                "no FC HEARTBEAT on this link, yet it reads alive"
            ));
        }

        // Streams lost on a live link are asked for again.
        if open && alive {
            let absent_from = self.live.data.unwrap_or(0).max(self.live.alive_since);
            let asked = self
                .live
                .req
                .is_some_and(|r| now - r <= STREAM_REREQUEST_MS);
            if now - absent_from > STREAM_REREQUEST_MS && !asked {
                self.fail(format_args!(
                    "the FC streamed only HEARTBEATs for {} ms on a live link and no stream request for {} ms",
                    now - absent_from,
                    self.live.req.map_or(now, |r| now - r)
                ));
            }
        }

        self.check_gates();
        self.check_uploads();
        if now.is_multiple_of(MIRROR_MS) {
            self.check_mirror().await;
        }
        if now.is_multiple_of(100) {
            let m = &self.router.mirror;
            let line = format!(
                "open={open} alive={alive} att={} fc_on={} rev={} items={} ok={} failed={} up={}/{} down={}/{}",
                self.live.att_fresh(now),
                self.fc.powered,
                m.mirror.rev,
                m.mirror.items.len(),
                m.downloads_ok,
                m.downloads_failed,
                self.up.lost,
                self.up.offered,
                self.down.lost,
                self.down.offered,
            );
            self.note(format_args!("{line}"));
        }
    }

    fn check_gates(&mut self) {
        if self.router.fc.command_down_gated() && self.fc.rx_bytes > 0 {
            self.fail(format_args!(
                "{} bytes reached the FC through a command-down-gated link",
                self.fc.rx_bytes
            ));
        }
        let heard = std::mem::take(&mut self.fc.heard);
        let visor = self.cfg.profile == "visor";
        for h in heard {
            if (h.sys, h.comp) != (crate::gcs::SYS, crate::gcs::COMP) {
                continue;
            }
            let flight = crate::gcs::is_flight(&h);
            if visor && flight {
                self.fail(format_args!(
                    "a visor forwarded a flight command to the FC: {h:?}"
                ));
            }
            if let Some(g) = self.gcs.as_mut() {
                g.heard[usize::from(flight)] += 1;
            }
            self.note(format_args!(
                "fc heard gcs msg {} {:?}",
                h.msg_id,
                h.command.map(|c| c.0)
            ));
        }
    }

    /// Every ground-station upload resolves within its deadline, and an
    /// accepted one is a mission the FC held since the upload began: the
    /// router forwarded the client's frames intact.
    fn check_uploads(&mut self) {
        let now = self.now;
        let Some(g) = self.gcs.as_mut() else {
            return;
        };
        let mut verdict = None;
        for (i, up) in g.gcs.uploads.iter().enumerate() {
            match up.outcome {
                None if now > up.deadline() => {
                    verdict = Some(format!(
                        "upload {i} of {} items has no outcome past its deadline",
                        up.items.len()
                    ));
                }
                Some((at, o)) if at == now => {
                    let sent: Vec<Key> = up.items.iter().map(key).collect();
                    let held = self
                        .fc_hist
                        .iter()
                        .any(|(_, until, k)| *k == sent && *until >= up.started);
                    if o == crate::gcs::Outcome::Accepted && !held {
                        verdict =
                            Some(format!("upload {i} was accepted, yet the FC never held it"));
                    }
                    self.trace.line(format_args!(
                        "{now:>7} upload {i} {} items: {o:?}",
                        up.items.len()
                    ));
                }
                _ => {}
            }
        }
        if let Some(v) = verdict {
            self.fail(format_args!("{v}"));
        }
    }

    async fn check_mirror(&mut self) {
        let now = self.now;
        let (ok, failed) = (
            self.router.mirror.downloads_ok,
            self.router.mirror.downloads_failed,
        );
        if ok != self.dl.ok || failed != self.dl.failed {
            self.note(format_args!("download ok={ok} failed={failed}"));
            self.dl.in_flight = None;
            if failed != self.dl.failed {
                self.dl.unresolved = true;
            }
            if ok != self.dl.ok {
                self.dl.unresolved = false;
            }
            self.dl.ok = ok;
            self.dl.failed = failed;
        }
        // The mirror's own download never hangs.
        if let Some((start, last)) = self.dl.in_flight {
            let reply = ITEM_TIMEOUT.as_millis() as u64 + MIRROR_MS + STEP;
            if now - last > reply {
                self.fail(format_args!(
                    "the mirror's download sent nothing for {} ms",
                    now - last
                ));
            }
            let budget = (MAX_ITEMS + 2) * (u64::from(RETRIES) + 1) * reply;
            if now - start > budget {
                self.fail(format_args!(
                    "the mirror's download ran {} ms, past its {budget} ms budget",
                    now - start
                ));
            }
        }
        let mission = self.router.mission_json().await;
        let stale = mission.get("route_stale").and_then(|v| v.as_bool()) == Some(true);
        // A failed download is reported until one succeeds.
        if self.dl.unresolved && !stale {
            self.fail(format_args!(
                "a mission download failed, yet the mirror reports itself current"
            ));
        }
        // A mirror reported current is a mission the FC held.
        let m = &self.router.mirror.mirror;
        if m.present && !stale {
            let keys: Vec<Key> = m.items.iter().map(mirror_key).collect();
            let held = self
                .fc_hist
                .iter()
                .any(|(_, until, k)| *k == keys && until.saturating_add(MIRROR_LAG_MS) >= now);
            if !held {
                self.fail(format_args!(
                    "the mirror reports {} items as current, a mission the FC did not hold",
                    keys.len()
                ));
            }
        }
    }

    /// The mirror holds the FC's mission now, reported current.
    pub async fn mirror_matches(&self) -> Result<(), String> {
        let mission = self.router.mission_json().await;
        if mission.get("route_stale").and_then(|v| v.as_bool()) == Some(true) {
            return Err("the mirror still reports route_stale".into());
        }
        let got: Vec<Key> = self
            .router
            .mirror
            .mirror
            .items
            .iter()
            .map(mirror_key)
            .collect();
        let want: Vec<Key> = self.fc.mission.iter().map(key).collect();
        if got != want {
            return Err(format!(
                "the mirror holds {} items, the FC {}",
                got.len(),
                want.len()
            ));
        }
        Ok(())
    }
}
