//! The scans' own closing speed toward one world sector: the range rate of its
//! nearest return, trusted only while that return behaves like one surface.

/// Scans a sector's closing speed is fitted over: 0.4 s of a 10 Hz planar
/// lidar, 0.8 s of a 5 Hz one.
pub const CLOSING_SCANS: usize = 5;
/// Fewest consecutive scans with a return that measure a closing speed.
pub const CLOSING_MIN_SCANS: usize = 4;
/// Largest RMS distance, metres, of a sector's nearest ranges from the line
/// fitted through them over [`CLOSING_TUNED_SPAN_S`] for the line's slope to
/// count as a closing speed. A surface the vehicle moves toward (a wall, a
/// column, a trunk) stays well inside it: the lidar is good to ±30 mm 3σ, and
/// braking at 3 m/s² bends 0.4 s of ranges by about 2.5 cm RMS. Leaf returns in a
/// crown, a beam grazing an edge, rain or dust land at a new depth every scan
/// (0.5-2.5 m apart), lie on no line, and measure nothing.
pub const CLOSING_FIT_RMS_M: f32 = 0.08;
/// Time the fitted scans span at which [`CLOSING_FIT_RMS_M`] holds: five
/// scans of a 10 Hz lidar.
pub const CLOSING_TUNED_SPAN_S: f32 = 0.4;
/// Steadiest change of closing speed, m/s², the fit allows for on top of
/// [`CLOSING_FIT_RMS_M`] when its scans span longer than
/// [`CLOSING_TUNED_SPAN_S`] (a slower lidar): above the brake's own 3 m/s²,
/// so a braking approach at 5 Hz still measures.
pub const CLOSING_FIT_ACCEL_MPS2: f32 = 4.0;
/// RMS distance, metres, of a parabola from the line fitted through five
/// even samples of it, per m/s² and per s² of span.
const BEND_RMS_PER_ACCEL_SPAN2: f32 = 0.0523;
/// Fastest closing speed a fit may measure, m/s: above the fastest
/// multirotor profile (14 m/s). A sector whose nearest range falls faster
/// than this has a new nearest surface (a post passing in front of a far
/// wall, a dropout recovering), not motion, and measures nothing.
pub const SCAN_CLOSING_MAX_MPS: f32 = 16.0;
/// Range noise, metres, allowed when a return continues a track from the
/// track of a nearby sector (one scan of [`SCAN_CLOSING_MAX_MPS`] plus this nearer,
/// or this further).
pub const HANDOVER_SLACK_M: f32 = 0.1;
/// Scans a sector keeps its last fitted closing speed, undecayed, while its
/// track is too short to fit: the surface left the sector or just entered
/// it. A fit that fails its residual drops it at once.
pub const CLOSING_HOLD_SCANS: u8 = 3;

use super::{sector_of, HANDOVER_SECTORS, SCAN_MAX_AGE_MS};

/// The last [`CLOSING_SCANS`] nearest ranges of one sector, oldest first,
/// and the closing speed they last measured.
#[derive(Debug, Clone, Copy)]
pub(super) struct RangeTrack {
    ms: [u64; CLOSING_SCANS],
    range_m: [f32; CLOSING_SCANS],
    len: usize,
    /// The last fit's closing speed, m/s.
    held: f32,
    /// Scans since that fit.
    held_scans: u8,
}

impl Default for RangeTrack {
    fn default() -> Self {
        Self {
            ms: [0; CLOSING_SCANS],
            range_m: [0.0; CLOSING_SCANS],
            len: 0,
            held: 0.0,
            held_scans: CLOSING_HOLD_SCANS,
        }
    }
}

impl RangeTrack {
    /// The newest tracked scan: (ms, range).
    pub(super) fn last(&self) -> Option<(u64, f32)> {
        (self.len > 0).then(|| (self.ms[self.len - 1], self.range_m[self.len - 1]))
    }

    /// Whether a return `range_m` at `ms` can be the surface this track last
    /// saw at `(t, r)`: it closed no faster than [`SCAN_CLOSING_MAX_MPS`],
    /// both within [`HANDOVER_SLACK_M`].
    pub(super) fn continues(&self, ms: u64, range_m: f32) -> Option<f32> {
        let (t, r) = self.last()?;
        let dt = ms.checked_sub(t)? as f32 / 1000.0;
        let fall = r - range_m;
        (-HANDOVER_SLACK_M..=SCAN_CLOSING_MAX_MPS * dt + HANDOVER_SLACK_M)
            .contains(&fall)
            .then_some(fall.abs())
    }

    /// Take over `other`'s ranges (a surface that moved here from the
    /// sector nearby), keeping this sector's own held closing speed.
    pub(super) fn adopt(&mut self, other: &RangeTrack) {
        self.ms = other.ms;
        self.range_m = other.range_m;
        self.len = other.len;
    }

    /// Whether the track has no ranges (the last scan had no return here).
    pub(super) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Add one scan's nearest range (infinite: no return, which ends the
    /// track: the next return may be another surface) and return the
    /// sector's closing speed, m/s ≥ 0: the fit's ([`Self::fit`]) when the
    /// track is long enough, else the last fit's for up to
    /// [`CLOSING_HOLD_SCANS`] scans, else zero.
    pub(super) fn push(&mut self, ms: u64, range_m: f32) -> f32 {
        if !range_m.is_finite() {
            self.len = 0;
        } else {
            if self.len == CLOSING_SCANS {
                self.ms.copy_within(1.., 0);
                self.range_m.copy_within(1.., 0);
                self.len -= 1;
            }
            self.ms[self.len] = ms;
            self.range_m[self.len] = range_m;
            self.len += 1;
        }
        if let Some(c) = self.fit() {
            self.held = c;
            self.held_scans = 0;
            return c;
        }
        if self.held_scans < CLOSING_HOLD_SCANS {
            self.held_scans += 1;
            return self.held;
        }
        self.held = 0.0;
        0.0
    }

    /// Forget the ranges and the held closing speed (a gap in the scans).
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Closing speed, m/s (≥ 0): minus the slope of the least-squares line
    /// through the tracked ranges, when they lie on it
    /// ([`CLOSING_FIT_RMS_M`]), zero when they do not; `None` while fewer
    /// than [`CLOSING_MIN_SCANS`] are tracked.
    fn fit(&self) -> Option<f32> {
        let n = self.len;
        if n < CLOSING_MIN_SCANS {
            return None;
        }
        let newest = self.ms[n - 1];
        let t = |i: usize| -(newest.saturating_sub(self.ms[i]) as f32) / 1000.0;
        let r = &self.range_m[..n];
        let t_mean = (0..n).map(t).sum::<f32>() / n as f32;
        let r_mean = r.iter().sum::<f32>() / n as f32;
        let (mut sxx, mut sxy) = (0.0f32, 0.0f32);
        for (i, ri) in r.iter().enumerate() {
            let dt = t(i) - t_mean;
            sxx += dt * dt;
            sxy += dt * (ri - r_mean);
        }
        if sxx < 1e-6 {
            return Some(0.0);
        }
        let slope = sxy / sxx;
        let ssr: f32 = r
            .iter()
            .enumerate()
            .map(|(i, ri)| (ri - r_mean - slope * (t(i) - t_mean)).powi(2))
            .sum();
        let span = -t(0);
        let bend = BEND_RMS_PER_ACCEL_SPAN2
            * CLOSING_FIT_ACCEL_MPS2
            * (span * span - CLOSING_TUNED_SPAN_S * CLOSING_TUNED_SPAN_S).max(0.0);
        let rms = CLOSING_FIT_RMS_M + bend;
        let closing = -slope;
        if ssr > n as f32 * rms * rms || closing > SCAN_CLOSING_MAX_MPS {
            return Some(0.0);
        }
        Some(closing.max(0.0))
    }
}

impl super::Obstacles {
    /// Each world sector's closing speed after this scan; a gap over
    /// [`SCAN_MAX_AGE_MS`] since the last scan starts every sector afresh. A
    /// return in a sector that had none last scan continues the track of a
    /// sector up to [`HANDOVER_SECTORS`] away whose last range it follows
    /// ([`RangeTrack::continues`]): an obstacle off the flight line
    /// sweeps across sectors as the vehicle passes it, and its closing speed
    /// must not restart at each one. Each scan is timed by its own capture
    /// stamp when it carries one, else by its arrival, so delivery jitter
    /// cannot scale a range rate; a scan that changes between the two
    /// clocks starts every sector afresh. A closing speed faster than
    /// `closing_max_mps` counts as that bound.
    pub(super) fn update_closing(
        &mut self,
        yaw: f32,
        stamp_ns: Option<u64>,
        closing_max_mps: f32,
        now_ms: u64,
    ) {
        const N: usize = ados_protocol::pilot::SECTORS;
        let mut nearest = [f32::INFINITY; N];
        for h in &self.hits {
            let s = sector_of(yaw + h.body_bearing, N);
            nearest[s] = nearest[s].min(h.range_m);
        }
        let arrival_gap = self
            .scan_ms
            .is_none_or(|t| now_ms.saturating_sub(t) > SCAN_MAX_AGE_MS);
        let stamp_gap = match (stamp_ns, self.scan_stamp_ns) {
            (Some(s), Some(p)) => s <= p || (s - p) / 1_000_000 > SCAN_MAX_AGE_MS,
            (None, None) => false,
            _ => true,
        };
        let gap = arrival_gap || stamp_gap;
        let t_ms = stamp_ns.map_or(now_ms, |s| s / 1_000_000);
        if self.tracks.len() != N {
            self.tracks = vec![RangeTrack::default(); N];
            self.closing = vec![0.0; N];
        }
        let tracks = &self.tracks;
        let handover: [Option<RangeTrack>; N] = std::array::from_fn(|s| {
            if gap || !nearest[s].is_finite() || !tracks[s].is_empty() {
                return None;
            }
            (1..=HANDOVER_SECTORS)
                .flat_map(|k| [(s + N - k) % N, (s + k) % N])
                .filter_map(|j| Some((tracks[j].continues(t_ms, nearest[s])?, j)))
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, j)| tracks[j])
        });
        for (((track, c), r), from) in self
            .tracks
            .iter_mut()
            .zip(&mut self.closing)
            .zip(nearest)
            .zip(&handover)
        {
            if gap {
                track.clear();
            }
            if let Some(from) = from {
                track.adopt(from);
            }
            // `min` (not `clamp`): a NaN bound keeps the measured rate.
            *c = track.push(t_ms, r).min(closing_max_mps);
        }
    }
}
