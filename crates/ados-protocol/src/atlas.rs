//! ADOS Atlas world-model wire contract.
//!
//! The topic names and wire structs for the world-model program: a drone
//! captures pose-tagged keyframes, a compute node reconstructs a 3D world model
//! (splat / cloud / mesh / occupancy), and the result is consumable as shared
//! data rather than only viewable. Two topic roots:
//!
//! - `atlas.*` is the agent's own service namespace (capture state, the
//!   offloaded-pose return leg).
//! - `plugin.atlas.*` is the shared-data namespace a plugin subscribes to **once
//!   it holds the capability that topic names** — see
//!   [`atlas_topic_subscribe_capability`]. It is not an open namespace: a
//!   reconstruction is derived imagery of wherever the aircraft flew, and an
//!   occupancy field is a planning input, so both are capability-gated. An
//!   earlier unqualified "any plugin subscribes to" here is what let the gap
//!   between this claim and the host's actual gate go unnoticed.
//!
//! Heavy payloads ride the shared-memory ring (see [`crate::framebus`]) or the
//! stream lane; these topics carry small descriptors only. The envelope is
//! transport-agnostic: the identical struct travels on any bearer (direct LAN,
//! the WFB relay, or the cloud relay), so no transport strings are baked in. It
//! is also tier-aware: a light descriptor (pose plus a thumbnail or an occupancy
//! slice) is small enough for an in-flight relay link, while a full keyframe
//! (full-resolution image bytes plus the IMU window) is a LAN-bulk artifact.

use serde::{Deserialize, Serialize};
use thiserror::Error;

// --- Topics ---------------------------------------------------------------

/// Capture-session state (state, keyframe counts, VIO health). Host-published.
pub const ATLAS_CAPTURE_STATE_TOPIC: &str = "atlas.capture.state";

/// A selected pose-tagged keyframe ([`KeyframeEnvelope`]) the capture service
/// emits drone-to-compute. The agent's own namespace (the keyframe is the
/// capture artifact, not shared plugin data); a compute node and the world-model
/// stream lane consume it.
pub const ATLAS_KEYFRAME_TOPIC: &str = "atlas.keyframe";

/// The compute node returns an offloaded pose to the drone on this topic. The
/// drone streamed an image to the node, the node ran SLAM, and the pose comes
/// back here for the drone to stamp into the keyframe envelope. This is the
/// localization return leg for NPU-less boards.
pub const ATLAS_POSE_OFFLOAD_TOPIC: &str = "atlas.pose.offload";

/// Shared-data: current 6-DoF pose plus world anchor (~10 Hz).
pub const PLUGIN_ATLAS_POSE_TOPIC: &str = "plugin.atlas.pose";

/// Shared-data: point-cloud descriptor (count, bounds, shm handle / url).
pub const PLUGIN_ATLAS_POINTCLOUD_TOPIC: &str = "plugin.atlas.pointcloud";

/// Shared-data: occupancy-grid descriptor (origin, resolution, dims, handle).
pub const PLUGIN_ATLAS_OCCUPANCY_TOPIC: &str = "plugin.atlas.occupancy";

/// Shared-data: "splat updated" descriptor (gaussian count, url / handle).
pub const PLUGIN_ATLAS_SPLAT_TOPIC: &str = "plugin.atlas.splat";

/// Shared-data: mesh descriptor (vertex / face count, handle / url).
pub const PLUGIN_ATLAS_MESH_TOPIC: &str = "plugin.atlas.mesh";

/// Every shared-data world-model topic, in one place, so a consumer or a gate
/// iterates the set rather than hard-coding four string literals that drift.
pub const PLUGIN_ATLAS_TOPICS: &[&str] = &[
    PLUGIN_ATLAS_POSE_TOPIC,
    PLUGIN_ATLAS_POINTCLOUD_TOPIC,
    PLUGIN_ATLAS_OCCUPANCY_TOPIC,
    PLUGIN_ATLAS_SPLAT_TOPIC,
    PLUGIN_ATLAS_MESH_TOPIC,
];

// --- Shared-data access policy --------------------------------------------

/// The capability a plugin must hold to subscribe to `topic`, or `None` when
/// `topic` is not a world-model shared-data topic.
///
/// **Exact match, never a prefix.** `plugin.atlas.occupancy.evil` resolves to
/// `None`, so it inherits nothing from `plugin.atlas.occupancy` and falls
/// through to the host's ordinary namespace rule. A prefix match here would let
/// a plugin mint a topic under a gated prefix and be handed that prefix's
/// capability, which turns the gate into a naming convention.
///
/// This exists because the `plugin.atlas.*` namespace was documented as "the
/// shared-data namespace any plugin subscribes to", while the plugin host's
/// generic rule grants a plugin only its OWN `plugin.<id>.` namespace — so in
/// practice the namespace was reachable by exactly one plugin, the one that
/// happened to be called `atlas`. Making the topics public instead would be the
/// opposite error: a reconstructed world model is derived imagery of wherever
/// the aircraft flew, and an occupancy field is a planning input, so neither
/// belongs on an ungated topic.
///
/// Note the revocation this implies once the host applies it: a plugin whose id
/// literally is `atlas` no longer gets the namespace for free.
///
/// The mapping reuses capabilities that already exist and are already
/// `enforced`, rather than minting new ones: a world-model artifact IS a
/// compute-node job result, so [`ATLAS_WORLD_READ_CAP`] gates the artifact
/// descriptors, and the live pose keeps the telemetry read it is equivalent to.
/// The host applies this as an ADDITIONAL requirement on top of
/// `event.subscribe`, never as a replacement for it.
pub fn atlas_topic_subscribe_capability(topic: &str) -> Option<&'static str> {
    match topic {
        PLUGIN_ATLAS_POSE_TOPIC => Some(ATLAS_POSE_READ_CAP),
        PLUGIN_ATLAS_POINTCLOUD_TOPIC
        | PLUGIN_ATLAS_OCCUPANCY_TOPIC
        | PLUGIN_ATLAS_SPLAT_TOPIC
        | PLUGIN_ATLAS_MESH_TOPIC => Some(ATLAS_WORLD_READ_CAP),
        _ => None,
    }
}

/// The capability gating the world-model artifact descriptors (cloud /
/// occupancy / splat / mesh). A descriptor names where a reconstruction of the
/// flown area can be fetched, which is precisely what "read compute-node job
/// status and results" already covers.
pub const ATLAS_WORLD_READ_CAP: &str = "compute.job.read";

/// The capability gating the live ~10 Hz world pose. It is the same class of
/// data as the vehicle telemetry a plugin already reads under `telemetry.read`,
/// expressed in the world frame.
pub const ATLAS_POSE_READ_CAP: &str = "telemetry.read";

/// Whether a bearer can carry a full keyframe, from the bearer name the
/// forwarder already reports (`direct-lan` / `wfb-relay` / `cloud`).
///
/// This is a pure property of the bearer, not a runtime measurement: the WFB
/// relay's datagram ceiling is 1300 bytes against a multi-MB JPEG keyframe, so
/// on a no-LAN field topology the ladder degrades to pose and status only —
/// i.e. NO world model — while the operator surface would otherwise report
/// `bearer: "wfb-relay"` as though the lane were working. Deciding it here, at
/// the contract, keeps one answer for the forwarder, the capture service and
/// the GCS instead of three.
pub fn bearer_carries_keyframes(bearer: &str) -> bool {
    !matches!(bearer, "wfb-relay")
}

/// The operator-facing reason a bearer cannot carry keyframes, or `None` when
/// it can. Short, factual, and safe to render verbatim.
pub fn bearer_keyframe_degraded_reason(bearer: &str) -> Option<&'static str> {
    match bearer {
        "wfb-relay" => Some(
            "relay datagrams are too small for full keyframes; pose and status only until a LAN bearer is available",
        ),
        _ => None,
    }
}

// --- Forwarder → capture-service handoff ----------------------------------

/// Where the drone-side Atlas forwarder writes its transport status for the
/// capture service to fold into the plugin-state sidecar.
///
/// The compute node, the active bearer, and the last-forwarded-keyframe time are
/// facts only the egress forwarder (`ados-cloud`) knows — the capture service
/// (`ados-atlas`) never sees the bearer ladder. Rather than race two processes on
/// one file, the forwarder writes these facts here and the capture service reads
/// them (freshness-gated) when it writes `atlas-state.json`, so the GCS Stream
/// card reads real values on hardware. This lives OUTSIDE `/run/ados/plugins`, so
/// the generic plugin-state readers never surface it as its own slice — it is a
/// private handoff, not a plugin.
pub const ATLAS_FORWARD_SIDECAR: &str = "/run/ados/atlas-forward.json";

/// Schema version stamped on the [`ATLAS_FORWARD_SIDECAR`] file by its writer and
/// checked (best-effort) by its reader. Held equal to the `atlas-forward` entry in
/// the sidecar registry (see [`crate::contracts`]); a drift warns and reads on.
pub const ATLAS_FORWARD_SIDECAR_VERSION: u16 = 1;

/// The forwarder → capture-service handoff payload (see [`ATLAS_FORWARD_SIDECAR`]).
/// The bearer is already in the GCS vocabulary (`direct-lan` / `wfb-relay` /
/// `cloud`) so the capture service folds it verbatim into the sidecar the GCS
/// reads; every field is optional so a forwarder with no resolved node writes an
/// honest "nothing yet".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AtlasForwardStatus {
    /// Sidecar schema version, stamped [`ATLAS_FORWARD_SIDECAR_VERSION`] on write.
    /// An older writer emitted no field, so a reader defaults it to `0` and warns
    /// (best-effort, never rejects) on a drift.
    #[serde(default)]
    pub version: u16,
    /// The resolved compute node's device id (the mDNS `deviceId`), or `None`
    /// while no compute node is on the LAN.
    #[serde(default)]
    pub compute_node_id: Option<String>,
    /// The bearer currently carrying the stream, in the GCS vocabulary
    /// (`direct-lan` / `wfb-relay` / `cloud`), or `None` before anything has been
    /// forwarded this run.
    #[serde(default)]
    pub bearer: Option<String>,
    /// Epoch ms a keyframe was last forwarded toward the compute node, or `None`
    /// if none has been forwarded yet this run.
    #[serde(default)]
    pub last_kf_at_ms: Option<i64>,
    /// Local write time (epoch ms), for the reader's own freshness reasoning.
    pub generated_at_ms: i64,
}

// --- Enums ----------------------------------------------------------------

/// Which camera on the rig produced a keyframe. Camera count is configurable
/// from one camera to an all-sides rig; the role tags each stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CameraRole {
    Primary,
    Aux,
    Down,
    Left,
    Right,
    Back,
    Up,
}

/// Delivery tier of a keyframe. A `Light` descriptor fits an in-flight relay
/// link (a thumbnail or an occupancy slice); a `Full` keyframe carries the
/// full-resolution image and IMU window for a LAN-bulk or post-flight pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyframeTier {
    Light,
    Full,
}

/// Where a keyframe's pose came from. Both produce the identical envelope, so
/// nothing downstream forks; only the producer changed.
///
/// `Default` is [`PoseSource::LocalVio`]: the flight controller's fused pose is
/// the floor every board has, so a decoder filling an absent field lands on the
/// tier that is always present rather than claiming an offload that may not
/// exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoseSource {
    /// Computed on-board by the drone's own VIO (a VIO-capable board).
    #[default]
    LocalVio,
    /// Computed on the compute node from a streamed image and returned on
    /// [`ATLAS_POSE_OFFLOAD_TOPIC`] (an NPU-less board, first-class).
    OffloadedSlam,
}

/// Image encoding carried in a full keyframe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageEncoding {
    Jpeg,
    /// HEVC keyframe (I-frame) bytes. Serializes as `hevc-keyframe`.
    HevcKeyframe,
}

// --- Structs --------------------------------------------------------------

/// Camera intrinsics for one `camera_id`. `k` is the 3x3 pinhole matrix in
/// row-major order; `distortion` names the model and its parameters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraIntrinsics {
    /// Row-major 3x3 intrinsic matrix K (wire key `K`, the math convention).
    #[serde(rename = "K")]
    pub k: [f64; 9],
    pub distortion: Distortion,
    /// Whether these intrinsics came from a real calibration, or were derived
    /// from the frame size and a nominal field of view.
    ///
    /// A derived pinhole with zero distortion is a guess, and a rig captured
    /// with one produces a metrically WRONG reconstruction — scaled, and bent at
    /// the frame edges — with no other symptom. So the guess travels labelled:
    /// `false` here is what lets the compute node treat the matrix as an initial
    /// estimate to refine and lets the operator surface badge the capture as
    /// uncalibrated, instead of both silently trusting it. `#[serde(default)]`
    /// reads an older producer's frame as uncalibrated, which is the honest
    /// direction to fail.
    #[serde(default)]
    pub calibrated: bool,
}

/// A lens-distortion model name plus its parameters (e.g. `radtan`, `equidist`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Distortion {
    pub model: String,
    pub params: Vec<f64>,
}

/// The number of elements in a row-major 6x6 pose covariance.
pub const POSE_COV_LEN: usize = 36;

/// A 6-DoF pose. `r` is a row-major 3x3 rotation (world-from-camera), `t` the
/// translation, `cov` an optional 6x6 covariance (36 row-major elements).
///
/// `cov` stays OPTIONAL here deliberately. This struct is also the ~10 Hz live
/// pose descriptor ([`PoseDescriptor`]), and 36 `f64` is 288 bytes — a fifth of
/// the whole 1300-byte relay datagram budget, per message, for a value no live
/// consumer reads. The consumer that genuinely requires a sigma is the
/// reconstructor, and it reads [`KeyframeEnvelope::pose_cov`], which is
/// required. So the cost is paid on the keyframe lane that needs it and not on
/// the pose lane that does not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    /// Row-major 3x3 rotation (wire key `R`, the math convention).
    #[serde(rename = "R")]
    pub r: [f64; 9],
    pub t: [f64; 3],
    pub cov: Option<Vec<f64>>,
}

/// A geo anchor stamped on the first keyframe of a session so the local world
/// frame can be georeferenced.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GlobalAnchor {
    pub lat: f64,
    pub lon: f64,
    pub alt_m: f64,
    pub yaw_rad: f64,
}

/// One IMU sample in a keyframe's pre-integration window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ImuSample {
    pub t_ms: i64,
    pub gyro: [f64; 3],
    pub accel: [f64; 3],
}

/// Image bytes carried in a keyframe. For a `Light` tier this may be a
/// thumbnail; for a `Full` tier it is the full-resolution frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyframeImage {
    pub encoding: ImageEncoding,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

/// Per-keyframe boolean flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyframeFlags {
    pub is_loop_closure: bool,
    pub is_session_start: bool,
    pub is_session_end: bool,
}

/// The default ceiling on [`TimeAlignment::clock_offset_sigma_ns`] for a
/// keyframe a reconstructor will trust: 5 ms. A pose paired to a frame no
/// better than 5 ms is already worse than one frame period at 200 fps and
/// comparable to one at 30 fps, so above this the pairing cannot be placed to
/// sub-frame accuracy and the frame is not reconstruction input.
pub const KEYFRAME_MAX_OFFSET_SIGMA_NS: i64 = 5_000_000;

/// The default ceiling on [`TimeAlignment::pose_pairing_residual_ms`]: 50 ms.
/// At the documented ~10 Hz pose stream this is half a pose interval, the
/// widest gap an interpolated pose may span before the position error at cruise
/// speed exceeds the keyframe selector's own baseline threshold.
pub const KEYFRAME_MAX_PAIRING_RESIDUAL_MS: f64 = 50.0;

/// The clock alignment stamped on a keyframe, so a consumer can DECIDE whether
/// the frame's pose pairing is trustworthy instead of assuming it.
///
/// Alignment accuracy is bounded by the clock, not by the protocol. NMEA-only
/// GNSS time lands at 1-10 ms — worse than a frame period, and therefore
/// useless for frame-to-pose pairing — while a PPS-disciplined clock lands at
/// 1-10 us. The capture path cannot know which one it is running on, so it
/// reports the offset it measured AND that offset's uncertainty, and the
/// consumer rejects a keyframe whose `clock_offset_sigma_ns` exceeds its own
/// budget rather than silently reconstructing from a mis-posed frame.
///
/// `ts_monotonic_ns` is the capture-side monotonic instant of the frame (the
/// V4L2 buffer timestamp, i.e. start of exposure) — the only stamp immune to a
/// wall-clock step mid-flight. [`KeyframeEnvelope::ts_unix_ms`] is derived from
/// it plus `clock_offset_ns`. A monotonic instant is meaningful only inside one
/// process, so anything crossing a process boundary compares the derived
/// realtime stamp; the monotonic value travels so a consumer can verify the
/// derivation and order frames within one capture without trusting the wall
/// clock at all.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimeAlignment {
    /// Capture-side monotonic nanoseconds at start of exposure.
    pub ts_monotonic_ns: i64,
    /// `realtime - monotonic` at the moment the frame was stamped, so
    /// `ts_unix_ms` is reproducible from `ts_monotonic_ns`.
    pub clock_offset_ns: i64,
    /// One-sigma uncertainty on `clock_offset_ns`. Negative means UNMEASURED,
    /// which reads as untrustworthy — see [`TimeAlignment::unmeasured`].
    pub clock_offset_sigma_ns: i64,
    /// The flight controller's `CAMERA_TRIGGER.seq` this frame was joined on,
    /// when the FC provides one. Present means the pairing is exact (the FC
    /// recorded its own inertial timestamp at start of exposure and the
    /// companion joined on the sequence number); absent means the pose was
    /// interpolated and `pose_pairing_residual_ms` carries the cost.
    #[serde(default)]
    pub trigger_seq: Option<u64>,
    /// Signed milliseconds between the frame's exposure instant and the pose
    /// sample it was paired with. Zero when joined on `trigger_seq`.
    pub pose_pairing_residual_ms: f64,
}

impl Default for TimeAlignment {
    /// Fails CLOSED: an unmeasured alignment, not a perfect one. A derived
    /// `Default` would zero `clock_offset_sigma_ns`, and a zero sigma reads as
    /// "this clock is exact" — the single most dangerous possible default for a
    /// field whose whole purpose is to admit uncertainty.
    fn default() -> Self {
        Self::unmeasured()
    }
}

impl TimeAlignment {
    /// An alignment whose offset uncertainty is unknown, so every trust check
    /// refuses it. The honest value for a producer with no disciplined clock.
    pub fn unmeasured() -> Self {
        Self {
            ts_monotonic_ns: 0,
            clock_offset_ns: 0,
            clock_offset_sigma_ns: -1,
            trigger_seq: None,
            pose_pairing_residual_ms: 0.0,
        }
    }

    /// Whether a consumer may treat the frame-to-pose pairing as sub-frame
    /// accurate, against its own budgets. An unmeasured (negative) sigma is
    /// never trustworthy, and a non-finite residual (a producer bug) is
    /// refused rather than compared.
    pub fn is_trustworthy(&self, max_sigma_ns: i64, max_residual_ms: f64) -> bool {
        self.clock_offset_sigma_ns >= 0
            && self.clock_offset_sigma_ns <= max_sigma_ns
            && self.pose_pairing_residual_ms.is_finite()
            && self.pose_pairing_residual_ms.abs() <= max_residual_ms
    }
}

/// A pose-tagged keyframe sent drone-to-compute. Extends the splat-capture
/// envelope with the camera identity so multi-camera rigs are unambiguous, and
/// with the tier and pose-source so the same struct serves the LAN-bulk and the
/// in-flight-relay paths and the VIO-vs-offloaded-pose producers.
///
/// Two fields exist so a consumer can refuse a frame instead of reconstructing
/// a wrong world from it: [`TimeAlignment`] says how well the frame and its
/// pose are time-paired, and `pose_cov` carries the pose sigma the
/// reconstructor's position prior needs. Check both with
/// [`KeyframeEnvelope::validate`] before a frame enters a dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyframeEnvelope {
    pub session_id: String,
    pub kf_id: u64,
    /// Wall-clock milliseconds at start of exposure, derived from
    /// `time.ts_monotonic_ns + time.clock_offset_ns`.
    pub ts_unix_ms: i64,
    pub camera_id: String,
    pub camera_role: CameraRole,
    pub tier: KeyframeTier,
    pub image: KeyframeImage,
    pub camera: CameraIntrinsics,
    pub pose: Pose,
    /// Row-major 6x6 covariance of `pose`, `POSE_COV_LEN` elements. REQUIRED.
    ///
    /// This is the reconstructor's position-prior sigma. COLMAP's
    /// `--prior_position_std_*` defaults to 1.0 m, which is wrong for both
    /// RTK-fixed (0.02-0.05 m) and consumer GNSS (1.5-3 m), and a prior with no
    /// sigma is the documented cause of `pose_prior_mapper` instability — so a
    /// missing covariance does not degrade the reconstruction quietly, it
    /// destabilises the bundle adjustment. Carried on the keyframe lane only,
    /// never on the ~10 Hz pose lane (see [`Pose::cov`]).
    ///
    /// `#[serde(default)]` decodes an older producer's frame to an EMPTY vec,
    /// which [`KeyframeEnvelope::validate`] rejects — the honest direction to
    /// fail, rather than inventing an identity covariance that asserts a
    /// precision nobody measured.
    #[serde(default)]
    pub pose_cov: Vec<f64>,
    pub pose_source: PoseSource,
    /// How well this frame and its pose are time-paired. `#[serde(default)]`
    /// reads an older producer's frame as [`TimeAlignment::unmeasured`], which
    /// fails every trust check.
    #[serde(default)]
    pub time: TimeAlignment,
    pub global_anchor: Option<GlobalAnchor>,
    pub imu_window: Vec<ImuSample>,
    pub flags: KeyframeFlags,
}

/// Why a keyframe is not usable as reconstruction input.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
pub enum KeyframeReject {
    /// `pose_cov` was absent or not `POSE_COV_LEN` elements, so the
    /// reconstructor has no position-prior sigma for this frame.
    #[error("pose covariance must be {POSE_COV_LEN} elements, got {got}")]
    PoseCovLen { got: usize },
    /// The clock offset's uncertainty is unmeasured or over budget, so the
    /// frame-to-pose pairing cannot be placed to sub-frame accuracy.
    #[error("clock offset sigma {sigma_ns} ns is unmeasured or over the {budget_ns} ns budget")]
    ClockOffsetSigma { sigma_ns: i64, budget_ns: i64 },
    /// The interpolated pose sat too far from the frame's exposure instant.
    #[error("pose pairing residual {residual_ms} ms exceeds the {budget_ms} ms budget")]
    PairingResidual { residual_ms: f64, budget_ms: f64 },
    /// The session's geo anchor was not latched when the frame was captured, so
    /// its translation is in a different frame from every post-anchor keyframe.
    #[error("keyframe carries no global anchor, so its world frame is unresolved")]
    NoAnchor,
}

/// What a consumer requires of a keyframe before it enters a dataset.
///
/// The clock budget is an `Option` on purpose. Two of the three checks —
/// covariance and pairing residual — are things the drone can satisfy on any
/// board today, so gating on them hard is free. The clock-offset SIGMA is not:
/// measuring it needs a disciplined clock (chrony with a PPS refclock), so on a
/// node without one the honest report is `unmeasured`, and a hard default gate
/// would silently reject every frame and produce ZERO reconstructions with no
/// operator-visible cause. A reconstruction labelled "clock undisciplined" is
/// strictly more useful than no reconstruction at all, so the default carries
/// the sigma through for the quality gate to weigh and does not reject on it;
/// [`KeyframeBudget::strict`] is the opt-in for a rig that does have PPS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyframeBudget {
    /// Ceiling on `time.clock_offset_sigma_ns`, or `None` to carry the sigma
    /// without gating on it.
    pub max_offset_sigma_ns: Option<i64>,
    /// Ceiling on `|time.pose_pairing_residual_ms|`. A non-finite residual is
    /// always refused.
    pub max_pairing_residual_ms: f64,
    /// Whether a frame captured before the session's geo anchor latched is
    /// refused. Always `true` in practice: a pre-anchor frame sits at the local
    /// origin while every post-anchor frame sits in ENU metres from the anchor,
    /// so mixing them shows the reconstructor a teleport inside one session.
    pub require_anchor: bool,
}

impl Default for KeyframeBudget {
    fn default() -> Self {
        Self {
            max_offset_sigma_ns: None,
            max_pairing_residual_ms: KEYFRAME_MAX_PAIRING_RESIDUAL_MS,
            require_anchor: true,
        }
    }
}

impl KeyframeBudget {
    /// The budget for a rig with a disciplined clock: also gate on the
    /// clock-offset sigma, so a frame whose pairing cannot be placed to
    /// sub-frame accuracy never reaches the reconstructor.
    pub fn strict() -> Self {
        Self {
            max_offset_sigma_ns: Some(KEYFRAME_MAX_OFFSET_SIGMA_NS),
            ..Self::default()
        }
    }
}

impl KeyframeEnvelope {
    /// Whether this frame is usable as reconstruction input under the default
    /// budget ([`KeyframeBudget::default`]).
    ///
    /// Every rejection here is a frame that would otherwise enter a dataset and
    /// bake a wrong pose into the reconstruction with no other symptom, which is
    /// unrecoverable after the fact — a mis-posed keyframe does not make the
    /// output visibly broken, it makes it subtly, confidently wrong.
    pub fn validate(&self) -> Result<(), KeyframeReject> {
        self.validate_with(&KeyframeBudget::default())
    }

    /// Whether this frame is usable as reconstruction input under `budget`.
    pub fn validate_with(&self, budget: &KeyframeBudget) -> Result<(), KeyframeReject> {
        if self.pose_cov.len() != POSE_COV_LEN {
            return Err(KeyframeReject::PoseCovLen {
                got: self.pose_cov.len(),
            });
        }
        if let Some(budget_ns) = budget.max_offset_sigma_ns {
            let sigma = self.time.clock_offset_sigma_ns;
            if sigma < 0 || sigma > budget_ns {
                return Err(KeyframeReject::ClockOffsetSigma {
                    sigma_ns: sigma,
                    budget_ns,
                });
            }
        }
        let residual = self.time.pose_pairing_residual_ms;
        if !residual.is_finite() || residual.abs() > budget.max_pairing_residual_ms {
            return Err(KeyframeReject::PairingResidual {
                residual_ms: residual,
                budget_ms: budget.max_pairing_residual_ms,
            });
        }
        if budget.require_anchor && self.global_anchor.is_none() {
            return Err(KeyframeReject::NoAnchor);
        }
        Ok(())
    }

    /// Whether the clock offset behind this frame was actually measured. A
    /// dataset built from frames where this is `false` is metrically fine but
    /// its frame-to-pose pairing is only as good as an undisciplined system
    /// clock, which is a fact the reconstruction quality gate must see.
    pub fn clock_is_disciplined(&self) -> bool {
        self.time.clock_offset_sigma_ns >= 0
    }
}

/// The pose the compute node returns to the drone on
/// [`ATLAS_POSE_OFFLOAD_TOPIC`] after running SLAM on a streamed image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OffloadedPose {
    pub session_id: String,
    pub kf_id: u64,
    pub camera_id: String,
    pub pose: Pose,
    /// Always [`PoseSource::OffloadedSlam`] on this lane; carried for symmetry.
    pub source: PoseSource,
    pub ts_ms: i64,
}

/// Capture-session lifecycle state published on [`ATLAS_CAPTURE_STATE_TOPIC`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    Idle,
    Capturing,
    Paused,
    Finalizing,
    Bagged,
}

/// SLAM / VIO health summary carried with the capture state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VioHealth {
    Good,
    Degraded,
    Lost,
}

/// The descriptor on [`ATLAS_CAPTURE_STATE_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureStatus {
    pub session_id: String,
    pub state: CaptureState,
    pub keyframes: u64,
    pub vio_health: VioHealth,
    /// Count of enabled cameras (1 to N); the fusion layer keys off this.
    pub camera_count: u32,
    pub ingest_rate_hz: f32,
    /// True once the session-wide keyframe cap is reached. Keyframe selection
    /// has stopped while the pose stream and `Capturing` continue, so without
    /// this flag the operator surface shows "capturing" forever against a frozen
    /// count and cannot tell that apart from a stalled camera.
    #[serde(default)]
    pub capped: bool,
    /// True once the session's geo anchor is latched (the first 3D fix). Before
    /// it, a pose translation is `[0, 0, alt_rel]` at the origin while every
    /// post-anchor pose is ENU metres from the anchor, so a session that mixes
    /// the two shows the reconstructor a teleport.
    #[serde(default)]
    pub anchored: bool,
    /// Which producer filled the pose the capture path is currently tagging
    /// with, so a silent switch to offloaded SLAM is visible to the operator
    /// rather than inferred.
    #[serde(default)]
    pub pose_tier: PoseSource,
    /// Keyframes the capture path produced but the bus could not deliver (a
    /// subscriber too slow for the 16-deep per-client queue was pruned). A
    /// non-zero count means the reconstruction is sparser than the keyframe
    /// count claims.
    #[serde(default)]
    pub dropped_keyframes: u64,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_POSE_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PoseDescriptor {
    pub pose: Pose,
    pub anchor: Option<GlobalAnchor>,
    pub ts_ms: i64,
}

/// The monotonic generation counter every world-model artifact set is stamped
/// with. A reconstruction is a JOB producing an immutable artifact set, not a
/// stream: generation N's splat, cloud, mesh and occupancy all describe the
/// same capture state, so a consumer can diff generations and a viewer can
/// fetch coarse chunks of N while N-1 is still on screen.
pub type Generation = u64;

/// Shared-data descriptor on [`PLUGIN_ATLAS_POINTCLOUD_TOPIC`]. The heavy buffer
/// rides the shm ring (`shm_name`/`slot`/`seq`, see [`crate::framebus`]) or a
/// stream-lane `url`; this carries the summary only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointCloudDescriptor {
    /// The capture session this cloud was reconstructed from.
    #[serde(default)]
    pub session_id: String,
    /// Monotonic artifact generation (see [`Generation`]).
    #[serde(default)]
    pub generation: Generation,
    pub point_count: u64,
    /// Axis-aligned bounds: `[min_x, min_y, min_z, max_x, max_y, max_z]`.
    pub bounds: [f64; 6],
    pub shm_name: Option<String>,
    pub slot: Option<u32>,
    pub seq: Option<u64>,
    pub url: Option<String>,
}

/// What an occupancy buffer's voxels hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OccupancyField {
    /// Per-voxel occupancy probability, quantised to `u8`.
    #[default]
    Occupancy,
    /// Euclidean signed distance to the nearest surface, `f32` metres,
    /// truncated at `truncation_m`. This is what a planner actually consumes:
    /// a binary voxel dump forces every planner to run its own distance
    /// transform, and a gradient is what lets a trajectory optimiser push away
    /// from an obstacle instead of only testing collisions.
    Esdf,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_OCCUPANCY_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OccupancyDescriptor {
    /// The capture session this grid was derived from.
    #[serde(default)]
    pub session_id: String,
    /// Monotonic artifact generation (see [`Generation`]).
    #[serde(default)]
    pub generation: Generation,
    /// World-frame origin of voxel `(0,0,0)`.
    pub origin: [f64; 3],
    pub resolution_m: f32,
    /// Grid dimensions in voxels `[nx, ny, nz]`.
    pub dims: [u32; 3],
    /// What the voxels hold. `#[serde(default)]` reads an older producer's
    /// buffer as plain occupancy, never as a distance field a planner would
    /// then mis-read as metres.
    #[serde(default)]
    pub field: OccupancyField,
    /// The distance at which an [`OccupancyField::Esdf`] buffer is truncated,
    /// in metres. Meaningless (and zero) for a plain occupancy buffer.
    #[serde(default)]
    pub truncation_m: f32,
    pub shm_name: Option<String>,
    pub slot: Option<u32>,
    pub seq: Option<u64>,
    /// Where the buffer can be fetched when it is NOT in shared memory. The
    /// on-drone producer writes into the shm ring; a compute node writes a file
    /// and serves it, exactly like the sibling artifact descriptors. Row-major
    /// `nx * ny * nz`, little-endian: `u8` occupancy probability for
    /// [`OccupancyField::Occupancy`], `f32` metres for
    /// [`OccupancyField::Esdf`].
    #[serde(default)]
    pub url: Option<String>,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_SPLAT_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SplatDescriptor {
    /// The capture session this splat was trained from.
    #[serde(default)]
    pub session_id: String,
    /// Monotonic artifact generation (see [`Generation`]).
    #[serde(default)]
    pub generation: Generation,
    pub gaussian_count: u64,
    /// Training step this descriptor reflects (monotonic for live sessions).
    pub step: u64,
    pub url: Option<String>,
    pub handle: Option<String>,
    /// A chunk manifest for level-of-detail streaming, when the producer wrote
    /// one. The viewer fetches the coarsest level first and refines, so first
    /// pixels arrive in well under a second on a scene whose full transfer
    /// takes minutes.
    ///
    /// This is what replaced the specified "SPZ delta" lane. SPZ and SOG are
    /// whole-scene containers with global quantisation and Morton ordering, and
    /// NEITHER the formats NOR the Khronos glTF extensions define an
    /// incremental append or delta codec — so a delta lane would have meant
    /// inventing and maintaining a codec no other tool reads. Generation-
    /// versioned LOD chunks get the same "see it grow" behaviour out of formats
    /// that already exist.
    #[serde(default)]
    pub manifest_url: Option<String>,
    /// Number of LOD levels behind `manifest_url` (0 when there is no manifest).
    #[serde(default)]
    pub lod_levels: u8,
}

/// Shared-data descriptor on [`PLUGIN_ATLAS_MESH_TOPIC`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeshDescriptor {
    /// The capture session this mesh was extracted from.
    #[serde(default)]
    pub session_id: String,
    /// Monotonic artifact generation (see [`Generation`]).
    #[serde(default)]
    pub generation: Generation,
    pub vertex_count: u64,
    pub face_count: u64,
    pub url: Option<String>,
    pub handle: Option<String>,
}

/// The on-wire version stamped on every [`AtlasEvent`] envelope. Every atlas
/// message crosses as an envelope, and the inner topic structs are decoded from
/// its `payload` only after the envelope decodes, so versioning the envelope
/// gates the whole contract. Kept in lockstep with the `atlas.envelope` entry in
/// the contract registry (see [`crate::contracts`]).
pub const ATLAS_ENVELOPE_VERSION: u16 = 1;

/// Errors raised decoding an [`AtlasEvent`] envelope.
#[derive(Debug, Error)]
pub enum AtlasError {
    /// The msgpack body did not decode into an envelope (also fires when the
    /// required version field is absent).
    #[error("msgpack decode error: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    /// The envelope carried a version this build does not speak.
    #[error("unsupported atlas envelope version {got} (this build speaks {ours})")]
    Version { got: u16, ours: u16 },
}

/// One framed message on the agent's local atlas bus. The capture service binds
/// a single broadcast socket and tags every message with the topic it belongs to
/// (one of the `atlas.*` / `plugin.atlas.*` constants above) so a subscriber can
/// demultiplex pose, keyframe, and capture-state streams off one connection.
/// `payload` is the topic's own struct already msgpack-encoded (e.g. a
/// [`KeyframeEnvelope`] for [`PLUGIN_ATLAS_POSE_TOPIC`]'s sibling keyframe lane),
/// so the wrapper stays agnostic to which struct it carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AtlasEvent {
    /// On-wire envelope version, the first field. A decoder that sees a version
    /// it does not speak rejects the frame in [`AtlasEvent::decode`] rather than
    /// silently mis-parsing the payload; a frame missing this field also fails to
    /// decode (no serde default). Stamped by [`AtlasEvent::new`]; held equal to
    /// [`ATLAS_ENVELOPE_VERSION`].
    #[serde(rename = "v")]
    pub v: u16,
    pub topic: String,
    /// The capturing drone's device id, stamped by the drone-side forwarder as
    /// the event leaves the drone (the single choke point every bearer passes
    /// through). The compute node reads it to attribute a reconstruct job to the
    /// drone that captured it (the world-model job's `deviceId`). Additive +
    /// optional: an event on the local publish bus (before egress) omits it, and
    /// a receiver decoding an older frame defaults it to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub payload: Vec<u8>,
}

impl AtlasEvent {
    /// Build an envelope stamped with the current [`ATLAS_ENVELOPE_VERSION`].
    /// `device_id` is `None` on the local publish bus and `Some(..)` once the
    /// drone-side forwarder stamps it on egress.
    pub fn new(topic: impl Into<String>, device_id: Option<String>, payload: Vec<u8>) -> Self {
        Self {
            v: ATLAS_ENVELOPE_VERSION,
            topic: topic.into(),
            device_id,
            payload,
        }
    }

    /// Encode as a msgpack map with named keys (the version field rides as `v`).
    pub fn encode(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
        rmp_serde::to_vec_named(self)
    }

    /// Decode a msgpack envelope, rejecting a frame whose version this build does
    /// not speak (and a frame missing the version field). Decoding the envelope
    /// gates the whole atlas contract: a subscriber only reads the inner topic
    /// struct out of `payload` once this succeeds.
    pub fn decode(bytes: &[u8]) -> Result<Self, AtlasError> {
        let event: AtlasEvent = rmp_serde::from_slice(bytes)?;
        if event.v != ATLAS_ENVELOPE_VERSION {
            return Err(AtlasError::Version {
                got: event.v,
                ours: ATLAS_ENVELOPE_VERSION,
            });
        }
        Ok(event)
    }
}

macro_rules! impl_msgpack {
    ($($t:ty),+ $(,)?) => {
        $(impl $t {
            /// Encode as a msgpack map with named keys.
            pub fn to_msgpack(&self) -> Result<Vec<u8>, rmp_serde::encode::Error> {
                rmp_serde::to_vec_named(self)
            }
            /// Decode from a msgpack map.
            pub fn from_msgpack(bytes: &[u8]) -> Result<Self, rmp_serde::decode::Error> {
                rmp_serde::from_slice(bytes)
            }
        })+
    };
}

impl_msgpack!(
    KeyframeEnvelope,
    OffloadedPose,
    CaptureStatus,
    PoseDescriptor,
    PointCloudDescriptor,
    OccupancyDescriptor,
    SplatDescriptor,
    MeshDescriptor,
    TimeAlignment,
);

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pose() -> Pose {
        Pose {
            r: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            t: [1.5, -2.0, 0.5],
            cov: None,
        }
    }

    /// A diagonal 6x6 covariance: 0.03 m position sigma (RTK-fixed class) and
    /// 0.01 rad orientation sigma, squared onto the diagonal.
    fn sample_cov() -> Vec<f64> {
        let mut cov = vec![0.0; POSE_COV_LEN];
        for i in 0..3 {
            cov[i * 6 + i] = 0.03 * 0.03;
        }
        for i in 3..6 {
            cov[i * 6 + i] = 0.01 * 0.01;
        }
        cov
    }

    /// A measured alignment well inside both default budgets: a 20 us offset
    /// sigma (PPS-disciplined class) joined on an FC trigger sequence.
    fn sample_time() -> TimeAlignment {
        TimeAlignment {
            ts_monotonic_ns: 42_000_000_000,
            clock_offset_ns: 1_700_000_000_000_000_000 - 42_000_000_000,
            clock_offset_sigma_ns: 20_000,
            trigger_seq: Some(19),
            pose_pairing_residual_ms: 0.0,
        }
    }

    fn sample_intrinsics() -> CameraIntrinsics {
        CameraIntrinsics {
            k: [900.0, 0.0, 640.0, 0.0, 900.0, 360.0, 0.0, 0.0, 1.0],
            distortion: Distortion {
                model: "radtan".into(),
                params: vec![0.0, 0.0, 0.0, 0.0],
            },
            calibrated: true,
        }
    }

    fn sample_keyframe() -> KeyframeEnvelope {
        KeyframeEnvelope {
            session_id: "sess-1".into(),
            kf_id: 7,
            ts_unix_ms: 1_700_000_000_000,
            camera_id: "front".into(),
            camera_role: CameraRole::Primary,
            tier: KeyframeTier::Full,
            image: KeyframeImage {
                encoding: ImageEncoding::Jpeg,
                width: 1280,
                height: 720,
                bytes: vec![1, 2, 3, 4],
            },
            camera: sample_intrinsics(),
            pose: sample_pose(),
            pose_cov: sample_cov(),
            pose_source: PoseSource::LocalVio,
            time: sample_time(),
            global_anchor: Some(GlobalAnchor {
                lat: 12.97,
                lon: 77.59,
                alt_m: 920.0,
                yaw_rad: 0.0,
            }),
            imu_window: vec![ImuSample {
                t_ms: 1,
                gyro: [0.0, 0.0, 0.0],
                accel: [0.0, 0.0, 9.81],
            }],
            flags: KeyframeFlags {
                is_session_start: true,
                ..KeyframeFlags::default()
            },
        }
    }

    #[test]
    fn keyframe_envelope_round_trips() {
        let kf = sample_keyframe();
        let bytes = kf.to_msgpack().expect("encode");
        let back = KeyframeEnvelope::from_msgpack(&bytes).expect("decode");
        assert_eq!(kf, back);
        assert_eq!(back.camera_role, CameraRole::Primary);
        assert_eq!(back.tier, KeyframeTier::Full);
        assert_eq!(back.pose_source, PoseSource::LocalVio);
    }

    #[test]
    fn offloaded_pose_round_trips() {
        let op = OffloadedPose {
            session_id: "sess-1".into(),
            kf_id: 7,
            camera_id: "front".into(),
            pose: sample_pose(),
            source: PoseSource::OffloadedSlam,
            ts_ms: 1_700_000_000_000,
        };
        let bytes = op.to_msgpack().expect("encode");
        let back = OffloadedPose::from_msgpack(&bytes).expect("decode");
        assert_eq!(op, back);
        assert_eq!(back.source, PoseSource::OffloadedSlam);
    }

    #[test]
    fn world_model_descriptors_round_trip() {
        let status = CaptureStatus {
            session_id: "sess-1".into(),
            state: CaptureState::Capturing,
            keyframes: 42,
            vio_health: VioHealth::Good,
            camera_count: 1,
            ingest_rate_hz: 9.5,
            capped: false,
            anchored: true,
            pose_tier: PoseSource::LocalVio,
            dropped_keyframes: 0,
        };
        let back = CaptureStatus::from_msgpack(&status.to_msgpack().unwrap()).unwrap();
        assert_eq!(status, back);

        let cloud = PointCloudDescriptor {
            session_id: "sess-1".into(),
            generation: 3,
            point_count: 10_000,
            bounds: [-1.0, -1.0, -1.0, 1.0, 1.0, 1.0],
            shm_name: Some("atlas-cloud-0".into()),
            slot: Some(2),
            seq: Some(99),
            url: None,
        };
        assert_eq!(
            cloud,
            PointCloudDescriptor::from_msgpack(&cloud.to_msgpack().unwrap()).unwrap()
        );
    }

    #[test]
    fn topic_names_are_stable() {
        assert_eq!(ATLAS_CAPTURE_STATE_TOPIC, "atlas.capture.state");
        assert_eq!(ATLAS_KEYFRAME_TOPIC, "atlas.keyframe");
        assert_eq!(ATLAS_POSE_OFFLOAD_TOPIC, "atlas.pose.offload");
        assert_eq!(PLUGIN_ATLAS_POSE_TOPIC, "plugin.atlas.pose");
        assert_eq!(PLUGIN_ATLAS_POINTCLOUD_TOPIC, "plugin.atlas.pointcloud");
        assert_eq!(PLUGIN_ATLAS_OCCUPANCY_TOPIC, "plugin.atlas.occupancy");
        assert_eq!(PLUGIN_ATLAS_SPLAT_TOPIC, "plugin.atlas.splat");
        assert_eq!(PLUGIN_ATLAS_MESH_TOPIC, "plugin.atlas.mesh");
    }

    #[test]
    fn atlas_event_round_trips_and_carries_a_struct_payload() {
        // The bus wrapper carries an already-encoded topic struct as its opaque
        // payload, so a subscriber demuxes by topic and decodes the inner struct.
        let status = CaptureStatus {
            session_id: "sess-1".into(),
            state: CaptureState::Capturing,
            keyframes: 3,
            vio_health: VioHealth::Good,
            camera_count: 2,
            ingest_rate_hz: 9.5,
            capped: false,
            anchored: true,
            pose_tier: PoseSource::LocalVio,
            dropped_keyframes: 0,
        };
        let ev = AtlasEvent::new(
            ATLAS_CAPTURE_STATE_TOPIC,
            None,
            status.to_msgpack().unwrap(),
        );
        let back = AtlasEvent::decode(&ev.encode().unwrap()).unwrap();
        assert_eq!(back, ev);
        assert_eq!(back.topic, "atlas.capture.state");
        let inner = CaptureStatus::from_msgpack(&back.payload).unwrap();
        assert_eq!(inner, status);
    }

    #[test]
    fn device_id_round_trips_and_is_skipped_when_absent() {
        // Absent (the local publish-bus shape): the key is omitted on the wire and
        // an old-frame decode defaults to None, so an unstamped event is
        // byte-unchanged for a receiver that never reads it.
        let bare = AtlasEvent::new(ATLAS_KEYFRAME_TOPIC, None, vec![1, 2, 3]);
        let bare_json = serde_json::to_value(&bare).unwrap();
        assert_eq!(
            bare_json["v"], ATLAS_ENVELOPE_VERSION,
            "the envelope version rides on the wire as `v`"
        );
        assert!(
            bare_json.get("device_id").is_none(),
            "device_id is skipped when None"
        );
        assert_eq!(AtlasEvent::decode(&bare.encode().unwrap()).unwrap(), bare);

        // Stamped (the egress shape): the drone id round-trips on the wire so the
        // compute node can attribute the job to the capturing drone.
        let stamped = AtlasEvent::new(ATLAS_KEYFRAME_TOPIC, Some("drone-42".into()), vec![9]);
        let stamped_json = serde_json::to_value(&stamped).unwrap();
        assert_eq!(stamped_json["v"], ATLAS_ENVELOPE_VERSION);
        assert_eq!(stamped_json["device_id"], "drone-42");
        assert_eq!(
            AtlasEvent::decode(&stamped.encode().unwrap()).unwrap(),
            stamped
        );
    }

    #[test]
    fn wire_keys_match_the_spec() {
        // The serialized key names are the contract a Python/TS consumer reads.
        // Pin them so a serde-rename regression (which a same-struct round-trip
        // can never catch) fails the build instead of breaking the wire.
        let mut kf = sample_keyframe();
        kf.image.encoding = ImageEncoding::HevcKeyframe;
        let v = serde_json::to_value(&kf).unwrap();
        for key in [
            "session_id",
            "kf_id",
            "ts_unix_ms",
            "camera_id",
            "camera_role",
            "tier",
            "pose_source",
            "pose_cov",
            "time",
            "global_anchor",
            "flags",
        ] {
            assert!(v.get(key).is_some(), "keyframe key `{key}` missing");
        }
        for key in [
            "ts_monotonic_ns",
            "clock_offset_ns",
            "clock_offset_sigma_ns",
            "trigger_seq",
            "pose_pairing_residual_ms",
        ] {
            assert!(
                v["time"].get(key).is_some(),
                "time-alignment key `{key}` missing"
            );
        }
        assert_eq!(
            v["pose_cov"].as_array().map(|a| a.len()),
            Some(POSE_COV_LEN),
            "pose_cov rides as a 36-element row-major array"
        );
        assert!(
            v["camera"].get("calibrated").is_some(),
            "intrinsics carry the calibrated flag"
        );
        assert!(v["imu_window"].is_array(), "imu_window is a bare array");
        // K and R are capitalized per the spec's math convention; t stays lower.
        assert!(v["camera"].get("K").is_some(), "intrinsics key is `K`");
        assert!(v["pose"].get("R").is_some(), "rotation key is `R`");
        assert!(v["pose"].get("t").is_some(), "translation key is `t`");
        // The HEVC encoding is hyphenated on the wire.
        assert_eq!(v["image"]["encoding"], "hevc-keyframe");
        assert_eq!(serde_json::to_value(ImageEncoding::Jpeg).unwrap(), "jpeg");
        assert_eq!(
            serde_json::to_value(ImageEncoding::HevcKeyframe).unwrap(),
            "hevc-keyframe"
        );
    }

    #[test]
    fn occupancy_splat_mesh_pose_round_trip() {
        let occ = OccupancyDescriptor {
            session_id: "sess-1".into(),
            generation: 4,
            origin: [0.0, 0.0, 0.0],
            resolution_m: 0.05,
            dims: [100, 100, 40],
            field: OccupancyField::Esdf,
            truncation_m: 2.0,
            shm_name: Some("atlas-occ-0".into()),
            slot: Some(1),
            seq: Some(7),
            url: None,
        };
        assert_eq!(
            occ,
            OccupancyDescriptor::from_msgpack(&occ.to_msgpack().unwrap()).unwrap()
        );

        let splat = SplatDescriptor {
            session_id: "sess-1".into(),
            generation: 4,
            gaussian_count: 250_000,
            step: 1500,
            url: Some("spz://session/1".into()),
            handle: None,
            manifest_url: Some("https://node.example/artifacts/sess-1/g4/manifest.json".into()),
            lod_levels: 3,
        };
        assert_eq!(
            splat,
            SplatDescriptor::from_msgpack(&splat.to_msgpack().unwrap()).unwrap()
        );

        let mesh = MeshDescriptor {
            session_id: "sess-1".into(),
            generation: 4,
            vertex_count: 8000,
            face_count: 16000,
            url: None,
            handle: Some("mesh-0".into()),
        };
        assert_eq!(
            mesh,
            MeshDescriptor::from_msgpack(&mesh.to_msgpack().unwrap()).unwrap()
        );

        let pose_desc = PoseDescriptor {
            pose: sample_pose(),
            anchor: None,
            ts_ms: 5,
        };
        assert_eq!(
            pose_desc,
            PoseDescriptor::from_msgpack(&pose_desc.to_msgpack().unwrap()).unwrap()
        );
    }

    #[test]
    fn occupancy_field_defaults_to_occupancy_never_esdf() {
        // An older producer's buffer holds occupancy probabilities. Defaulting to
        // Esdf would have a planner read a probability byte as a distance in
        // metres, so the default must fall on the side that cannot be
        // mis-measured.
        let old: OccupancyDescriptor = serde_json::from_str(
            r#"{"origin":[0,0,0],"resolution_m":0.1,"dims":[4,4,4],
                "shm_name":null,"slot":null,"seq":null}"#,
        )
        .unwrap();
        assert_eq!(old.field, OccupancyField::Occupancy);
        assert_eq!(old.truncation_m, 0.0);
        assert_eq!(old.generation, 0);
    }

    #[test]
    fn an_unmeasured_time_alignment_is_never_trustworthy() {
        // The whole point of the sigma field is to admit uncertainty, so the
        // absent/default value must fail closed rather than read as an exact
        // clock.
        let un = TimeAlignment::unmeasured();
        assert!(un.clock_offset_sigma_ns < 0);
        assert!(!un.is_trustworthy(
            KEYFRAME_MAX_OFFSET_SIGMA_NS,
            KEYFRAME_MAX_PAIRING_RESIDUAL_MS
        ));
        assert_eq!(
            TimeAlignment::default(),
            un,
            "Default must be the unmeasured (fail-closed) alignment, not a zeroed one"
        );

        // A measured, in-budget alignment is trustworthy; a NaN residual (a
        // producer bug) is refused rather than compared.
        assert!(sample_time().is_trustworthy(
            KEYFRAME_MAX_OFFSET_SIGMA_NS,
            KEYFRAME_MAX_PAIRING_RESIDUAL_MS
        ));
        let mut nan = sample_time();
        nan.pose_pairing_residual_ms = f64::NAN;
        assert!(!nan.is_trustworthy(
            KEYFRAME_MAX_OFFSET_SIGMA_NS,
            KEYFRAME_MAX_PAIRING_RESIDUAL_MS
        ));
    }

    #[test]
    fn keyframe_validate_refuses_every_untrustworthy_frame() {
        assert!(sample_keyframe().validate().is_ok());

        // No covariance: the reconstructor would have no position-prior sigma.
        let mut no_cov = sample_keyframe();
        no_cov.pose_cov.clear();
        assert_eq!(
            no_cov.validate(),
            Err(KeyframeReject::PoseCovLen { got: 0 })
        );

        // A wrong-length covariance is just as unusable as none.
        let mut short_cov = sample_keyframe();
        short_cov.pose_cov.truncate(9);
        assert_eq!(
            short_cov.validate(),
            Err(KeyframeReject::PoseCovLen { got: 9 })
        );

        // The clock-offset sigma is NOT a default gate: a node with no PPS
        // discipline honestly reports it unmeasured, and rejecting every frame
        // there would yield zero reconstructions with no visible cause. The
        // sigma still travels, and `clock_is_disciplined` reports the fact.
        let mut unmeasured = sample_keyframe();
        unmeasured.time = TimeAlignment::unmeasured();
        assert!(
            unmeasured.validate().is_ok(),
            "an undisciplined clock degrades the dataset's label, not its existence"
        );
        assert!(!unmeasured.clock_is_disciplined());
        assert!(sample_keyframe().clock_is_disciplined());

        // Under the strict budget (a rig that DOES have PPS) it is refused.
        let strict = KeyframeBudget::strict();
        assert!(matches!(
            unmeasured.validate_with(&strict),
            Err(KeyframeReject::ClockOffsetSigma { .. })
        ));
        // A 1 ms sigma passes strict; 50 ms does not (worse than a frame period).
        let mut ok_sigma = sample_keyframe();
        ok_sigma.time.clock_offset_sigma_ns = 1_000_000;
        assert!(ok_sigma.validate_with(&strict).is_ok());
        let mut bad_sigma = sample_keyframe();
        bad_sigma.time.clock_offset_sigma_ns = 50_000_000;
        assert!(matches!(
            bad_sigma.validate_with(&strict),
            Err(KeyframeReject::ClockOffsetSigma { .. })
        ));

        // A pose interpolated too far from the exposure instant.
        let mut skewed = sample_keyframe();
        skewed.time.trigger_seq = None;
        skewed.time.pose_pairing_residual_ms = -120.0;
        assert!(matches!(
            skewed.validate(),
            Err(KeyframeReject::PairingResidual { .. })
        ));

        // No anchor: the frame sits at the local origin while every post-anchor
        // frame sits in ENU metres, which the reconstructor reads as a teleport.
        let mut unanchored = sample_keyframe();
        unanchored.global_anchor = None;
        assert_eq!(unanchored.validate(), Err(KeyframeReject::NoAnchor));
    }

    #[test]
    fn an_older_producers_keyframe_decodes_as_untrustworthy_not_as_perfect() {
        // The compatibility direction that matters: a frame from a build with no
        // pose_cov / time fields must be REFUSED, not silently accepted with a
        // zeroed covariance and a zero-sigma clock.
        let legacy = serde_json::json!({
            "session_id": "old", "kf_id": 0, "ts_unix_ms": 1,
            "camera_id": "front", "camera_role": "primary", "tier": "full",
            "image": {"encoding": "jpeg", "width": 1, "height": 1, "bytes": [0]},
            "camera": {"K": [1.0,0.0,0.0,0.0,1.0,0.0,0.0,0.0,1.0],
                       "distortion": {"model": "radtan", "params": []}},
            "pose": {"R": [1.0,0.0,0.0,0.0,1.0,0.0,0.0,0.0,1.0], "t": [0.0,0.0,0.0], "cov": null},
            "pose_source": "local_vio",
            "global_anchor": {"lat": 0.0, "lon": 0.0, "alt_m": 0.0, "yaw_rad": 0.0},
            "imu_window": [], "flags": {"is_loop_closure": false,
            "is_session_start": true, "is_session_end": false}
        });
        let kf: KeyframeEnvelope = serde_json::from_value(legacy).unwrap();
        assert!(
            !kf.camera.calibrated,
            "an unlabelled camera reads uncalibrated"
        );
        assert_eq!(kf.time, TimeAlignment::unmeasured());
        assert!(kf.pose_cov.is_empty());
        assert!(
            kf.validate().is_err(),
            "a legacy frame must not pass validation on serde defaults"
        );
    }

    #[test]
    fn every_shared_data_topic_names_an_enforced_capability() {
        // The namespace is documented as "any plugin subscribes to", and the
        // host's generic rule grants a plugin only its own plugin.<id>. space,
        // so without this mapping the shared-data topics are reachable by
        // exactly one plugin. Every one must resolve to a capability, and no
        // agent-namespace topic may resolve to any.
        for topic in PLUGIN_ATLAS_TOPICS {
            assert!(
                atlas_topic_subscribe_capability(topic).is_some(),
                "shared-data topic `{topic}` names no capability, so it is either \
                 unreachable or ungated"
            );
        }
        assert_eq!(
            atlas_topic_subscribe_capability(PLUGIN_ATLAS_SPLAT_TOPIC),
            Some(ATLAS_WORLD_READ_CAP)
        );
        assert_eq!(
            atlas_topic_subscribe_capability(PLUGIN_ATLAS_OCCUPANCY_TOPIC),
            Some(ATLAS_WORLD_READ_CAP)
        );
        assert_eq!(
            atlas_topic_subscribe_capability(PLUGIN_ATLAS_POSE_TOPIC),
            Some(ATLAS_POSE_READ_CAP)
        );
        // The agent's own namespace is not shared plugin data: the raw keyframe
        // lane and the capture-state lane are not offered to plugins here.
        for topic in [
            ATLAS_KEYFRAME_TOPIC,
            ATLAS_CAPTURE_STATE_TOPIC,
            ATLAS_POSE_OFFLOAD_TOPIC,
            "plugin.other.thing",
        ] {
            assert_eq!(atlas_topic_subscribe_capability(topic), None);
        }
    }

    #[test]
    fn the_capability_mapping_is_exact_and_cannot_be_inherited_by_a_longer_topic() {
        // A prefix match would turn the gate into a naming convention: a plugin
        // could mint `plugin.atlas.occupancy.<anything>` and be handed
        // `compute.job.read` for it. Every one of these must resolve to None so
        // it falls through to the host's ordinary namespace rule instead.
        for topic in [
            "plugin.atlas.occupancy.evil",
            "plugin.atlas.occupancy.",
            "plugin.atlas.splat/x",
            "plugin.atlas.pose.extra",
            "plugin.atlas.meshy",
            "plugin.atlas.",
            "plugin.atlas",
            "Plugin.Atlas.Occupancy",
            "",
        ] {
            assert_eq!(
                atlas_topic_subscribe_capability(topic),
                None,
                "`{topic}` must not inherit a shared-data capability"
            );
        }
        // And a shortened topic is not a match either.
        assert_eq!(
            atlas_topic_subscribe_capability(&PLUGIN_ATLAS_OCCUPANCY_TOPIC[..20]),
            None
        );
    }

    #[test]
    fn the_wfb_relay_bearer_reports_that_it_cannot_carry_keyframes() {
        // The field topology that has no LAN degrades to pose + status only, and
        // the surface must say so instead of reporting a working bearer.
        assert!(!bearer_carries_keyframes("wfb-relay"));
        assert!(bearer_keyframe_degraded_reason("wfb-relay").is_some());
        for bearer in ["direct-lan", "cloud"] {
            assert!(bearer_carries_keyframes(bearer));
            assert_eq!(bearer_keyframe_degraded_reason(bearer), None);
        }
    }

    #[test]
    fn version_matches_registry() {
        // The constant and the contract registry are the two sources of truth for
        // this contract's version; a drift between them is caught here.
        assert_eq!(
            ATLAS_ENVELOPE_VERSION,
            crate::contracts::contract_version("atlas.envelope").unwrap()
        );
    }

    #[test]
    fn atlas_forward_sidecar_version_matches_registry() {
        // The per-file sidecar constant and the sidecar registry are the two
        // sources of truth for the atlas-forward version; catch a drift here.
        assert_eq!(
            ATLAS_FORWARD_SIDECAR_VERSION,
            crate::contracts::sidecar_version("atlas-forward").unwrap()
        );
    }

    #[test]
    fn atlas_forward_status_defaults_version_to_zero_for_an_old_file() {
        // A file written before the field existed reads back as version 0 (the
        // serde default), so the reader warns best-effort rather than failing.
        let old: AtlasForwardStatus = serde_json::from_str(r#"{"generatedAtMs":5}"#).unwrap();
        assert_eq!(old.version, 0);
        assert_eq!(old.generated_at_ms, 5);
    }

    #[test]
    fn rejects_a_future_envelope_version() {
        // A future producer stamps a higher version; this build must refuse it at
        // decode rather than silently mis-parse the payload (the pose-bug class).
        let future = AtlasEvent {
            v: ATLAS_ENVELOPE_VERSION + 1,
            topic: ATLAS_KEYFRAME_TOPIC.into(),
            device_id: None,
            payload: vec![1, 2, 3],
        };
        let bytes = future.encode().unwrap();
        let err = AtlasEvent::decode(&bytes).unwrap_err();
        assert!(matches!(
            err,
            AtlasError::Version { got, ours }
                if got == ATLAS_ENVELOPE_VERSION + 1 && ours == ATLAS_ENVELOPE_VERSION
        ));
    }
}
