//! Which requested streams are arriving, one message id at a time.
//!
//! The stream cadence re-sends its whole table on a relaxing interval (up to
//! a minute on a healthy link) and fast only when the vehicle streams nothing
//! at all. A single lost `SET_MESSAGE_INTERVAL` (one stream missing while the
//! others flow) was therefore left missing until the next relaxed refresh.
//! This watch records when each requested id last arrived and when it was
//! last asked for, so the cadence re-asks for just the missing ids every
//! [`STREAM_REASK`] while the rest of the link flows. An id asked for
//! [`STREAM_UNANSWERED_MAX`] times in a row without arriving is given up as
//! unsupported (the FC lacks the sensor or the message): it is not re-asked
//! on its own again, only with the table's periodic refresh, and the router
//! status lists it (`fc_streams_unsupported`). Its first frame clears that.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a requested stream may be absent before it counts as missing:
/// this, or three of its periods when they are longer.
pub(crate) const STREAM_ABSENT: Duration = Duration::from_secs(5);
/// The shortest spacing between two asks for one missing id.
pub(crate) const STREAM_REASK: Duration = Duration::from_secs(10);
/// Asks in a row an id may go unanswered (while the link flows) before it is
/// given up as unsupported.
pub(crate) const STREAM_UNANSWERED_MAX: u8 = 3;

#[derive(Debug, Clone, Copy)]
struct Watch {
    /// The requested period.
    period: Duration,
    seen: Option<Instant>,
    asked: Option<Instant>,
    /// Asks in a row, made while the link flowed, that the id did not answer.
    unanswered: u8,
}

impl Watch {
    /// Ask for the id at `now`. An earlier ask it never answered counts
    /// against it when the link was flowing (a stalled link answers nothing,
    /// whatever the FC supports).
    fn ask(&mut self, now: Instant, flowing: bool) {
        let ignored = self.asked.is_some_and(|a| self.seen.is_none_or(|s| s < a));
        if flowing && ignored {
            self.unanswered = self.unanswered.saturating_add(1);
        }
        self.asked = Some(now);
    }

    fn given_up(&self) -> bool {
        self.unanswered >= STREAM_UNANSWERED_MAX
    }
}

/// The per-id arrival watch over the requested stream table.
#[derive(Debug, Default)]
pub struct StreamWatch {
    ids: Mutex<BTreeMap<u32, Watch>>,
}

impl StreamWatch {
    fn ids(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, Watch>> {
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The whole table `(id, Hz)` was requested at `now`; ids it no longer
    /// holds are forgotten. `flowing`: the vehicle was streaming, so an ask
    /// an id left unanswered counts against it.
    pub(crate) fn requested(&self, table: &[(u32, f32)], now: Instant, flowing: bool) {
        let mut ids = self.ids();
        ids.retain(|id, _| table.iter().any(|&(t, _)| t == *id));
        for &(id, hz) in table {
            let period = Duration::from_secs_f32(1.0 / hz.max(0.001));
            let w = ids.entry(id).or_insert(Watch {
                period,
                seen: None,
                asked: None,
                unanswered: 0,
            });
            w.period = period;
            w.ask(now, flowing);
        }
    }

    /// A frame of `id` arrived at `now`.
    pub(crate) fn seen(&self, id: u32, now: Instant) {
        if let Some(w) = self.ids().get_mut(&id) {
            w.seen = Some(now);
            w.unanswered = 0;
        }
    }

    /// The ids to ask for again at `now` while the link flows: absent past
    /// their grace, not asked for [`STREAM_REASK`], not given up. Each
    /// returned id is recorded as asked.
    pub(crate) fn reask(&self, now: Instant) -> Vec<u32> {
        let mut out = Vec::new();
        for (id, w) in self.ids().iter_mut() {
            if w.given_up() {
                continue;
            }
            let grace = STREAM_ABSENT.max(w.period * 3);
            let last = w.seen.max(w.asked);
            let absent = last.is_none_or(|t| now.saturating_duration_since(t) >= grace);
            let quiet = w
                .asked
                .is_none_or(|a| now.saturating_duration_since(a) >= STREAM_REASK);
            if absent && quiet {
                w.ask(now, true);
                out.push(*id);
            }
        }
        out
    }

    /// Requested ids given up as unsupported, ascending.
    pub fn unsupported(&self) -> Vec<u32> {
        self.ids()
            .iter()
            .filter(|(_, w)| w.given_up())
            .map(|(id, _)| *id)
            .collect()
    }

    /// The link dropped: the next link starts the watch afresh.
    pub(crate) fn reset(&self) {
        self.ids().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_missing_stream_is_reasked_every_ten_seconds_then_given_up() {
        let w = StreamWatch::default();
        let t0 = Instant::now();
        w.requested(&[(30, 50.0), (1, 2.0)], t0, true);
        // SYS_STATUS keeps arriving; ATTITUDE's request was lost.
        let at = |s: u32| {
            w.seen(1, t0 + s * S);
            w.reask(t0 + s * S)
        };
        assert!(at(9).is_empty(), "not before the re-ask interval");
        assert_eq!(at(10), vec![30], "only the missing id");
        assert!(at(15).is_empty());
        assert_eq!(at(20), vec![30]);
        assert_eq!(at(30), vec![30]);
        assert_eq!(w.unsupported(), vec![30], "three unanswered asks");
        assert!(at(60).is_empty(), "given up: no more asks of its own");
        w.seen(30, t0 + 61 * S);
        assert!(w.unsupported().is_empty(), "a frame clears the verdict");
    }

    #[test]
    fn an_arriving_stream_is_never_reasked() {
        let w = StreamWatch::default();
        let t0 = Instant::now();
        w.requested(&[(30, 50.0)], t0, true);
        for s in 1..60 {
            w.seen(30, t0 + s * S);
            assert!(w.reask(t0 + s * S).is_empty());
        }
    }

    #[test]
    fn asks_on_a_stalled_link_do_not_count_against_an_id() {
        let w = StreamWatch::default();
        let t0 = Instant::now();
        for k in 0..6 {
            w.requested(&[(30, 50.0)], t0 + k * 10 * S, false);
        }
        assert!(w.unsupported().is_empty());
    }

    #[test]
    fn a_slow_stream_gets_three_periods() {
        let w = StreamWatch::default();
        let t0 = Instant::now();
        w.requested(&[(242, 0.2)], t0, true);
        assert!(w.reask(t0 + 12 * S).is_empty(), "within three 5 s periods");
        assert_eq!(w.reask(t0 + 15 * S), vec![242]);
    }
}
