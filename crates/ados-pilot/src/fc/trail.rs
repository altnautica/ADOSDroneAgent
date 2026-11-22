//! The FC's attitude and local position over the last [`TRAIL_MS`], so a
//! measurement taken earlier than it arrives (a camera frame published a
//! few hundred milliseconds after its exposure) is placed from where the
//! vehicle was and how it leaned then.

use std::collections::VecDeque;

use super::{Attitude, LocalPos, FRESH_MS};

/// How far back the trail reaches, ms.
pub const TRAIL_MS: u64 = 1_000;

/// The reports of the last [`TRAIL_MS`], oldest first.
#[derive(Debug, Clone, Default)]
pub struct Trail {
    att: VecDeque<(u64, Attitude)>,
    local: VecDeque<(u64, LocalPos)>,
}

/// Append `v` at `at_ms`, dropping what is older than the trail; a clock
/// that stepped back starts the trail over.
fn push<T>(q: &mut VecDeque<(u64, T)>, at_ms: u64, v: T) {
    if q.back().is_some_and(|b| b.0 > at_ms) {
        q.clear();
    }
    q.push_back((at_ms, v));
    while q.front().is_some_and(|f| f.0 + TRAIL_MS < at_ms) {
        q.pop_front();
    }
}

/// The newest report at or before `t_ms`, as long as it was fresh then
/// (younger than [`FRESH_MS`], as the FC's live reads are); `None` when the
/// trail starts after it or the FC had gone quiet.
fn at<T: Copy>(q: &VecDeque<(u64, T)>, t_ms: u64) -> Option<T> {
    let i = q.partition_point(|x| x.0 <= t_ms);
    let (at_ms, v) = q[i.checked_sub(1)?];
    (t_ms - at_ms < FRESH_MS).then_some(v)
}

impl Trail {
    pub fn on_attitude(&mut self, at_ms: u64, v: Attitude) {
        push(&mut self.att, at_ms, v);
    }

    pub fn on_local(&mut self, at_ms: u64, v: LocalPos) {
        push(&mut self.local, at_ms, v);
    }

    /// The attitude the FC reported last at or before `t_ms`.
    pub fn attitude_at(&self, t_ms: u64) -> Option<Attitude> {
        at(&self.att, t_ms)
    }

    /// The local position the FC reported last at or before `t_ms`.
    pub fn local_at(&self, t_ms: u64) -> Option<LocalPos> {
        at(&self.local, t_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_inside_the_trail_reads_the_report_then() {
        let mut t = Trail::default();
        for k in 0..100u64 {
            let roll = k as f32;
            t.on_attitude(
                1000 + 20 * k,
                Attitude {
                    roll,
                    ..Attitude::default()
                },
            );
        }
        // The newest at or before: 2500 ms is report 75.
        assert_eq!(t.attitude_at(2510).map(|a| a.roll), Some(75.0));
        // Older than the trail, or before any report: none.
        assert_eq!(t.attitude_at(1500), None);
        assert_eq!(t.local_at(2900), None);
        // A report the FC has not followed for FRESH_MS reads as none.
        assert_eq!(t.attitude_at(2980 + FRESH_MS), None);
        // A clock stepped back starts over.
        t.on_attitude(10, Attitude::default());
        assert_eq!(t.attitude_at(500).map(|a| a.roll), Some(0.0));
        assert_eq!(t.attitude_at(5), None);
    }
}
