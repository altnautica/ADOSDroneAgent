//! Attention-based video: the hero / thumbnail encoder-profile policy.
//!
//! ADOS flies a whole fleet on ONE 20 MHz channel with ONE radio per node, so
//! video is allocated by *attention*: exactly one drone is the **hero** and
//! encodes at the full `video.camera` settings; every other registered drone
//! drops to the **thumbnail** profile. Both ride the existing p0 pipeline and
//! the existing `wfb_tx` — switching restarts only the encoder child, and adds
//! no radio port, no second encoder process and no ground-side receiver.
//!
//! ```text
//! hero:      1280x720, 30 fps, 4000 kbps   (the existing defaults, unchanged)
//! thumbnail:  320x180,  1 fps,   50 kbps
//! ```
//!
//! # The airtime arithmetic these numbers exist to satisfy
//!
//! Measured on the bench (ch149, 20 MHz, **MCS 1** = 13.0 Mbps PHY, SNR 35 dB):
//!
//! | contributor                                     | airtime |
//! |-------------------------------------------------|---------|
//! | 1 hero video (4.165 Mbps payload x 1.5 FEC)      | 48%     |
//! | 1 control-only drone (MAVLink telemetry + aux)   | 2.4%    |
//! | 1 thumbnail's video                              | ~0.4%   |
//!
//! So a 24-drone fleet after this policy is
//! `48 + 23*0.4 + 23*2.4` = **~112% at MCS 1** — still over the channel.
//!
//! **The committed fleet size at MCS 1 alone is 8** (1 hero + 7 thumbnails
//! = ~68%). Reaching 24 REQUIRES the adaptive-MCS ladder: at MCS 3 (26.0 Mbps,
//! needs 11 dB against the 35 dB measured) the same 24-drone fleet lands at
//! ~56%. Do not read "24 drones" anywhere in this codebase as a supported
//! MCS-1 configuration — thumbnails buy the headroom, they do not close the gap
//! on their own.
//!
//! # Two orthogonal inputs, one applier
//!
//! Two independent controllers steer the same encoder and must not clobber each
//! other:
//!
//! * the **profile** (this module's [`VideoProfile`]) owns width / height / fps
//!   and the profile's nominal bitrate — driven by the operator's hero choice;
//! * the **bitrate ceiling** ([`EncoderControl::request_ceiling`]) is a clamp
//!   only — driven by the adaptive-bitrate ladder in `ados-radio`.
//!
//! They compose in one place, [`resolve`]:
//!
//! ```text
//! width / height / fps = profile
//! bitrate_kbps         = min(profile.bitrate_kbps, ceiling.unwrap_or(MAX))
//! ```
//!
//! `min` is the only correct composition in both directions: a hero on a bad
//! link MUST be clamped down to the rescue tier, and a thumbnail at 50 kbps must
//! NOT be raised to the rescue tier's 1200 kbps. Neither controller does a
//! read-modify-write, so neither can lose a race against the other.
//!
//! # Boot default
//!
//! A node that shares the WFB channel boots **thumbnail**, so a fleet powering
//! up together never has 24 aircraft radiating at 48% airtime each. The ground
//! station promotes exactly one hero from its slot registry (and auto-promotes a
//! one-drone fleet, preserving today's single-drone behaviour). A node that is
//! *not* on the shared channel — `video.mode` anything but `wfb`, i.e. a LAN /
//! cloud-only rig — has no channel to protect and boots **hero**, which is
//! byte-identical to what it does today. See [`boot_profile`].

pub mod ipc;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config::CameraConfig;
pub use crate::config::CameraProfile as EncoderSettings;

pub use ipc::{
    read_state, read_state_from, serve, set_bitrate_ceiling, set_bitrate_ceiling_at, set_profile,
    set_profile_at, state, state_at, VIDEO_ENCODER_SOCK, VIDEO_PROFILE_SIDECAR,
};

/// Which attention profile the encoder is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VideoProfile {
    /// The operator's selected aircraft: the full `video.camera` settings.
    Hero,
    /// Every other registered aircraft: 320x180 / 1 fps / 50 kbps.
    #[default]
    Thumbnail,
}

impl VideoProfile {
    /// The wire string (`"hero"` / `"thumbnail"`) used by the HTTP routes, the
    /// command socket and the sidecar.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hero => "hero",
            Self::Thumbnail => "thumbnail",
        }
    }

    /// Parse a wire string. Case-sensitive and exact — an operator typo must
    /// fail loudly rather than silently pick a profile.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hero" => Some(Self::Hero),
            "thumbnail" => Some(Self::Thumbnail),
            _ => None,
        }
    }
}

impl std::fmt::Display for VideoProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The profile a node boots into, from its `video.mode`.
///
/// `wfb` shares one channel with the rest of the fleet, so it boots
/// `Thumbnail` and waits to be promoted. Anything else (a LAN / cloud-only
/// rig, or a bench node) has no shared channel to protect, so it boots `Hero` —
/// exactly the resolution / fps / bitrate it produces today.
pub fn boot_profile(video_mode: &str) -> VideoProfile {
    if video_mode == "wfb" {
        VideoProfile::Thumbnail
    } else {
        VideoProfile::Hero
    }
}

/// The base (unclamped) settings for a profile: the hero profile is the
/// top-level `video.camera` block, the thumbnail profile its `thumbnail:`
/// sub-block.
pub fn base_settings(profile: VideoProfile, cfg: &CameraConfig) -> EncoderSettings {
    match profile {
        VideoProfile::Hero => EncoderSettings {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate_kbps: cfg.bitrate_kbps,
        },
        VideoProfile::Thumbnail => cfg.thumbnail,
    }
}

/// Compose the two orthogonal inputs into the settings the encoder actually
/// runs: geometry and frame rate come from the profile, bitrate is the
/// profile's nominal value clamped by the adaptive ladder's ceiling.
///
/// See the module docs for why this is `min` and not last-writer-wins.
pub fn resolve(
    profile: VideoProfile,
    ceiling_kbps: Option<u32>,
    cfg: &CameraConfig,
) -> EncoderSettings {
    let mut s = base_settings(profile, cfg);
    if let Some(c) = ceiling_kbps {
        s.bitrate_kbps = s.bitrate_kbps.min(c);
    }
    s
}

/// The full encoder attention state, as published on the sidecar and returned
/// by every command-socket op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderState {
    pub profile: VideoProfile,
    /// The adaptive ladder's clamp, or `None` when unclamped. `ados-radio`
    /// self-heals off this field, so it must always be present in the sidecar
    /// (JSON `null` when clear).
    pub ceiling_kbps: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl EncoderState {
    pub fn new(profile: VideoProfile, ceiling_kbps: Option<u32>, s: EncoderSettings) -> Self {
        Self {
            profile,
            ceiling_kbps,
            width: s.width,
            height: s.height,
            fps: s.fps,
            bitrate_kbps: s.bitrate_kbps,
        }
    }

    pub fn settings(&self) -> EncoderSettings {
        EncoderSettings {
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
        }
    }
}

/// What the command socket asked for, and the monotonic request generation a
/// waiter blocks on until the orchestrator has actually applied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desired {
    pub profile: VideoProfile,
    pub ceiling_kbps: Option<u32>,
    pub generation: u64,
}

/// What the orchestrator last actually spawned the encoder with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub state: EncoderState,
    /// The highest [`Desired::generation`] this reflects.
    pub generation: u64,
    /// Whether applying it required an encoder restart (`false` = the resolved
    /// settings were already live, so nothing was touched).
    pub restarted: bool,
}

/// The shared handle between the command-socket server (which requests) and the
/// video orchestrator (which applies).
///
/// Deliberately two watch channels rather than one lock: a requester never
/// reads-then-writes, it only publishes its own concern and waits for the
/// applier's acknowledgement, so the profile setter and the ceiling setter can
/// never clobber each other.
#[derive(Debug)]
pub struct EncoderControl {
    desired: watch::Sender<Desired>,
    applied: watch::Sender<Applied>,
}

impl EncoderControl {
    /// A control seeded with the boot state: `profile`, no ceiling, and the
    /// settings the orchestrator is about to cold-start the encoder with.
    pub fn new(profile: VideoProfile, settings: EncoderSettings) -> Arc<Self> {
        let state = EncoderState::new(profile, None, settings);
        let (desired, _) = watch::channel(Desired {
            profile,
            ceiling_kbps: None,
            generation: 0,
        });
        let (applied, _) = watch::channel(Applied {
            state,
            generation: 0,
            restarted: false,
        });
        Arc::new(Self { desired, applied })
    }

    /// The currently requested profile + ceiling.
    pub fn desired(&self) -> Desired {
        *self.desired.borrow()
    }

    /// The state the encoder is actually running.
    pub fn applied(&self) -> Applied {
        *self.applied.borrow()
    }

    /// Watch handle the orchestrator's run loop selects on.
    pub fn subscribe_desired(&self) -> watch::Receiver<Desired> {
        self.desired.subscribe()
    }

    /// Request a base profile. Returns the generation to wait on.
    pub fn request_profile(&self, profile: VideoProfile) -> u64 {
        self.bump(|d| d.profile = profile)
    }

    /// Request an adaptive bitrate clamp (`None` clears it). Returns the
    /// generation to wait on.
    pub fn request_ceiling(&self, ceiling_kbps: Option<u32>) -> u64 {
        self.bump(|d| d.ceiling_kbps = ceiling_kbps)
    }

    fn bump(&self, f: impl FnOnce(&mut Desired)) -> u64 {
        let mut generation = 0;
        self.desired.send_modify(|d| {
            f(d);
            d.generation += 1;
            generation = d.generation;
        });
        generation
    }

    /// Record what the orchestrator spawned, releasing any waiter blocked on a
    /// generation at or below `generation`.
    pub fn note_applied(&self, state: EncoderState, generation: u64, restarted: bool) {
        self.applied.send_replace(Applied {
            state,
            generation,
            restarted,
        });
    }

    /// Block until the orchestrator has applied a request at least as new as
    /// `generation`, or the bound elapses. `None` on timeout — the caller
    /// reports the request as accepted-but-unconfirmed rather than lying that
    /// it took effect.
    pub async fn wait_applied(&self, generation: u64, bound: Duration) -> Option<Applied> {
        let mut rx = self.applied.subscribe();
        if rx.borrow().generation >= generation {
            return Some(*rx.borrow());
        }
        tokio::time::timeout(bound, async {
            loop {
                if rx.changed().await.is_err() {
                    return None;
                }
                let a = *rx.borrow();
                if a.generation >= generation {
                    return Some(a);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
}

/// Write the attention state sidecar with a temp-file-plus-rename, so a reader
/// polling it at 10 Hz (the state-snapshot builder, which feeds the swarm
/// beacon's hero bit) can never observe a torn file.
pub fn write_sidecar(path: &Path, state: &EncoderState) -> std::io::Result<()> {
    let body = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
}

/// [`write_sidecar`] on the blocking pool.
///
/// Called from the async health tick; the tmp-plus-rename is correct and the
/// sync filesystem work is what does not belong on a reactor worker.
pub async fn write_sidecar_async(
    path: std::path::PathBuf,
    state: EncoderState,
) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || write_sidecar(&path, &state))
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CameraConfig {
        CameraConfig::default()
    }

    #[test]
    fn hero_is_the_existing_camera_defaults_and_thumbnail_is_the_small_profile() {
        let c = cfg();
        assert_eq!(
            base_settings(VideoProfile::Hero, &c),
            EncoderSettings {
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_kbps: 4000
            }
        );
        assert_eq!(
            base_settings(VideoProfile::Thumbnail, &c),
            EncoderSettings {
                width: 320,
                height: 180,
                fps: 1,
                bitrate_kbps: 50
            }
        );
    }

    #[test]
    fn a_wfb_node_boots_thumbnail_and_an_off_channel_node_boots_hero() {
        // The fleet case: 24 aircraft powering up together must not each grab
        // 48% airtime.
        assert_eq!(boot_profile("wfb"), VideoProfile::Thumbnail);
        // A rig that shares no channel keeps today's behaviour exactly.
        assert_eq!(boot_profile("cloud"), VideoProfile::Hero);
        assert_eq!(boot_profile("disabled"), VideoProfile::Hero);
    }

    #[test]
    fn the_ceiling_only_ever_reduces_the_bitrate() {
        let c = cfg();
        // A hero on a bad link is clamped down to the rescue tier.
        assert_eq!(
            resolve(VideoProfile::Hero, Some(1200), &c).bitrate_kbps,
            1200
        );
        // A thumbnail is NOT raised to the rescue tier.
        assert_eq!(
            resolve(VideoProfile::Thumbnail, Some(1200), &c).bitrate_kbps,
            50
        );
        // A ceiling above the profile's nominal value is a no-op.
        assert_eq!(
            resolve(VideoProfile::Hero, Some(9000), &c).bitrate_kbps,
            4000
        );
        // No ceiling is the profile's nominal value.
        assert_eq!(resolve(VideoProfile::Hero, None, &c).bitrate_kbps, 4000);
    }

    #[test]
    fn the_ceiling_never_touches_geometry_or_frame_rate() {
        let c = cfg();
        let clamped = resolve(VideoProfile::Hero, Some(200), &c);
        assert_eq!(
            (clamped.width, clamped.height, clamped.fps),
            (1280, 720, 30)
        );
    }

    #[test]
    fn profile_wire_strings_round_trip_and_reject_typos() {
        for p in [VideoProfile::Hero, VideoProfile::Thumbnail] {
            assert_eq!(VideoProfile::parse(p.as_str()), Some(p));
        }
        assert_eq!(VideoProfile::parse("Hero"), None);
        assert_eq!(VideoProfile::parse("thumb"), None);
        assert_eq!(VideoProfile::parse(""), None);
    }

    #[test]
    fn the_two_setters_compose_without_clobbering_each_other() {
        let ctl = EncoderControl::new(VideoProfile::Thumbnail, cfg().thumbnail);
        let g1 = ctl.request_ceiling(Some(1200));
        let g2 = ctl.request_profile(VideoProfile::Hero);
        assert!(g2 > g1);
        let d = ctl.desired();
        // The profile request did not wipe the ceiling, nor vice versa.
        assert_eq!(d.profile, VideoProfile::Hero);
        assert_eq!(d.ceiling_kbps, Some(1200));
        assert_eq!(
            resolve(d.profile, d.ceiling_kbps, &cfg()).bitrate_kbps,
            1200
        );
    }

    #[tokio::test]
    async fn a_waiter_unblocks_only_once_its_own_generation_is_applied() {
        let ctl = EncoderControl::new(VideoProfile::Thumbnail, cfg().thumbnail);
        let g = ctl.request_profile(VideoProfile::Hero);
        // Nothing applied yet: the wait must not resolve.
        assert!(ctl
            .wait_applied(g, Duration::from_millis(20))
            .await
            .is_none());
        let hero = base_settings(VideoProfile::Hero, &cfg());
        ctl.note_applied(EncoderState::new(VideoProfile::Hero, None, hero), g, true);
        let applied = ctl
            .wait_applied(g, Duration::from_millis(20))
            .await
            .unwrap();
        assert_eq!(applied.state.profile, VideoProfile::Hero);
        assert_eq!(applied.state.bitrate_kbps, 4000);
        assert!(applied.restarted);
    }

    #[test]
    fn the_sidecar_round_trips_and_always_carries_the_ceiling_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("video-profile.json");
        let st = EncoderState::new(
            VideoProfile::Hero,
            None,
            base_settings(VideoProfile::Hero, &cfg()),
        );
        write_sidecar(&path, &st).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["profile"], "hero");
        // `ados-radio` self-heals off this key; it must exist even when clear.
        assert!(raw
            .get("ceiling_kbps")
            .is_some_and(serde_json::Value::is_null));
        assert_eq!(read_state_from(&path).unwrap(), st);
        // No temp file left behind.
        assert!(!path.with_extension("json.tmp").exists());
    }
}
