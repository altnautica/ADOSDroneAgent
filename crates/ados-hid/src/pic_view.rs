//! The PIC-verdict authority half of the RC/attitude merge.
//!
//! This module owns the PIC arbiter's *report* — the sidecar read plus the
//! pure decision that maps a mode and the arbiter's holder onto the source that
//! has authority — so any control lane (the RC packer in `ados-crsf`, the
//! attitude rung in the MAVLink router) shares the one verdict rather than each
//! re-deriving it from a caller-supplied label.
//!
//! The rules this half encodes are the launch-critical ones:
//!
//! * A PIC arbiter that is **not reporting** (its sidecar absent, unreadable,
//!   malformed, or stale) is treated as **UNKNOWN**, never as "no human wants
//!   control": hybrid fails SAFE to the human/neutral hold, so a dead or hung
//!   arbiter can never hand the autonomous injector authority on a missing
//!   verdict.
//! * While a client holds the PIC claim, the lane obeys that client's lane:
//!   the programmatic injector when the holder IS the injector's verified
//!   identity, the human HID path for any other holder.
//! * A FRESH, affirmative "no claim held" report lets the programmatic lane
//!   feed.
//!
//! The injected-set identity consulted here is the **verified** identity (a
//! credential minted against the pairing key), never the caller-supplied
//! `client_id` label on the wire request — holding the two under one name is
//! how the label came to be used as the credential, and the build-item calls
//! that out as the spoof to fix.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// How fresh the PIC arbiter's state sidecar must be to count as a live view.
/// The arbiter daemon rewrites it on every transition and each watchdog tick
/// (~5 s); beyond this window the arbiter is not reporting and the view is
/// treated as unavailable, NOT as an affirmative unclaimed — so hybrid mode
/// fails safe to a human/neutral hold rather than handing the programmatic
/// lane authority on a missing verdict.
pub const PIC_STALE_AFTER: Duration = Duration::from_secs(20);

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
/// arbiter is not reporting, which the caller treats as unavailable and fails
/// safe (never as an affirmative unclaimed that would grant the injector).
pub fn read_pic_view(path: &Path, now: SystemTime) -> Option<PicView> {
    let meta = std::fs::metadata(path).ok()?;
    // Fail CLOSED on an unreadable or future mtime. An errored metadata read, or
    // a `now` that has stepped behind the file's mtime (a backward wall-clock
    // correction on an RTC-less SBC after boot), yields None so the arbiter reads
    // as NOT reporting (safe hold). Previously such a case skipped the staleness
    // gate and returned a fresh view, which — if that view were an affirmative
    // "unclaimed" — could hand the autonomous injector authority from a dead or
    // hung arbiter, the exact outcome this fail-safe exists to prevent.
    let modified = meta.modified().ok()?;
    let age = now.duration_since(modified).ok()?;
    if age > PIC_STALE_AFTER {
        return None;
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
/// is absent, unreadable, malformed, or stale). `verified_injector` is the
/// ATTESTED identity of the currently-LIVE injected set (`None` when there is
/// no live injection or it carried no identity). A caller-supplied label is not
/// this — this is the credential `InjectorClaim::resolve` minted against the
/// pairing key, so a spoofed holder name cannot claim the lane.
pub fn resolve_authority(
    mode: ChannelSourceMode,
    pic: Option<&PicView>,
    verified_injector: Option<&str>,
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
            Some(view) if view.claimed => match (view.holder.as_deref(), verified_injector) {
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

#[cfg(test)]
mod tests {
    use super::*;

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

        // A backward wall-clock step: `now` is BEHIND the file's just-written
        // mtime, so the freshness math errors. This must fail CLOSED (None),
        // never return the view as fresh, or a dead arbiter's lingering
        // "unclaimed" could hand the injector authority.
        std::fs::write(&path, r#"{"state":"unclaimed","claimed_by":null}"#).unwrap();
        let stepped_back = SystemTime::now() - Duration::from_secs(60);
        assert!(
            read_pic_view(&path, stepped_back).is_none(),
            "future mtime (backward clock step) fails closed"
        );
    }
}
