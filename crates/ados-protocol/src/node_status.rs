//! Compact node status and identity for the auxiliary radio lane.
//!
//! An operator paired only to a ground station sees the drone it relays as a
//! bare row: no capabilities, no services, no board. Everything the fleet view
//! wants lives on the drone's own `/api/status`, and the WFB link carries no IP,
//! so none of it reaches the ground node. This module is the wire shape that
//! closes that gap, carried on the auxiliary lane's status and identity
//! channels.
//!
//! ## Why a dedicated schema and not the heartbeat payload
//!
//! The cloud heartbeat already assembles exactly these facts, but its payload is
//! a hundred-plus camelCase fields sized for an HTTP POST. One aux frame carries
//! [`AUX_MAX_PAYLOAD`](crate::aux_mux::AUX_MAX_PAYLOAD) = 1200 bytes, and the
//! lane is shared with MAVLink at telemetry rates. So this is a deliberately
//! separate, deliberately small projection: the fields a ground operator acts
//! on, and nothing else.
//!
//! Two economies do the work. The encoding is msgpack rather than JSON, matching
//! the state socket's v2 form. Field names are two or three characters, because
//! msgpack writes map keys as strings and twenty-odd long names would spend
//! several hundred bytes on nothing but labels. Every field is documented here,
//! which is where the readability lives.
//!
//! ## Absent means unknown, never zero
//!
//! Every optional field is `skip_serializing_if`, so a fact the drone could not
//! read is ABSENT from the frame rather than sent as a plausible-looking `0` or
//! `false`. A ground reader shows "unknown", never a fabricated reading
//! (operating rule 44). This is also what keeps the frame small in practice: a
//! node with no flight controller spends no bytes saying so.
//!
//! ## Trimming, not fragmentation
//!
//! A snapshot that will not fit is TRIMMED, in a fixed order, until it does. It
//! is never split across frames.
//!
//! Fragmentation was considered and rejected. It would need a reassembly buffer,
//! a reassembly timeout, and a partial-frame policy on the receiving side, and
//! on a lossy radio lane one dropped fragment discards the whole snapshot
//! anyway. Status is periodic and idempotent: losing one costs a second, and the
//! next is complete. Trimming keeps the receiver a pure function of one datagram
//! with no cross-frame state to get wrong.
//!
//! In practice trimming never fires. A fully-populated snapshot measures a few
//! hundred bytes against a 1200-byte budget, and a test pins the worst case with
//! every field at its maximum length. The ladder exists so that a future field,
//! or an unusually long board name, degrades predictably instead of silently
//! failing to send.
//!
//! ## Bounded by construction
//!
//! Every variable-length field is capped at build time by the [`NodeStatus`]
//! builders and [`NodeIdentity::build`] — strings truncated, the failed-service list
//! capped at [`MAX_FAILED_NAMES`]. The cap is not a formality: the failed-unit
//! list is the one field an operator most wants and the one that grows without
//! bound on a badly broken node, which is exactly when the lane must not be
//! flooded.

use serde::{Deserialize, Serialize};

use crate::aux_mux::AUX_MAX_PAYLOAD;

/// Schema version for both the status and identity frames.
///
/// Bump on any breaking field-shape change. A reader that does not recognise the
/// version drops the frame rather than misreading it: the two rigs are upgraded
/// independently, so a mixed-version pair is a normal state, not an error.
pub const NODE_STATUS_VERSION: u8 = 1;

/// Longest device id carried. Real ids are 12 hex characters, so this is
/// headroom rather than a limit that bites.
pub const MAX_DEVICE_ID: usize = 32;

/// Longest human-facing node name carried.
pub const MAX_NAME: usize = 48;

/// Longest short free-text field (agent version, board name, SoC, firmware
/// family, camera and video state).
pub const MAX_SHORT: usize = 32;

/// Most failed-unit names carried in one snapshot.
///
/// A healthy node sends none. A node with more failures than this reports the
/// first few by name and the true total in [`NodeStatus::sf`], so the
/// count stays honest even when the list is clipped.
pub const MAX_FAILED_NAMES: usize = 6;

/// Longest single failed-unit name carried.
pub const MAX_UNIT_NAME: usize = 40;

/// Truncate `s` to at most `max` characters, respecting character boundaries so
/// a multi-byte name never becomes invalid UTF-8.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Clip and normalise an optional string, mapping empty to `None` so a
/// blank source reads as unknown rather than as a present-but-empty value.
fn clip_opt(s: Option<&str>, max: usize) -> Option<String> {
    let v = s?.trim();
    if v.is_empty() {
        return None;
    }
    Some(clip(v, max))
}

/// A drone's identity, sent less often than status because it does not change.
///
/// This is the frame that lets a ground station answer "who is my peer" with the
/// node's real identity. The presence beacon carries a device id, but the beacon
/// is a fixed 68-byte HMAC-signed frame with its identity field already full, so
/// there is no room in it for a name or an agent version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeIdentity {
    /// Schema version ([`NODE_STATUS_VERSION`]).
    pub v: u8,
    /// The node's persistent device id, the key everything else correlates on.
    pub id: String,
    /// Human-facing node name, when the node has one configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nm: Option<String>,
    /// The node's profile (`drone`, `ground-station`, ...), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    /// Agent version string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,
}

impl NodeIdentity {
    /// Build a bounded identity frame. Every field is clipped here, so no caller
    /// can put an unbounded string on the lane.
    pub fn build(
        device_id: &str,
        name: Option<&str>,
        profile: Option<&str>,
        version: Option<&str>,
    ) -> Self {
        Self {
            v: NODE_STATUS_VERSION,
            id: clip(device_id.trim(), MAX_DEVICE_ID),
            nm: clip_opt(name, MAX_NAME),
            pr: clip_opt(profile, MAX_SHORT),
            ver: clip_opt(version, MAX_SHORT),
        }
    }

    /// Encode to msgpack. `None` when the frame would exceed one aux payload,
    /// which the field caps make unreachable; it is a guard, not a path.
    pub fn encode(&self) -> Option<Vec<u8>> {
        let bytes = rmp_serde::to_vec_named(self).ok()?;
        (bytes.len() <= AUX_MAX_PAYLOAD).then_some(bytes)
    }

    /// Decode a msgpack identity frame, rejecting an unrecognised schema version
    /// and an empty device id (an id-less identity correlates to nothing and is
    /// worse than no frame at all).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let me: Self = rmp_serde::from_slice(bytes).ok()?;
        if me.v != NODE_STATUS_VERSION || me.id.trim().is_empty() {
            return None;
        }
        Some(me)
    }
}

/// A compact projection of a node's status, sized for one aux frame.
///
/// Field names are short because msgpack writes map keys verbatim; each is
/// documented, and the `with_*` builders are the only way to put external data
/// into one, so the bounds cannot be bypassed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeStatus {
    /// Schema version ([`NODE_STATUS_VERSION`]).
    pub v: u8,
    /// The device id this snapshot describes, so a receiver can key it without
    /// depending on an identity frame having arrived first.
    pub id: String,
    /// Monotonic sequence number. Lets a receiver spot loss and ignore a
    /// datagram that arrives out of order, which UDP permits even over loopback.
    pub sq: u32,
    /// Agent uptime in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up: Option<u32>,
    /// Agent version string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ver: Option<String>,

    // ── board ────────────────────────────────────────────────────────────
    /// Board name (`rock-5c-lite`, `rpi4b`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bn: Option<String>,
    /// Board SoC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bs: Option<String>,
    /// Board capability tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bt: Option<u8>,

    // ── flight controller ────────────────────────────────────────────────
    /// The honest connected-or-reachable verdict: true for a live MAVLink
    /// heartbeat, and also true for a healthy MSP flight controller
    /// (Betaflight/iNav), which never emits a MAVLink heartbeat but is
    /// reachable and drivable over the byte-transparent proxy. False only
    /// when no flight controller is evidenced at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fc: Option<bool>,
    /// Whether MAVLink heartbeats are actually arriving. Distinct from [`Self::fc`]
    /// on purpose: an MSP flight controller is reachable but never sends a MAVLink
    /// heartbeat, and a serial port can be open with nothing behind it. Collapsing
    /// the two would report a silent link as a live one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fa: Option<bool>,
    /// Firmware family from the USB descriptor (`betaflight`, `inav`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fv: Option<String>,
    /// Canonical firmware family (`ardupilot`, `px4`, `betaflight`, `inav`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ff: Option<String>,

    // ── services ─────────────────────────────────────────────────────────
    /// Units in `running`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sr: Option<u16>,
    /// Units failed. This is the TRUE total even when [`Self::sn`] is clipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sf: Option<u16>,
    /// Units in any other state (dead, exited, inactive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub so: Option<u16>,
    /// Names of failed units, capped at [`MAX_FAILED_NAMES`]. Which service
    /// failed is the single most actionable fact a ground operator can have, so
    /// it is worth the bytes; the cap is what keeps it safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sn: Option<Vec<String>>,

    // ── resources ────────────────────────────────────────────────────────
    /// CPU percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cp: Option<f32>,
    /// Memory used percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mp: Option<f32>,
    /// Root filesystem used percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dp: Option<f32>,
    /// SoC temperature in Celsius.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tc: Option<f32>,

    // ── payload ──────────────────────────────────────────────────────────
    /// Camera state (`ready`, `recovering`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cs: Option<String>,
    /// Video pipeline state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vs: Option<String>,
    /// How many video legs the node is serving.
    ///
    /// A ground station relays a drone it cannot otherwise interrogate — the
    /// radio carries no IP — so without this it cannot tell a single-camera
    /// drone from a pod shipping two concurrent legs, and any surface that
    /// needs to know has to assume. Absent means the drone did not say, which
    /// is deliberately distinct from a reported `1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vn: Option<u8>,
}

/// The fields a snapshot sheds, lowest value first, when it will not fit.
///
/// Order is deliberate. Failed-unit NAMES go first because the failed COUNT
/// survives them and carries most of the signal. Then the payload states, then
/// the board descriptors, then resource gauges, then firmware detail. The core
/// that never sheds is the version, device id, sequence, and the two flight
/// controller booleans: identity plus whether the aircraft is talking.
const TRIM_LADDER: &[fn(&mut NodeStatus)] = &[
    |s| s.sn = None,
    |s| {
        s.cs = None;
        s.vs = None;
        s.vn = None;
    },
    |s| {
        s.bs = None;
        s.bn = None;
        s.bt = None;
    },
    |s| {
        s.tc = None;
        s.dp = None;
        s.mp = None;
        s.cp = None;
    },
    |s| {
        s.fv = None;
        s.ff = None;
    },
    |s| {
        s.ver = None;
        s.up = None;
        s.sr = None;
        s.so = None;
    },
];

impl NodeStatus {
    /// Start a snapshot with the always-present core. Optional fields are set by
    /// the caller through the `with_*` helpers, each of which clips its input.
    pub fn new(device_id: &str, seq: u32) -> Self {
        Self {
            v: NODE_STATUS_VERSION,
            id: clip(device_id.trim(), MAX_DEVICE_ID),
            sq: seq,
            ..Default::default()
        }
    }

    /// Set the service summary. `failed_names` is clipped to
    /// [`MAX_FAILED_NAMES`] entries of [`MAX_UNIT_NAME`] characters; `failed`
    /// stays the true total so a clipped list never understates the fault.
    pub fn with_services(
        mut self,
        running: u16,
        failed: u16,
        other: u16,
        failed_names: &[String],
    ) -> Self {
        self.sr = Some(running);
        self.sf = Some(failed);
        self.so = Some(other);
        if !failed_names.is_empty() {
            self.sn = Some(
                failed_names
                    .iter()
                    .take(MAX_FAILED_NAMES)
                    .map(|n| clip(n, MAX_UNIT_NAME))
                    .collect(),
            );
        }
        self
    }

    /// Set the board descriptors, clipping each string.
    pub fn with_board(mut self, name: Option<&str>, soc: Option<&str>, tier: Option<u8>) -> Self {
        self.bn = clip_opt(name, MAX_SHORT);
        self.bs = clip_opt(soc, MAX_SHORT);
        self.bt = tier;
        self
    }

    /// Set the flight controller block, clipping each string.
    pub fn with_fc(
        mut self,
        connected: Option<bool>,
        alive: Option<bool>,
        variant: Option<&str>,
        firmware: Option<&str>,
    ) -> Self {
        self.fc = connected;
        self.fa = alive;
        self.fv = clip_opt(variant, MAX_SHORT);
        self.ff = clip_opt(firmware, MAX_SHORT);
        self
    }

    /// Set the resource gauges.
    pub fn with_resources(
        mut self,
        cpu: Option<f32>,
        mem: Option<f32>,
        disk: Option<f32>,
        temp_c: Option<f32>,
    ) -> Self {
        self.cp = cpu;
        self.mp = mem;
        self.dp = disk;
        self.tc = temp_c;
        self
    }

    /// Set the payload states, clipping each string.
    ///
    /// `video_legs` is how many video legs the node serves. `None` when the node
    /// could not read its own stream list: a ground station reading this must
    /// show "unknown", never assume one camera.
    pub fn with_payload(
        mut self,
        camera: Option<&str>,
        video: Option<&str>,
        video_legs: Option<u8>,
    ) -> Self {
        self.cs = clip_opt(camera, MAX_SHORT);
        self.vs = clip_opt(video, MAX_SHORT);
        self.vn = video_legs;
        self
    }

    /// Set uptime and agent version.
    pub fn with_agent(mut self, uptime_s: Option<u32>, version: Option<&str>) -> Self {
        self.up = uptime_s;
        self.ver = clip_opt(version, MAX_SHORT);
        self
    }

    /// Encode to msgpack, shedding fields down [`TRIM_LADDER`] until the frame
    /// fits one aux payload.
    ///
    /// Returns the bytes and how many ladder steps were applied, so a producer
    /// can log that it degraded rather than trimming invisibly. `None` only when
    /// even the fully-trimmed core does not fit, which the caps make
    /// unreachable; a producer that somehow hits it must send nothing rather
    /// than a truncated frame, because a truncated msgpack body decodes as
    /// garbage instead of failing cleanly.
    pub fn encode(&self) -> Option<(Vec<u8>, usize)> {
        let mut candidate = self.clone();
        let mut applied = 0usize;
        loop {
            if let Ok(bytes) = rmp_serde::to_vec_named(&candidate) {
                if bytes.len() <= AUX_MAX_PAYLOAD {
                    return Some((bytes, applied));
                }
            }
            // Out of ladder: send nothing rather than a frame that cannot fit.
            let step = TRIM_LADDER.get(applied)?;
            step(&mut candidate);
            applied += 1;
        }
    }

    /// Decode a msgpack status frame, rejecting an unrecognised schema version
    /// and an empty device id.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let me: Self = rmp_serde::from_slice(bytes).ok()?;
        if me.v != NODE_STATUS_VERSION || me.id.trim().is_empty() {
            return None;
        }
        Some(me)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot with every field at its maximum permitted length: the worst
    /// case the caps allow.
    fn maximal() -> NodeStatus {
        let long = "x".repeat(200);
        let names: Vec<String> = (0..MAX_FAILED_NAMES + 20)
            .map(|i| format!("{}-{i}", "u".repeat(120)))
            .collect();
        NodeStatus::new(&"d".repeat(120), u32::MAX)
            .with_agent(Some(u32::MAX), Some(&long))
            .with_board(Some(&long), Some(&long), Some(255))
            .with_fc(Some(true), Some(true), Some(&long), Some(&long))
            .with_services(u16::MAX, u16::MAX, u16::MAX, &names)
            .with_resources(Some(99.9), Some(99.9), Some(99.9), Some(99.9))
            .with_payload(Some(&long), Some(&long), Some(u8::MAX))
    }

    #[test]
    fn a_maximal_snapshot_fits_one_frame_untrimmed() {
        // The headline budget claim. Every field at its cap must still fit, and
        // must fit WITHOUT trimming: if this ever needs the ladder, the schema
        // has outgrown the frame and that is a design decision, not a runtime
        // surprise.
        let (bytes, trimmed) = maximal().encode().expect("maximal snapshot encodes");
        assert_eq!(trimmed, 0, "maximal snapshot should not need trimming");
        assert!(
            bytes.len() <= AUX_MAX_PAYLOAD,
            "maximal snapshot is {} bytes, budget is {AUX_MAX_PAYLOAD}",
            bytes.len()
        );
        // Pin the measured worst case (638 bytes, ~53% of budget). A schema
        // change that pushes past this ceiling should fail here and be a
        // conscious decision, not a silent slide toward the cap.
        assert!(
            bytes.len() <= 700,
            "worst-case snapshot grew to {} bytes; budget headroom is shrinking",
            bytes.len()
        );
    }

    #[test]
    fn build_bounds_every_variable_length_field() {
        let s = maximal();
        assert_eq!(s.id.chars().count(), MAX_DEVICE_ID);
        assert_eq!(s.ver.as_ref().unwrap().chars().count(), MAX_SHORT);
        assert_eq!(s.bn.as_ref().unwrap().chars().count(), MAX_SHORT);
        assert_eq!(s.ff.as_ref().unwrap().chars().count(), MAX_SHORT);
        assert_eq!(s.cs.as_ref().unwrap().chars().count(), MAX_SHORT);
        let names = s.sn.as_ref().unwrap();
        assert_eq!(names.len(), MAX_FAILED_NAMES);
        assert!(names.iter().all(|n| n.chars().count() <= MAX_UNIT_NAME));
    }

    #[test]
    fn a_clipped_failed_list_keeps_the_true_failed_count() {
        // The clip must not make a badly broken node look less broken: the list
        // is capped but the count is not.
        let names: Vec<String> = (0..50).map(|i| format!("ados-svc-{i}")).collect();
        let s = NodeStatus::new("abc", 1).with_services(3, 50, 1, &names);
        assert_eq!(s.sn.as_ref().unwrap().len(), MAX_FAILED_NAMES);
        assert_eq!(s.sf, Some(50));
    }

    #[test]
    fn round_trips_through_msgpack() {
        let s = NodeStatus::new("abcdef123456", 42)
            .with_agent(Some(3600), Some("1.2.3"))
            .with_board(Some("rock-5c-lite"), Some("rk3588s2"), Some(3))
            .with_fc(
                Some(true),
                Some(false),
                Some("betaflight"),
                Some("betaflight"),
            )
            .with_services(12, 1, 2, &["ados-video".to_string()])
            .with_resources(Some(12.5), Some(48.0), Some(31.2), Some(52.0))
            .with_payload(Some("ready"), Some("streaming"), Some(2));
        let (bytes, trimmed) = s.encode().unwrap();
        assert_eq!(trimmed, 0);
        assert_eq!(NodeStatus::decode(&bytes).unwrap(), s);
    }

    #[test]
    fn a_realistic_snapshot_is_a_small_fraction_of_the_budget() {
        // Guards against the schema quietly bloating: a normal drone's snapshot
        // should leave plenty of headroom, not sit just under the cap.
        let s = NodeStatus::new("abcdef123456", 7)
            .with_agent(Some(3600), Some("0.99.241"))
            .with_board(Some("rock-5c-lite"), Some("rk3588s2"), Some(3))
            .with_fc(Some(true), Some(true), None, Some("ardupilot"))
            .with_services(14, 0, 3, &[])
            .with_resources(Some(12.5), Some(48.0), Some(31.2), Some(52.0))
            .with_payload(Some("ready"), Some("streaming"), Some(2));
        let (bytes, _) = s.encode().unwrap();
        assert!(
            bytes.len() < AUX_MAX_PAYLOAD / 3,
            "realistic snapshot grew to {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn the_video_leg_count_crosses_the_lane_and_unknown_stays_unknown() {
        // A ground station cannot reach a relayed drone over IP, so this frame
        // is the only way it learns the drone serves more than one camera.
        // Reporting two must survive the round trip, and a drone that could not
        // read its own stream list must arrive as absent — not as a plausible
        // `1`, which is what made a two-leg pod read as single-camera.
        let two = NodeStatus::new("abcdef123456", 1).with_payload(None, None, Some(2));
        let back = NodeStatus::decode(&two.encode().unwrap().0).unwrap();
        assert_eq!(back.vn, Some(2));

        let unknown = NodeStatus::new("abcdef123456", 1).with_payload(None, None, None);
        let back = NodeStatus::decode(&unknown.encode().unwrap().0).unwrap();
        assert_eq!(back.vn, None, "unknown must not decode as a count");
    }

    #[test]
    fn absent_fields_cost_nothing_and_stay_absent() {
        // Honest-unknown must also be the cheap case: a bare snapshot carries
        // only its core, and decodes back with every optional still None.
        let bare = NodeStatus::new("abcdef123456", 1);
        let (bytes, _) = bare.encode().unwrap();
        assert!(bytes.len() < 64, "bare snapshot is {} bytes", bytes.len());
        let back = NodeStatus::decode(&bytes).unwrap();
        assert_eq!(back.cp, None);
        assert_eq!(back.fc, None);
        assert_eq!(back.sn, None);
    }

    #[test]
    fn the_trim_ladder_sheds_in_order_and_keeps_the_core() {
        // Drive the ladder directly: each step must drop its own fields and
        // leave identity plus the flight controller booleans standing.
        let mut s = maximal();
        for (i, step) in TRIM_LADDER.iter().enumerate() {
            step(&mut s);
            assert_eq!(s.v, NODE_STATUS_VERSION, "step {i} dropped the version");
            assert!(!s.id.is_empty(), "step {i} dropped the device id");
            assert_eq!(s.fc, Some(true), "step {i} dropped fc connected");
            assert_eq!(s.fa, Some(true), "step {i} dropped mavlink alive");
        }
        assert!(s.sn.is_none() && s.cs.is_none() && s.bn.is_none() && s.cp.is_none());
        // The failed COUNT outlives the ladder: it is core signal, not detail.
        assert_eq!(s.sf, Some(u16::MAX));
    }

    #[test]
    fn a_foreign_or_future_version_is_dropped_not_misread() {
        let mut s = NodeStatus::new("abcdef123456", 1);
        s.v = NODE_STATUS_VERSION + 1;
        let bytes = rmp_serde::to_vec_named(&s).unwrap();
        assert!(NodeStatus::decode(&bytes).is_none());
        assert!(NodeStatus::decode(b"not msgpack at all").is_none());
    }

    #[test]
    fn an_idless_snapshot_or_identity_is_rejected() {
        // An id-less frame correlates to no peer, so it is worse than nothing.
        let s = NodeStatus::new("   ", 1);
        let bytes = rmp_serde::to_vec_named(&s).unwrap();
        assert!(NodeStatus::decode(&bytes).is_none());

        let idy = NodeIdentity::build("", None, None, None);
        let bytes = rmp_serde::to_vec_named(&idy).unwrap();
        assert!(NodeIdentity::decode(&bytes).is_none());
    }

    #[test]
    fn identity_round_trips_and_is_bounded() {
        let idy = NodeIdentity::build(
            &"d".repeat(99),
            Some(&"n".repeat(99)),
            Some("drone"),
            Some("0.99.241"),
        );
        assert_eq!(idy.id.chars().count(), MAX_DEVICE_ID);
        assert_eq!(idy.nm.as_ref().unwrap().chars().count(), MAX_NAME);
        let bytes = idy.encode().expect("encodes");
        assert!(bytes.len() <= AUX_MAX_PAYLOAD);
        assert_eq!(NodeIdentity::decode(&bytes).unwrap(), idy);
    }

    #[test]
    fn blank_optional_strings_read_as_unknown() {
        // An empty source string must become absent, not present-and-empty: the
        // ground side renders absent as "unknown" and "" as a real value.
        let s = NodeStatus::new("abc", 1)
            .with_board(Some("  "), None, None)
            .with_fc(None, None, Some(""), None);
        assert_eq!(s.bn, None);
        assert_eq!(s.fv, None);
    }

    #[test]
    fn a_multibyte_name_clips_without_breaking_utf8() {
        // Clipping on characters, not bytes, so a name of multi-byte glyphs
        // never becomes invalid UTF-8 on the wire.
        let idy = NodeIdentity::build("abc", Some(&"\u{00e9}".repeat(200)), None, None);
        let name = idy.nm.as_ref().unwrap();
        assert_eq!(name.chars().count(), MAX_NAME);
        let bytes = idy.encode().unwrap();
        assert_eq!(NodeIdentity::decode(&bytes).unwrap().nm, idy.nm);
    }
}
