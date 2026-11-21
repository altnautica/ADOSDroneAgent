//! Operator designation and outside corrections of track identity.

use ados_protocol::framebus::{BoundingBox, TrackState};

use super::kalman::Box4;
use super::reid::Subject;
use super::track::Track;
use super::MultiObjectTracker;

/// Least IoU between a designated box and a track's last observed box for the
/// designation to land on that track rather than start a new one.
const DESIGNATE_MIN_IOU: f32 = 0.3;

fn corner_box(b: &BoundingBox) -> Box4 {
    Box4 {
        x1: b.x,
        y1: b.y,
        x2: b.x + b.width,
        y2: b.y + b.height,
    }
}

impl MultiObjectTracker {
    /// Operator designation of the object in `bbox` (a box from a recent
    /// frame, of class `class_label`). The track that box belongs to, or a new
    /// confirmed track when none does, becomes the designated subject under a
    /// fresh id, which is returned: a designation always starts a new identity
    /// for consumers. Designating the subject's own track again keeps its
    /// appearance model; any other track starts a new one.
    pub fn designate(&mut self, bbox: &BoundingBox, class_label: &str) -> u32 {
        let b = corner_box(bbox);
        let t = self.now_s.unwrap_or(0.0);
        let found = self
            .tracks
            .iter()
            .enumerate()
            .map(|(i, tr)| (i, tr.last_obs.iou(&b)))
            .filter(|&(_, iou)| iou >= DESIGNATE_MIN_IOU)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i);
        let fresh = self.mint_id();
        let ti = match found {
            Some(i) => i,
            None => {
                let class = self.class_index(class_label);
                self.tracks.push(Track::new(b, class, t, &self.cfg));
                self.tracks.len() - 1
            }
        };
        let tr = &mut self.tracks[ti];
        let was_subject = tr.id != 0 && self.subject.as_ref().is_some_and(|s| s.id == tr.id);
        tr.id = fresh;
        tr.state = TrackState::Confirmed;
        tr.hits = tr.hits.max(self.cfg.birth_hits);
        let (class, feat, feat_n) = (tr.class, tr.feat, tr.feat_n);
        match self.subject.as_mut() {
            Some(s) if was_subject => s.id = fresh,
            _ => {
                let mut s = Subject::new(fresh, self.next_appearance_id, class, t);
                self.next_appearance_id += 1;
                if feat_n > 0 {
                    s.add_template(&feat, self.cfg.reid_template_rate);
                }
                self.subject = Some(s);
            }
        }
        fresh
    }

    /// Forget the designated subject (its appearance model too). Tracks keep
    /// their ids.
    pub fn clear_designation(&mut self) {
        self.subject = None;
    }

    /// An outside authority (a single-subject lock) holds that the detection
    /// at `index` of the last update is the object with track id `id`. Makes
    /// it so: that detection's track takes `id`, and the track that held `id`
    /// takes the other one's id (a fresh one when that was none or the
    /// subject's), so ids stay unique; a detection no track took gets a new
    /// confirmed track. The designated subject ends on this detection's track
    /// when either the track or `id` was the subject's.
    pub fn bind(&mut self, index: usize, id: u32) {
        let Some(ti) = self.track_at(index) else {
            return;
        };
        let old = self.tracks[ti].id;
        if old != id {
            let was_subject = old != 0 && self.subject.as_ref().is_some_and(|s| s.id == old);
            if let Some(hi) = self.tracks.iter().position(|t| t.id == id) {
                self.tracks[hi].id = if old != 0 && !was_subject {
                    old
                } else {
                    self.mint_id()
                };
            }
            self.tracks[ti].id = id;
            if let (true, Some(s)) = (was_subject, self.subject.as_mut()) {
                s.id = id;
            }
        }
        self.promote(ti);
    }

    /// Confirm the track of the detection at `index` of the last update (or
    /// start a confirmed one) and return its id. For an outside authority that
    /// trusts the object sooner than [`super::MotConfig::birth_hits`].
    pub fn confirm_at(&mut self, index: usize) -> Option<u32> {
        let ti = self.track_at(index)?;
        if self.tracks[ti].id == 0 {
            self.tracks[ti].id = self.mint_id();
        }
        self.promote(ti);
        Some(self.tracks[ti].id)
    }

    fn promote(&mut self, ti: usize) {
        let birth = self.cfg.birth_hits;
        let tr = &mut self.tracks[ti];
        if tr.state == TrackState::Tentative {
            tr.state = TrackState::Confirmed;
            tr.hits = tr.hits.max(birth);
        }
    }

    /// The track index of the detection at `index`, starting a track for an
    /// untaken detection with a box.
    fn track_at(&mut self, index: usize) -> Option<usize> {
        if let Some(Some(ti)) = self.assigned.get(index) {
            return Some(*ti);
        }
        let d = *self.dets.get(index)?;
        if d.bbox.height() <= 0.0 {
            return None;
        }
        let t = self.now_s.unwrap_or(0.0);
        let mut tr = Track::new(d.bbox, d.class, t, &self.cfg);
        tr.det = Some(index);
        self.tracks.push(tr);
        let ti = self.tracks.len() - 1;
        self.assigned[index] = Some(ti);
        Some(ti)
    }
}
