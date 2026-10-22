//! Appearance descriptors and the candidate type the tracker associates over.

use ados_protocol::framebus::{BoundingBox, Detection};

/// Length of the model-free appearance descriptor the interim
/// [`HistogramAppearance`] produces. Small and fixed so it is cheap to compare
/// and carry; a learned embedding would pick its own length behind the trait.
pub const APPEARANCE_DIM: usize = 16;

/// An appearance descriptor for one candidate box.
///
/// This is a normalized feature vector: the model-free interim builds it from
/// coarse colour/gradient statistics of the box pixels; a learned re-id model
/// would emit an L2-normalized embedding of the same shape. It is compared with
/// [`AppearanceModel::similarity`], not by raw equality, so two descriptors of
/// the *same* object (under jitter / lighting change) score high and two
/// *different* objects score low.
#[derive(Debug, Clone, PartialEq)]
pub struct Appearance {
    /// The feature vector. Length is model-defined; the interim uses
    /// [`APPEARANCE_DIM`].
    pub features: Vec<f32>,
}

impl Appearance {
    /// Build from a raw feature slice (the harness and a real extractor both use
    /// this). The vector is stored as-is; [`AppearanceModel::similarity`] owns
    /// any normalization.
    pub fn from_features(features: Vec<f32>) -> Self {
        Self { features }
    }
}

/// Pluggable appearance backend.
///
/// `extract` turns a candidate box (its pixels, in a real impl) into an
/// [`Appearance`]; `similarity` scores two descriptors in `0..=1` (1 = identical
/// appearance, 0 = unrelated). The tracker depends only on this trait, so the
/// model-free [`HistogramAppearance`] shipped here can be swapped for a learned
/// re-id / siamese embedding with no tracker change: the new model
/// just produces longer descriptors and a learned similarity.
///
/// The default `extract` returns `None` (no pixels available, the pure /
/// synthetic path), so a tracker can run appearance-aware on descriptors handed
/// to it directly (the harness) while a deployment plugs in a real extractor.
pub trait AppearanceModel: Send + Sync {
    /// Extract a descriptor for the box `bbox` from the frame `pixels`. Returns
    /// `None` when no frame is available (the model-free / synthetic path), in
    /// which case the caller must supply the descriptor on the [`Candidate`].
    fn extract(&self, _pixels: Option<&[u8]>, _bbox: &BoundingBox) -> Option<Appearance> {
        None
    }

    /// Similarity of two descriptors in `0..=1`. Higher = more alike.
    fn similarity(&self, a: &Appearance, b: &Appearance) -> f32;
}

/// The model-free interim appearance backend: cosine similarity over the
/// descriptor vectors.
///
/// It does NOT extract from pixels (a real deployment swaps in a re-id model for
/// that); it scores descriptors the caller already holds. Cosine
/// similarity is scale-invariant and maps cleanly onto `0..=1` after clamping,
/// so a colour/gradient histogram now and a learned embedding later both work
/// through the same `similarity`. Two equal vectors score 1.0; orthogonal ones
/// score 0.0.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistogramAppearance;

impl AppearanceModel for HistogramAppearance {
    fn similarity(&self, a: &Appearance, b: &Appearance) -> f32 {
        cosine_similarity(&a.features, &b.features)
    }
}

/// Cosine similarity clamped to `0..=1` (negative correlation reads as "no
/// match", not as anti-match). A zero-magnitude vector yields 0.
pub(super) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())).clamp(0.0, 1.0)
}

/// One candidate detection for a frame, optionally carrying its appearance
/// descriptor.
///
/// The motion-only [`SingleObjectTracker::update`](super::SingleObjectTracker::update) path wraps each wire
/// [`Detection`] with `appearance: None`; the appearance-aware
/// [`SingleObjectTracker::update_with_appearance`](super::SingleObjectTracker::update_with_appearance) path carries a descriptor per
/// candidate (extracted from pixels in a real deployment, supplied directly by
/// the synthetic harness). The descriptor never reaches the wire, only the
/// resulting lock-state / association confidence does.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// The detection as it would appear on the wire (no appearance field).
    pub detection: Detection,
    /// Appearance descriptor for this box, when available.
    pub appearance: Option<Appearance>,
}

impl Candidate {
    /// A candidate with no appearance descriptor (motion-only path).
    pub fn motion_only(detection: Detection) -> Self {
        Self {
            detection,
            appearance: None,
        }
    }

    /// A candidate carrying its appearance descriptor.
    pub fn with_appearance(detection: Detection, appearance: Appearance) -> Self {
        Self {
            detection,
            appearance: Some(appearance),
        }
    }
}
