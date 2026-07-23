//! Channel sources and the authority merge feeding the RC packer.
//!
//! Two sources can feed the transmitted channel set:
//!
//! * **HID** — stick/switch intent read from the primary gamepad the
//!   `ados-input` daemon selects, gated by the PIC arbiter (the `hid` module
//!   owns the device read; this module owns only the merged values).
//! * **Injection** — explicit channel values set programmatically over the
//!   command socket, each write carrying a time-to-live. An injector that goes
//!   silent past its TTL decays to the safe neutral set — the lane never
//!   holds a stale stick.
//!
//! The configured `channel_source` mode decides authority. In `hybrid` the
//! PIC arbiter's holder wins: while a client holds the PIC claim the lane
//! obeys that client's lane (the injector when the holder IS the injector's
//! client id, the human HID path for any other holder); with a FRESH,
//! affirmative "no claim held" report the programmatic lane feeds. A PIC
//! arbiter that is NOT reporting (its sidecar absent, unreadable, malformed,
//! or stale) is treated as UNKNOWN, never as "no human wants control": hybrid
//! fails SAFE to the human/neutral hold, so a dead or hung arbiter can never
//! hand the autonomous injector authority on a missing verdict. The losing
//! source's values are stored but never transmitted — authority never silently
//! falls through to the other source, because a source that did not win must
//! not fly the aircraft.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use crate::bank::{BankError, ChannelBank};
use crate::channels::CHANNEL_COUNT;

/// Default injection time-to-live when a write does not carry one.
pub const DEFAULT_INJECT_TTL: Duration = Duration::from_millis(1000);
/// Floor on a requested injection TTL.
pub const MIN_INJECT_TTL: Duration = Duration::from_millis(100);
/// Ceiling on a requested injection TTL: an injector must refresh at least
/// this often, so a crashed injector's last stick is held no longer than this.
pub const MAX_INJECT_TTL: Duration = Duration::from_millis(5000);

/// How fresh the PIC arbiter's state sidecar must be to count as a live view.
/// The arbiter daemon rewrites it on every transition and each watchdog tick
/// (~5 s); beyond this window the arbiter is not reporting and the view reads
/// unclaimed (no human input stack ⇒ the programmatic lane holds authority).
pub const PIC_STALE_AFTER: Duration = Duration::from_secs(20);

/// Clamp a requested TTL into the allowed window.
pub fn clamp_ttl(requested: Duration) -> Duration {
    requested.clamp(MIN_INJECT_TTL, MAX_INJECT_TTL)
}

/// The configured channel-source mode (`radio.crsf.channel_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelSourceMode {
    /// Only the HID/PIC gamepad path feeds the lane.
    Hid,
    /// Only programmatic injection feeds the lane.
    Inject,
    /// Both sources feed; the PIC arbiter's holder decides authority.
    Hybrid,
}

impl ChannelSourceMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "hid" => Some(Self::Hid),
            "inject" => Some(Self::Inject),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hid => "hid",
            Self::Inject => "inject",
            Self::Hybrid => "hybrid",
        }
    }
}

/// Which source's values the transmitter obeys right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority {
    Hid,
    Inject,
}

impl Authority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hid => "hid",
            Self::Inject => "inject",
        }
    }
}

/// The source a transmitted value set actually came from, reported on the
/// sidecar (`channel_source`). `None` (⇒ a JSON null) means the neutral
/// fallback — no live source, never a fabricated label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelSource {
    Hid,
    Inject,
}

impl ChannelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hid => "hid",
            Self::Inject => "inject",
        }
    }
}

/// The PIC arbiter view the hybrid merge consults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PicView {
    /// A PIC claim is currently held.
    pub claimed: bool,
    /// The holding client id, when claimed.
    pub holder: Option<String>,
}

/// Read the PIC arbiter's state sidecar (`pic-state.json`), staleness-gated
/// against its file mtime. `None` when the file is absent, unreadable,
/// malformed, or older than [`PIC_STALE_AFTER`] relative to `now` — the
/// arbiter is not reporting, which the caller treats as an unclaimed view.
pub fn read_pic_view(path: &Path, now: SystemTime) -> Option<PicView> {
    let meta = std::fs::metadata(path).ok()?;
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = now.duration_since(modified) {
            if age > PIC_STALE_AFTER {
                return None;
            }
        }
    }
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let claimed = value.get("state").and_then(|v| v.as_str()) == Some("claimed");
    let holder = value
        .get("claimed_by")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(PicView { claimed, holder })
}

/// Decide which source has authority. Pure.
///
/// `pic` is the PIC arbiter's report: `Some` when the arbiter is reporting a
/// fresh claimed/unclaimed view, `None` when it is NOT reporting (its sidecar
/// is absent, unreadable, malformed, or stale). `injector_id` is the client id
/// attached to the currently-LIVE injected set (`None` when there is no live
/// injection or it carried no id).
pub fn resolve_authority(
    mode: ChannelSourceMode,
    pic: Option<&PicView>,
    injector_id: Option<&str>,
) -> Authority {
    match mode {
        ChannelSourceMode::Hid => Authority::Hid,
        ChannelSourceMode::Inject => Authority::Inject,
        ChannelSourceMode::Hybrid => match pic {
            // The arbiter is not reporting (absent / stale / malformed): its
            // verdict is UNKNOWN, so hybrid fails SAFE. This routes to the
            // human/neutral hold — a live human HID stack keeps flying, no HID
            // holds neutral — but the autonomous injector NEVER wins on a
            // missing verdict. A dead or hung arbiter is not consent.
            None => Authority::Hid,
            // The PIC arbiter's holder wins: when the holder IS the injector's
            // client, the programmatic lane flies; any other holder is the
            // human input path.
            Some(view) if view.claimed => match (view.holder.as_deref(), injector_id) {
                (Some(holder), Some(injector)) if holder == injector => Authority::Inject,
                _ => Authority::Hid,
            },
            // A FRESH, affirmative "no client holds the claim": the
            // programmatic lane feeds. HID input without a PIC claim must not
            // fly — the claim is the whole point of the arbiter.
            Some(_) => Authority::Inject,
        },
    }
}

/// A live injected channel set with its expiry and the injecting client.
#[derive(Debug, Clone)]
struct Injected {
    bank: ChannelBank,
    fresh_until: Instant,
    client_id: Option<String>,
}

/// The merged channel state the fixed-cadence transmitter reads each tick.
/// Shared behind a mutex between the command socket (injection writer), the
/// HID reader (hid writer), the heartbeat (PIC refresh + label read), and the
/// TX task (value reader).
#[derive(Debug)]
pub struct SourceMerge {
    mode: ChannelSourceMode,
    inject: Option<Injected>,
    hid: Option<ChannelBank>,
    /// The PIC arbiter's latest report: `Some` for a fresh claimed/unclaimed
    /// view, `None` when the arbiter is not reporting (absent / stale sidecar).
    /// Starts `None` — before the first sidecar read the arbiter's verdict is
    /// genuinely unknown, so a fresh hybrid merge holds SAFE (the injector does
    /// not fly until the arbiter affirmatively reports).
    pic: Option<PicView>,
}

impl SourceMerge {
    pub fn new(mode: ChannelSourceMode) -> Self {
        Self {
            mode,
            inject: None,
            hid: None,
            pic: None,
        }
    }

    pub fn mode(&self) -> ChannelSourceMode {
        self.mode
    }

    fn live_inject(&self, now: Instant) -> Option<&Injected> {
        self.inject.as_ref().filter(|i| now < i.fresh_until)
    }

    /// Inject all 16 channels with a TTL (clamped into the allowed window).
    /// The whole set is validated before anything applies.
    pub fn inject_all(
        &mut self,
        values: [u16; CHANNEL_COUNT],
        ttl: Duration,
        now: Instant,
        client_id: Option<String>,
    ) -> Result<(), BankError> {
        let mut bank = ChannelBank::default();
        bank.set_all(values)?;
        self.inject = Some(Injected {
            bank,
            fresh_until: now + clamp_ttl(ttl),
            client_id,
        });
        Ok(())
    }

    /// Inject one channel with a TTL. The base is the still-fresh injected set
    /// when one exists, else the neutral set — never an expired (stale) one.
    pub fn inject_one(
        &mut self,
        index: usize,
        value: u16,
        ttl: Duration,
        now: Instant,
        client_id: Option<String>,
    ) -> Result<(), BankError> {
        let mut bank = match self.live_inject(now) {
            Some(live) => live.bank.clone(),
            None => ChannelBank::default(),
        };
        bank.set_one(index, value)?;
        self.inject = Some(Injected {
            bank,
            fresh_until: now + clamp_ttl(ttl),
            client_id,
        });
        Ok(())
    }

    /// Update the HID source's latest channel set. HID values stay valid while
    /// the device reader is alive (evdev is edge-triggered — a held stick
    /// produces no events, so time-based expiry would be wrong here); the
    /// reader clears the slot when the device goes away.
    pub fn set_hid(&mut self, values: [u16; CHANNEL_COUNT]) -> Result<(), BankError> {
        let mut bank = self.hid.take().unwrap_or_default();
        bank.set_all(values)?;
        self.hid = Some(bank);
        Ok(())
    }

    /// Drop the HID source (device lost / reader exiting): its last stick must
    /// not outlive the device that produced it.
    pub fn clear_hid(&mut self) {
        self.hid = None;
    }

    /// Replace the PIC arbiter view (refreshed from its sidecar each tick).
    /// `None` = the arbiter is not reporting (its sidecar was absent, stale, or
    /// malformed on this read) — the hybrid merge then holds SAFE.
    pub fn set_pic(&mut self, view: Option<PicView>) {
        self.pic = view;
    }

    /// The last arbiter report, or `None` when the arbiter is not reporting.
    pub fn pic(&self) -> Option<&PicView> {
        self.pic.as_ref()
    }

    /// The source holding authority right now.
    pub fn authority(&self, now: Instant) -> Authority {
        let injector = self.live_inject(now).and_then(|i| i.client_id.as_deref());
        resolve_authority(self.mode, self.pic.as_ref(), injector)
    }

    /// The channel set to transmit right now, plus the live source it came
    /// from (`None` = the safe neutral fallback: the winning source has no
    /// live values). The losing source's values are never transmitted.
    pub fn current(&self, now: Instant) -> ([u16; CHANNEL_COUNT], Option<ChannelSource>) {
        match self.authority(now) {
            Authority::Inject => match self.live_inject(now) {
                Some(live) => (live.bank.values(), Some(ChannelSource::Inject)),
                None => (ChannelBank::neutral(), None),
            },
            Authority::Hid => match &self.hid {
                Some(bank) => (bank.values(), Some(ChannelSource::Hid)),
                None => (ChannelBank::neutral(), None),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{CHANNEL_MAX, CHANNEL_MID, CHANNEL_MIN};

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn mode_parse_round_trips_and_rejects_unknown() {
        for mode in [
            ChannelSourceMode::Hid,
            ChannelSourceMode::Inject,
            ChannelSourceMode::Hybrid,
        ] {
            assert_eq!(ChannelSourceMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ChannelSourceMode::parse("both"), None);
        assert_eq!(ChannelSourceMode::parse(""), None);
    }

    #[test]
    fn ttl_clamps_into_the_allowed_window() {
        assert_eq!(clamp_ttl(Duration::from_millis(1)), MIN_INJECT_TTL);
        assert_eq!(clamp_ttl(Duration::from_secs(3600)), MAX_INJECT_TTL);
        let mid = Duration::from_millis(1500);
        assert_eq!(clamp_ttl(mid), mid);
    }

    // ── authority arbitration ────────────────────────────────────────────────

    #[test]
    fn fixed_modes_ignore_the_pic_view() {
        let claimed = PicView {
            claimed: true,
            holder: Some("operator".into()),
        };
        assert_eq!(
            resolve_authority(ChannelSourceMode::Hid, Some(&claimed), Some("ai")),
            Authority::Hid
        );
        assert_eq!(
            resolve_authority(ChannelSourceMode::Inject, Some(&claimed), None),
            Authority::Inject
        );
    }

    #[test]
    fn hybrid_pic_holder_wins() {
        // A non-injector holder (the human path) takes authority.
        let human = PicView {
            claimed: true,
            holder: Some("hdmi-kiosk".into()),
        };
        assert_eq!(
            resolve_authority(ChannelSourceMode::Hybrid, Some(&human), Some("ai-mission")),
            Authority::Hid
        );
        // The injector itself holding PIC keeps the programmatic lane.
        let robot = PicView {
            claimed: true,
            holder: Some("ai-mission".into()),
        };
        assert_eq!(
            resolve_authority(ChannelSourceMode::Hybrid, Some(&robot), Some("ai-mission")),
            Authority::Inject
        );
        // A claim with no holder id (defensive) reads as the human path.
        let anon = PicView {
            claimed: true,
            holder: None,
        };
        assert_eq!(
            resolve_authority(ChannelSourceMode::Hybrid, Some(&anon), Some("ai-mission")),
            Authority::Hid
        );
    }

    #[test]
    fn hybrid_unclaimed_pic_feeds_the_programmatic_lane() {
        // A FRESH, affirmative unclaimed report — the arbiter IS reporting and
        // says no one holds — lets the programmatic lane feed.
        let unclaimed = PicView::default();
        assert_eq!(
            resolve_authority(ChannelSourceMode::Hybrid, Some(&unclaimed), None),
            Authority::Inject
        );
    }

    #[test]
    fn hybrid_holds_safe_when_the_arbiter_is_unavailable() {
        // A dead / hung PIC arbiter reports nothing (None). Hybrid must NOT hand
        // the autonomous injector authority on a missing verdict — it holds to
        // the human/neutral path even with a live injector id present.
        assert_eq!(
            resolve_authority(ChannelSourceMode::Hybrid, None, Some("ai-mission")),
            Authority::Hid
        );
        // The fixed modes are the operator's explicit choice and ignore the
        // arbiter entirely, reporting or not.
        assert_eq!(
            resolve_authority(ChannelSourceMode::Inject, None, Some("ai-mission")),
            Authority::Inject
        );
        assert_eq!(
            resolve_authority(ChannelSourceMode::Hid, None, None),
            Authority::Hid
        );
    }

    #[test]
    fn merge_hybrid_switches_sources_with_the_pic_claim() {
        let mut merge = SourceMerge::new(ChannelSourceMode::Hybrid);
        let now = t0();
        let injected = [CHANNEL_MID; CHANNEL_COUNT];
        let mut hid = [CHANNEL_MID; CHANNEL_COUNT];
        hid[2] = CHANNEL_MAX;
        merge
            .inject_all(injected, DEFAULT_INJECT_TTL, now, Some("ai".into()))
            .unwrap();
        merge.set_hid(hid).unwrap();

        // A fresh unclaimed report: the injection flies.
        merge.set_pic(Some(PicView::default()));
        assert_eq!(merge.current(now), (injected, Some(ChannelSource::Inject)));

        // A human claims PIC: the HID path takes over on the same tick.
        merge.set_pic(Some(PicView {
            claimed: true,
            holder: Some("operator".into()),
        }));
        assert_eq!(merge.current(now), (hid, Some(ChannelSource::Hid)));

        // The injector claims PIC: the programmatic lane wins again.
        merge.set_pic(Some(PicView {
            claimed: true,
            holder: Some("ai".into()),
        }));
        assert_eq!(merge.current(now), (injected, Some(ChannelSource::Inject)));
    }

    #[test]
    fn merge_hybrid_holds_safe_until_a_fresh_arbiter_report() {
        // A brand-new merge has never heard from the arbiter (pic unavailable):
        // the injector must not fly even with a live injected set.
        let mut merge = SourceMerge::new(ChannelSourceMode::Hybrid);
        let now = t0();
        merge
            .inject_all(
                [CHANNEL_MAX; CHANNEL_COUNT],
                DEFAULT_INJECT_TTL,
                now,
                Some("ai".into()),
            )
            .unwrap();
        assert_eq!(merge.authority(now), Authority::Hid);
        assert_eq!(merge.current(now), (ChannelBank::neutral(), None));

        // A human is at the sticks while the arbiter is still down: HID keeps
        // control, the injector still never wins.
        let mut hid = ChannelBank::neutral();
        hid[0] = CHANNEL_MAX;
        merge.set_hid(hid).unwrap();
        assert_eq!(merge.current(now), (hid, Some(ChannelSource::Hid)));

        // A FRESH unclaimed report finally arrives: only now may the
        // programmatic lane feed.
        merge.clear_hid();
        merge.set_pic(Some(PicView::default()));
        assert_eq!(
            merge.current(now),
            ([CHANNEL_MAX; CHANNEL_COUNT], Some(ChannelSource::Inject))
        );
    }

    #[test]
    fn merge_hybrid_fails_safe_when_a_reporting_arbiter_goes_away() {
        // The injector is flying under a fresh unclaimed report; then the
        // arbiter dies (a later read returns None). The injector must lose
        // authority immediately — a stale/absent verdict is never consent.
        let mut merge = SourceMerge::new(ChannelSourceMode::Hybrid);
        let now = t0();
        let injected = [CHANNEL_MID; CHANNEL_COUNT];
        merge
            .inject_all(injected, DEFAULT_INJECT_TTL, now, Some("ai".into()))
            .unwrap();
        merge.set_pic(Some(PicView::default()));
        assert_eq!(merge.current(now), (injected, Some(ChannelSource::Inject)));

        // The arbiter stops reporting: fail safe, injector loses, neutral hold.
        merge.set_pic(None);
        assert_eq!(merge.authority(now), Authority::Hid);
        assert_eq!(merge.current(now), (ChannelBank::neutral(), None));
    }

    #[test]
    fn losing_source_never_falls_through() {
        // HID authority with no HID data transmits neutral, NOT the live
        // injected set — the losing source must not fly the aircraft.
        let mut merge = SourceMerge::new(ChannelSourceMode::Hid);
        let now = t0();
        merge
            .inject_all([CHANNEL_MAX; CHANNEL_COUNT], DEFAULT_INJECT_TTL, now, None)
            .unwrap();
        assert_eq!(merge.current(now), (ChannelBank::neutral(), None));
    }

    // ── injection TTL ────────────────────────────────────────────────────────

    #[test]
    fn injection_expires_to_neutral() {
        let mut merge = SourceMerge::new(ChannelSourceMode::Inject);
        let now = t0();
        let values = [CHANNEL_MID; CHANNEL_COUNT];
        merge
            .inject_all(values, Duration::from_millis(500), now, None)
            .unwrap();
        // Live inside the TTL.
        assert_eq!(
            merge.current(now + Duration::from_millis(400)),
            (values, Some(ChannelSource::Inject))
        );
        // Expired: the safe neutral, with no fabricated source label.
        assert_eq!(
            merge.current(now + Duration::from_millis(600)),
            (ChannelBank::neutral(), None)
        );
    }

    #[test]
    fn a_refresh_extends_the_ttl() {
        let mut merge = SourceMerge::new(ChannelSourceMode::Inject);
        let now = t0();
        let values = [CHANNEL_MID; CHANNEL_COUNT];
        merge
            .inject_all(values, Duration::from_millis(500), now, None)
            .unwrap();
        let refresh_at = now + Duration::from_millis(400);
        merge
            .inject_all(values, Duration::from_millis(500), refresh_at, None)
            .unwrap();
        assert_eq!(
            merge.current(now + Duration::from_millis(800)),
            (values, Some(ChannelSource::Inject))
        );
    }

    #[test]
    fn inject_one_bases_on_the_live_set_but_never_a_stale_one() {
        let mut merge = SourceMerge::new(ChannelSourceMode::Inject);
        let now = t0();
        let mut values = ChannelBank::neutral();
        values[4] = 1500;
        merge
            .inject_all(values, Duration::from_millis(500), now, None)
            .unwrap();

        // A single-channel write inside the TTL keeps the rest of the set.
        let fresh_at = now + Duration::from_millis(100);
        merge
            .inject_one(7, 1000, Duration::from_millis(500), fresh_at, None)
            .unwrap();
        let (current, _) = merge.current(fresh_at);
        assert_eq!(current[4], 1500);
        assert_eq!(current[7], 1000);

        // After expiry a single-channel write bases on NEUTRAL: the stale
        // channel 4 value must not resurrect.
        let late = fresh_at + Duration::from_secs(10);
        merge
            .inject_one(7, 1000, Duration::from_millis(500), late, None)
            .unwrap();
        let (current, src) = merge.current(late);
        assert_eq!(current[4], ChannelBank::neutral()[4], "no stale resurrect");
        assert_eq!(current[7], 1000);
        assert_eq!(src, Some(ChannelSource::Inject));
    }

    #[test]
    fn expired_injection_loses_its_hybrid_identity() {
        // In hybrid, an EXPIRED injection's client id no longer counts as the
        // injector for authority: with the injector holding PIC but its values
        // stale, the lane transmits neutral (Inject authority via the claim is
        // gone — the holder no longer matches a live injector, so HID wins,
        // and with no HID data that is neutral).
        let mut merge = SourceMerge::new(ChannelSourceMode::Hybrid);
        let now = t0();
        merge
            .inject_all(
                [CHANNEL_MAX; CHANNEL_COUNT],
                Duration::from_millis(200),
                now,
                Some("ai".into()),
            )
            .unwrap();
        merge.set_pic(Some(PicView {
            claimed: true,
            holder: Some("ai".into()),
        }));
        let late = now + Duration::from_secs(5);
        assert_eq!(merge.authority(late), Authority::Hid);
        assert_eq!(merge.current(late), (ChannelBank::neutral(), None));
    }

    #[test]
    fn injection_validation_rejects_before_apply() {
        let mut merge = SourceMerge::new(ChannelSourceMode::Inject);
        let now = t0();
        let mut bad = [CHANNEL_MID; CHANNEL_COUNT];
        bad[0] = CHANNEL_MIN - 1;
        assert!(merge
            .inject_all(bad, DEFAULT_INJECT_TTL, now, None)
            .is_err());
        assert_eq!(merge.current(now), (ChannelBank::neutral(), None));
    }

    // ── HID slot lifecycle ───────────────────────────────────────────────────

    #[test]
    fn hid_values_hold_until_cleared() {
        // evdev is edge-triggered: a held stick produces no events, so HID
        // values persist while the reader lives and vanish when it clears.
        let mut merge = SourceMerge::new(ChannelSourceMode::Hid);
        let now = t0();
        let mut hid = ChannelBank::neutral();
        hid[0] = CHANNEL_MAX;
        merge.set_hid(hid).unwrap();
        assert_eq!(
            merge.current(now + Duration::from_secs(3600)),
            (hid, Some(ChannelSource::Hid))
        );
        merge.clear_hid();
        assert_eq!(merge.current(now), (ChannelBank::neutral(), None));
    }

    // ── the PIC sidecar read ────────────────────────────────────────────────

    #[test]
    fn pic_view_reads_the_sidecar_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pic-state.json");
        std::fs::write(
            &path,
            r#"{"version":1,"state":"claimed","claimed_by":"op-a","claim_counter":3}"#,
        )
        .unwrap();
        let view = read_pic_view(&path, SystemTime::now()).unwrap();
        assert!(view.claimed);
        assert_eq!(view.holder.as_deref(), Some("op-a"));

        std::fs::write(&path, r#"{"state":"unclaimed","claimed_by":null}"#).unwrap();
        let view = read_pic_view(&path, SystemTime::now()).unwrap();
        assert!(!view.claimed);
        assert!(view.holder.is_none());
    }

    #[test]
    fn pic_view_is_none_when_absent_stale_or_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pic-state.json");
        assert!(read_pic_view(&path, SystemTime::now()).is_none());

        std::fs::write(&path, b"not json").unwrap();
        assert!(read_pic_view(&path, SystemTime::now()).is_none());

        std::fs::write(&path, r#"{"state":"claimed","claimed_by":"op-a"}"#).unwrap();
        let future = SystemTime::now() + PIC_STALE_AFTER + Duration::from_secs(5);
        assert!(read_pic_view(&path, future).is_none(), "stale view dropped");
    }
}
