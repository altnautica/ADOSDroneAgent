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
//! # Base profile and hero leases
//!
//! The profile input is itself a composition, owned by [`EncoderControl`]:
//!
//! ```text
//! effective profile = hero   while any hero lease is live
//!                   = base   otherwise (the boot profile or the last plain set)
//! ```
//!
//! A plain set (the ground station's fleet-hero assignment) moves the base. A
//! lease is a holder's time-bounded claim on hero (an operator viewing this
//! drone full-screen), dropped when released or when it expires, with no
//! further request; the attached vision engine holds one with no expiry. See
//! [`lease`].
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
pub mod lease;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::config::CameraConfig;
pub use crate::config::CameraProfile as EncoderSettings;

pub use ipc::{
    read_state, read_state_from, release_lease, release_lease_at, serve, set_bitrate_ceiling,
    set_bitrate_ceiling_at, set_lease, set_lease_at, set_profile, set_profile_at, state, state_at,
    VIDEO_ENCODER_SOCK, VIDEO_PROFILE_SIDECAR,
};
use lease::{LeaseTable, LeaseView, MonotonicClock, SystemClock};

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncoderState {
    /// The effective profile: hero while any lease is live, else the base.
    pub profile: VideoProfile,
    /// The adaptive ladder's clamp, or `None` when unclamped. `ados-radio`
    /// self-heals off this field, so it must always be present in the sidecar
    /// (JSON `null` when clear).
    pub ceiling_kbps: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// The ceiling above was ACCEPTED but has deliberately NOT been pushed to
    /// the encoder, because applying it would have cost a respawn worth more
    /// than the change buys (see the orchestrator's ceiling-deferral rule).
    /// `bitrate_kbps` therefore reports what the encoder is really running, not
    /// the clamp — the two disagreeing is the whole point of this flag, and a
    /// consumer that renders the ceiling as the live bitrate would be reporting
    /// a value the encoder never received.
    #[serde(default)]
    pub ceiling_deferred: bool,
    /// Encoder-only respawns since `ados-video` started. A climbing counter is
    /// the visible form of a respawn storm: every respawn costs a fresh
    /// SPS/PPS, a decoder reset and a black frame, so an oscillating adaptive
    /// ladder is diagnosable from this sidecar instead of from an operator
    /// reporting a blinking picture. The radio ladder carries its own
    /// equivalent counter.
    #[serde(default)]
    pub encoder_respawns: u64,
    /// The profile the node runs with no lease live: the boot profile or the
    /// last plain set. `None` only in a sidecar written before leases existed.
    #[serde(default)]
    pub base_profile: Option<VideoProfile>,
    /// The live hero leases. `expires_in_s` is `None` for an internal lease
    /// with no expiry (the attached vision engine).
    #[serde(default)]
    pub leases: Vec<LeaseView>,
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
            ceiling_deferred: false,
            encoder_respawns: 0,
            base_profile: Some(profile),
            leases: Vec::new(),
        }
    }

    /// Mark the accepted ceiling as held back from the encoder.
    pub fn with_ceiling_deferred(mut self, deferred: bool) -> Self {
        self.ceiling_deferred = deferred;
        self
    }

    /// Stamp the encoder-only respawn count the publisher has observed.
    pub fn with_encoder_respawns(mut self, respawns: u64) -> Self {
        self.encoder_respawns = respawns;
        self
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
    /// The effective profile (see [`lease`]), not the base.
    pub profile: VideoProfile,
    pub ceiling_kbps: Option<u32>,
    pub generation: u64,
}

/// What the orchestrator last actually spawned the encoder with.
#[derive(Debug, Clone, PartialEq)]
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
/// never clobber each other. The base profile and the hero leases share one
/// lock because the effective profile is a function of both.
#[derive(Debug)]
pub struct EncoderControl {
    desired: watch::Sender<Desired>,
    applied: watch::Sender<Applied>,
    leases: Mutex<LeaseTable>,
}

impl EncoderControl {
    /// A control seeded with the boot state: `profile`, no ceiling, and the
    /// settings the orchestrator is about to cold-start the encoder with.
    pub fn new(profile: VideoProfile, settings: EncoderSettings) -> Arc<Self> {
        Self::with_clock(profile, settings, Arc::new(SystemClock))
    }

    /// [`Self::new`] with lease expiry read from `clock` (tests inject one).
    pub fn with_clock(
        profile: VideoProfile,
        settings: EncoderSettings,
        clock: Arc<dyn MonotonicClock>,
    ) -> Arc<Self> {
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
        Arc::new(Self {
            desired,
            applied,
            leases: Mutex::new(LeaseTable::new(profile, clock)),
        })
    }

    /// The currently requested profile + ceiling.
    pub fn desired(&self) -> Desired {
        *self.desired.borrow()
    }

    /// The state the encoder is actually running.
    pub fn applied(&self) -> Applied {
        self.applied.borrow().clone()
    }

    /// Watch handle the orchestrator's run loop selects on.
    pub fn subscribe_desired(&self) -> watch::Receiver<Desired> {
        self.desired.subscribe()
    }

    /// Request a base profile. Returns the generation to wait on. A live
    /// lease keeps the effective profile at hero until it ends.
    pub fn request_profile(&self, profile: VideoProfile) -> u64 {
        self.compose(|t| t.base = profile)
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
    /// generation at or below `generation`. Returns `state` stamped with the
    /// base profile and the live leases, which is what the caller publishes.
    pub fn note_applied(
        &self,
        state: EncoderState,
        generation: u64,
        restarted: bool,
    ) -> EncoderState {
        let state = self.stamp_attention(state);
        self.applied.send_replace(Applied {
            state: state.clone(),
            generation,
            restarted,
        });
        state
    }

    /// Block until the orchestrator has applied a request at least as new as
    /// `generation`, or the bound elapses. `None` on timeout — the caller
    /// reports the request as accepted-but-unconfirmed rather than lying that
    /// it took effect.
    pub async fn wait_applied(&self, generation: u64, bound: Duration) -> Option<Applied> {
        let mut rx = self.applied.subscribe();
        if rx.borrow().generation >= generation {
            return Some(rx.borrow().clone());
        }
        tokio::time::timeout(bound, async {
            loop {
                if rx.changed().await.is_err() {
                    return None;
                }
                let a = rx.borrow().clone();
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
mod tests;
