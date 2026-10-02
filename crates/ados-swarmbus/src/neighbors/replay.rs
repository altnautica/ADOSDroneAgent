//! Per-slot sender tracking: the replay window and the second-sender check.
//!
//! An authenticated beacon proves a fleet member sealed it, not that it was sealed
//! just now. Without this, a captured frame re-injected later opens cleanly, lands
//! in the table stamped as just received, and every consumer dead-reckons a
//! position the aircraft left long ago.
//!
//! Every sender seals under a nonce of `prefix || counter`: the prefix is drawn at
//! random once per sender run, and the counter advances on every frame. So a slot's
//! legitimate traffic is one prefix with a strictly rising counter, and a sender
//! restart shows up as a new prefix. That gives three checks:
//!
//! - **Same prefix, counter not above the last accepted one**: a replay.
//! - **A prefix this slot has already moved on from**: a replay of an earlier run.
//!   A restart always draws a fresh prefix, so a retired one never legitimately
//!   comes back.
//! - **A new prefix while the slot's entry is still live**: a second sender on the
//!   slot. A restart that fast is not possible (the entry only goes stale after
//!   [`super::NEIGHBOR_STALE`] of silence), so this is two nodes provisioned with one
//!   slot, and the first one heard keeps it.
//!
//! The marks outlive the process. A receiver that forgot them on restart would
//! read every slot as never heard, accept a frame captured during an earlier run
//! as fresh, and then refuse the real sender as a second sender for as long as the
//! replay kept the slot live. So the marks are written to [`REPLAY_STATE_PATH`]
//! (at most once a second, and once more on a clean stop) and read back before
//! the bus accepts its first frame.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::crypto::{SenderNonce, NONCE_PREFIX_LEN};

/// How many superseded runs a slot remembers. A slot restarting more than this
/// often within the remembered history is already a fault in its own right.
pub const RETIRED_PREFIXES: usize = 8;

/// Where the per-slot marks persist across restarts.
pub const REPLAY_STATE_PATH: &str = "/var/lib/ados/swarmbus-replay.json";

/// Schema version of the persisted marks.
const REPLAY_STATE_VERSION: u16 = 1;

/// One slot's marks as written to disk.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedSlot {
    slot: u8,
    prefix: [u8; NONCE_PREFIX_LEN],
    counter: u32,
    retired: Vec<[u8; NONCE_PREFIX_LEN]>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedMarks {
    version: u16,
    slots: Vec<PersistedSlot>,
}

/// What the replay window says about one authenticated frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderVerdict {
    /// The next frame from the slot's current run, or the first frame of a new run
    /// on a slot that has gone quiet.
    Fresh,
    /// Already seen, or from a run the slot has moved on from.
    Replayed,
    /// A second live sender on a slot that already has one.
    SecondSender,
}

#[derive(Debug, Clone)]
struct SlotMark {
    /// The current run's prefix and the highest counter accepted from it.
    current: SenderNonce,
    /// Prefixes of earlier runs on this slot, oldest first.
    retired: Vec<[u8; NONCE_PREFIX_LEN]>,
}

/// The per-slot sender state. Deliberately not pruned with the neighbour entries:
/// a slot that went quiet must still refuse its old frames when they come back.
/// Bounded by the slot space (`u8`) times [`RETIRED_PREFIXES`].
#[derive(Debug, Default)]
pub struct SenderMarks {
    by_slot: BTreeMap<u8, SlotMark>,
    /// Whether an accept happened since the marks were last written out.
    unsaved: bool,
}

impl SenderMarks {
    /// The marks persisted at `path`. An absent file is a node that has never
    /// heard a peer. An unreadable or malformed one is logged and starts empty:
    /// refusing to run the bus over it would ground the fleet's separation layer
    /// on a file this service itself writes.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "swarm_replay_state_unreadable");
                return Self::default();
            }
        };
        let persisted: PersistedMarks = match serde_json::from_slice(&text) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "swarm_replay_state_malformed");
                return Self::default();
            }
        };
        ados_protocol::sidecar::check_sidecar_version(
            "swarmbus-replay",
            persisted.version,
            REPLAY_STATE_VERSION,
        );
        let by_slot = persisted
            .slots
            .into_iter()
            .map(|s| {
                let mut retired = s.retired;
                // A hand-edited file must not lift the bound.
                let excess = retired.len().saturating_sub(RETIRED_PREFIXES);
                retired.drain(..excess);
                (
                    s.slot,
                    SlotMark {
                        current: SenderNonce {
                            prefix: s.prefix,
                            counter: s.counter,
                        },
                        retired,
                    },
                )
            })
            .collect();
        Self {
            by_slot,
            unsaved: false,
        }
    }

    /// The serialized marks when an accept happened since the last call, `None`
    /// when nothing changed. Clears the unsaved flag; a caller whose write then
    /// fails hands it back with [`Self::mark_unsaved`].
    pub fn take_unsaved(&mut self) -> Option<Vec<u8>> {
        if !self.unsaved {
            return None;
        }
        self.unsaved = false;
        let persisted = PersistedMarks {
            version: REPLAY_STATE_VERSION,
            slots: self
                .by_slot
                .iter()
                .map(|(slot, mark)| PersistedSlot {
                    slot: *slot,
                    prefix: mark.current.prefix,
                    counter: mark.current.counter,
                    retired: mark.retired.clone(),
                })
                .collect(),
        };
        serde_json::to_vec(&persisted).ok()
    }

    /// Re-flag the marks as unsaved after a failed write, so the next persist
    /// tick tries again.
    pub fn mark_unsaved(&mut self) {
        self.unsaved = true;
    }

    /// Judge `sender` for `slot`. `slot_live` is whether the slot's table entry is
    /// present and not stale. Pure: nothing changes until [`Self::accept`].
    pub fn verdict(&self, slot: u8, sender: SenderNonce, slot_live: bool) -> SenderVerdict {
        let Some(mark) = self.by_slot.get(&slot) else {
            return SenderVerdict::Fresh;
        };
        if sender.prefix == mark.current.prefix {
            return if sender.counter > mark.current.counter {
                SenderVerdict::Fresh
            } else {
                SenderVerdict::Replayed
            };
        }
        if mark.retired.contains(&sender.prefix) {
            return SenderVerdict::Replayed;
        }
        if slot_live {
            SenderVerdict::SecondSender
        } else {
            SenderVerdict::Fresh
        }
    }

    /// Record `sender` as the slot's latest accepted frame, retiring the previous
    /// run when the prefix changed.
    pub fn accept(&mut self, slot: u8, sender: SenderNonce) {
        self.unsaved = true;
        match self.by_slot.get_mut(&slot) {
            Some(mark) => {
                if mark.current.prefix != sender.prefix {
                    if mark.retired.len() == RETIRED_PREFIXES {
                        mark.retired.remove(0);
                    }
                    mark.retired.push(mark.current.prefix);
                }
                mark.current = sender;
            }
            None => {
                self.by_slot.insert(
                    slot,
                    SlotMark {
                        current: sender,
                        retired: Vec::new(),
                    },
                );
            }
        }
    }

    /// The next nonce a well-behaved sender on `slot` would use: the current run
    /// with its counter advanced, or a first frame from a per-slot run.
    #[cfg(test)]
    pub(crate) fn next_for(&self, slot: u8) -> SenderNonce {
        match self.by_slot.get(&slot) {
            Some(mark) => SenderNonce {
                prefix: mark.current.prefix,
                counter: mark.current.counter + 1,
            },
            None => SenderNonce {
                prefix: [slot; NONCE_PREFIX_LEN],
                counter: 0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(p: u8, counter: u32) -> SenderNonce {
        SenderNonce {
            prefix: [p; NONCE_PREFIX_LEN],
            counter,
        }
    }

    #[test]
    fn a_counter_at_or_below_the_last_accepted_one_is_a_replay() {
        let mut marks = SenderMarks::default();
        assert_eq!(marks.verdict(3, run(1, 10), false), SenderVerdict::Fresh);
        marks.accept(3, run(1, 10));
        assert_eq!(marks.verdict(3, run(1, 10), true), SenderVerdict::Replayed);
        assert_eq!(marks.verdict(3, run(1, 9), true), SenderVerdict::Replayed);
        // Counters need not be contiguous.
        assert_eq!(marks.verdict(3, run(1, 14), true), SenderVerdict::Fresh);
        // Other slots are judged on their own history.
        assert_eq!(marks.verdict(4, run(1, 0), true), SenderVerdict::Fresh);
    }

    #[test]
    fn a_new_run_is_accepted_only_once_the_slot_went_quiet_and_never_goes_back() {
        let mut marks = SenderMarks::default();
        marks.accept(3, run(1, 500));
        assert_eq!(
            marks.verdict(3, run(2, 0), true),
            SenderVerdict::SecondSender,
            "a live slot does not change hands"
        );
        assert_eq!(marks.verdict(3, run(2, 0), false), SenderVerdict::Fresh);
        marks.accept(3, run(2, 0));
        // The superseded run is a replay forever, live slot or not.
        assert_eq!(
            marks.verdict(3, run(1, 501), false),
            SenderVerdict::Replayed
        );
        assert_eq!(marks.verdict(3, run(1, 501), true), SenderVerdict::Replayed);
    }

    #[test]
    fn the_retired_history_is_bounded_and_drops_the_oldest_run() {
        let mut marks = SenderMarks::default();
        for p in 0..=RETIRED_PREFIXES as u8 + 1 {
            marks.accept(3, run(p, 0));
        }
        let mark = &marks.by_slot[&3];
        assert_eq!(mark.retired.len(), RETIRED_PREFIXES);
        assert!(
            !mark.retired.contains(&[0; NONCE_PREFIX_LEN]),
            "oldest dropped"
        );
        assert!(mark.retired.contains(&[1; NONCE_PREFIX_LEN]));
    }

    #[test]
    fn a_malformed_or_oversized_file_cannot_widen_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replay.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(SenderMarks::load(&path).by_slot.is_empty());

        // A file listing more retired runs than the bound keeps only the newest.
        let retired: Vec<[u8; NONCE_PREFIX_LEN]> = (0..RETIRED_PREFIXES as u8 + 3)
            .map(|p| [p; NONCE_PREFIX_LEN])
            .collect();
        let prefix = [0xEE_u8; NONCE_PREFIX_LEN];
        let body = serde_json::json!({
            "version": REPLAY_STATE_VERSION,
            "slots": [{"slot": 3, "prefix": prefix, "counter": 7, "retired": retired}],
        });
        std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();
        let marks = SenderMarks::load(&path);
        let mark = &marks.by_slot[&3];
        assert_eq!(mark.retired.len(), RETIRED_PREFIXES);
        assert!(!mark.retired.contains(&[0; NONCE_PREFIX_LEN]));
        assert_eq!(
            marks.verdict(3, run(0xEE, 7), false),
            SenderVerdict::Replayed
        );
        assert!(SenderMarks::load(&dir.path().join("absent.json"))
            .by_slot
            .is_empty());
    }
}
