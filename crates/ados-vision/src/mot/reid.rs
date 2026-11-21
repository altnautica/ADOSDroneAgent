//! Re-identification of the designated subject.
//!
//! While the subject's track is measured cleanly (a confident detection that
//! no other box overlaps), its descriptor trains an online classifier as a
//! positive and updates a slow appearance template; every other confirmed
//! track measured on the same frame trains it as a negative. The model uses no
//! learned weights from outside: the descriptor is computed in this crate
//! ([`super::describe`]).
//!
//! When the subject's track is lost or removed, a track born after the subject
//! was last seen is a candidate. Each candidate's running descriptor is scored
//! as the mean of the classifier's probability and its cosine similarity to the
//! template. The best candidate takes the subject's id only when it scores at
//! least [`MotConfig::reid_min_score`] and leads every other measured track
//! (other candidates, and people known not to be the subject because they were
//! seen beside it) by [`MotConfig::reid_min_margin`]. Two people the model
//! cannot tell apart therefore never re-lock: no lock is safer than a wrong one.

use ados_protocol::framebus::TrackState;

use super::appearance::{blend, similarity, Classifier, Features};
use super::{MotConfig, MultiObjectTracker};

/// Least IoU with another measured box that makes a view of the subject
/// unclean (partly hidden or merged), so it is not learned from.
const CLEAN_VIEW_MAX_IOU: f32 = 0.1;

/// The designated subject's appearance model.
#[derive(Debug, Clone)]
pub(super) struct Subject {
    /// The track id the subject currently holds.
    pub id: u32,
    pub appearance_id: u32,
    pub class: u16,
    pub template: Features,
    pub positives: u32,
    pub classifier: Classifier,
    pub last_seen_s: f64,
}

impl Subject {
    pub fn new(id: u32, appearance_id: u32, class: u16, t: f64) -> Self {
        Self {
            id,
            appearance_id,
            class,
            template: [0.0; super::FEAT_DIM],
            positives: 0,
            classifier: Classifier::default(),
            last_seen_s: t,
        }
    }

    /// Fold a clean positive view into the template.
    pub fn add_template(&mut self, f: &Features, rate: f32) {
        if self.positives == 0 {
            self.template = *f;
        } else {
            blend(&mut self.template, f, rate);
        }
        self.positives += 1;
    }

    fn score(&self, f: &Features) -> f32 {
        0.5 * self.classifier.score(f) + 0.5 * similarity(&self.template, f)
    }
}

impl MultiObjectTracker {
    /// The designated subject's appearance identity, when `track_id` is the
    /// id the subject currently holds.
    pub fn appearance_id(&self, track_id: u32) -> Option<u32> {
        self.subject
            .as_ref()
            .filter(|s| track_id != 0 && s.id == track_id)
            .map(|s| s.appearance_id)
    }

    /// The designated subject's current track id, while it has an appearance
    /// model.
    pub fn designated_id(&self) -> Option<u32> {
        self.subject.as_ref().map(|s| s.id)
    }

    /// Train the subject's model on this frame's measured tracks.
    pub(super) fn learn(&mut self, features: &[Option<Features>]) {
        let cfg: MotConfig = self.cfg;
        let Some(subject) = self.subject.as_mut() else {
            return;
        };
        let Some(si) = self
            .tracks
            .iter()
            .position(|t| t.id == subject.id && !t.removed && t.det.is_some())
        else {
            return;
        };
        let now = self.tracks[si].last_seen_s;
        subject.last_seen_s = now;
        let sdi = self.tracks[si].det.unwrap_or_default();
        let sbox = self.dets[sdi].bbox;
        let clean = self.dets[sdi].score >= cfg.high_score
            && self.tracks.iter().enumerate().all(|(ti, t)| {
                ti == si
                    || t.det
                        .is_none_or(|d| self.dets[d].bbox.iou(&sbox) < CLEAN_VIEW_MAX_IOU)
            });
        let Some(Some(pos)) = features.get(sdi) else {
            return;
        };
        if !clean {
            return;
        }
        subject.add_template(pos, cfg.reid_template_rate);
        subject
            .classifier
            .train(pos, true, cfg.reid_learning_rate, cfg.reid_l2);
        for (ti, t) in self.tracks.iter().enumerate() {
            if ti == si || t.state != TrackState::Confirmed {
                continue;
            }
            if let Some(Some(neg)) = t.det.and_then(|d| features.get(d)) {
                subject
                    .classifier
                    .train(neg, false, cfg.reid_learning_rate, cfg.reid_l2);
            }
        }
    }

    /// Give the subject's id back to a reappeared track, when one stands out.
    pub(super) fn reidentify(&mut self, t: f64) {
        let cfg = self.cfg;
        let Some(subject) = self.subject.as_ref() else {
            return;
        };
        if t - subject.last_seen_s > f64::from(cfg.reid_window_s) {
            self.subject = None;
            return;
        }
        let held = self
            .tracks
            .iter()
            .any(|tr| tr.id == subject.id && !tr.removed && tr.det.is_some());
        if held || subject.positives == 0 {
            return;
        }
        let mut best: Option<(usize, f32)> = None;
        let mut runner_up = 0f32;
        for (ti, tr) in self.tracks.iter().enumerate() {
            if tr.removed || tr.det.is_none() || tr.feat_n == 0 || tr.id == subject.id {
                continue;
            }
            let s = subject.score(&tr.feat);
            let candidate = tr.class == subject.class
                && tr.born_s > subject.last_seen_s
                && tr.feat_n >= cfg.reid_min_obs;
            match best {
                Some((_, b)) if candidate && s > b => {
                    runner_up = runner_up.max(b);
                    best = Some((ti, s));
                }
                None if candidate => best = Some((ti, s)),
                _ => runner_up = runner_up.max(s),
            }
        }
        let Some((ti, s)) = best else {
            return;
        };
        if s < cfg.reid_min_score || s - runner_up < cfg.reid_min_margin {
            return;
        }
        let id = subject.id;
        for tr in &mut self.tracks {
            if tr.id == id {
                tr.removed = true;
            }
        }
        let tr = &mut self.tracks[ti];
        tr.id = id;
        tr.state = TrackState::Confirmed;
        if let Some(s) = self.subject.as_mut() {
            s.last_seen_s = t;
        }
    }
}
