//! CBBA phase 2 — the conflict-resolution decision table.
//!
//! A direct transcription of Table 1 in Choi, Brunet & How, *Consensus-Based
//! Decentralized Auctions for Robust Task Allocation* (IEEE T-RO 2009). The table
//! is what makes the auction converge without a coordinator: for every
//! combination of "who the sender thinks holds task j" against "who I think holds
//! task j", it names one of three outcomes, and every agent applying it to every
//! message reaches the same conflict-free assignment.
//!
//! Transcribed rather than simplified on purpose. Several rows look redundant and
//! are not: the ones keyed on the information timestamps `s` rather than the bids
//! `y` are what break ties when two agents hold contradictory second-hand
//! rumours about a third, and dropping them is how a "simplified" CBBA
//! livelocks.

use super::bid::BidVector;

/// One row's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAction {
    /// Take the sender's bid and winner for this task.
    Update,
    /// Void this task: nobody is known to hold it.
    Reset,
    /// Keep what we have.
    Leave,
}

/// A slot's information timestamp within a bid vector. An unknown slot reads 0,
/// the "no information" floor, so a roster that has grown never makes a lookup
/// fail.
fn stamp(v: &BidVector, roster: &[u8], slot: u8) -> u16 {
    roster
        .iter()
        .position(|s| *s == slot)
        .and_then(|i| v.s.get(i).copied())
        .unwrap_or(0)
}

/// The table row for task `j`, receiving from `sender` as `receiver`.
pub fn decide(
    j: usize,
    sender: u8,
    receiver: u8,
    theirs: &BidVector,
    mine: &BidVector,
    roster: &[u8],
) -> TableAction {
    use TableAction::{Leave, Reset, Update};
    let (Some(&yk), Some(&yi)) = (theirs.y.get(j), mine.y.get(j)) else {
        return Leave;
    };
    let (Some(&zk), Some(&zi)) = (theirs.z.get(j), mine.z.get(j)) else {
        return Leave;
    };
    let sk = |slot: u8| stamp(theirs, roster, slot);
    let si = |slot: u8| stamp(mine, roster, slot);

    match (zk, zi) {
        // --- the sender believes it holds j ---
        (Some(k), Some(i)) if k == sender && i == receiver => {
            // The direct contest. An EXACT tie is not a curiosity here: N drones
            // spawned at the same point score every task identically, and with
            // `yk > yi` alone neither side ever yields, so the auction settles
            // conflicted. CBBA requires bids to be unique or a deterministic
            // tie-break; the lower slot wins, which is antisymmetric — of the two
            // drones evaluating the same tie, exactly one concedes.
            if yk > yi || (yk == yi && sender < receiver) {
                Update
            } else {
                Leave
            }
        }
        (Some(k), Some(i)) if k == sender && i == sender => Update,
        (Some(k), Some(m)) if k == sender => {
            if sk(m) > si(m) || yk > yi {
                Update
            } else {
                Leave
            }
        }
        (Some(k), None) if k == sender => Update,

        // --- the sender believes WE hold j ---
        (Some(k), Some(i)) if k == receiver && i == receiver => Leave,
        (Some(k), Some(i)) if k == receiver && i == sender => Reset,
        (Some(k), Some(m)) if k == receiver => {
            if sk(m) > si(m) {
                Reset
            } else {
                Leave
            }
        }
        (Some(k), None) if k == receiver => Leave,

        // --- the sender believes a third party m holds j ---
        (Some(m), Some(i)) if i == receiver => {
            if sk(m) > si(m) && yk > yi {
                Update
            } else {
                Leave
            }
        }
        (Some(m), Some(k)) if k == sender => {
            if sk(m) > si(m) {
                Update
            } else {
                Reset
            }
        }
        (Some(m), Some(n)) if m == n => {
            if sk(m) > si(m) {
                Update
            } else {
                Leave
            }
        }
        (Some(m), Some(n)) => {
            // Two separate rows of the paper's table, identical in outcome: the
            // sender is newer on m AND newer on n, or newer on m with a better bid.
            if sk(m) > si(m) && (sk(n) > si(n) || yk > yi) {
                Update
            } else if sk(n) > si(n) && si(m) > sk(m) {
                Reset
            } else {
                Leave
            }
        }
        (Some(m), None) => {
            if sk(m) > si(m) {
                Update
            } else {
                Leave
            }
        }

        // --- the sender believes j is unheld ---
        (None, Some(i)) if i == receiver => Leave,
        (None, Some(k)) if k == sender => Update,
        (None, Some(m)) => {
            if sk(m) > si(m) {
                Update
            } else {
                Leave
            }
        }
        (None, None) => Leave,
    }
}

/// Apply a row's outcome. Returns whether anything changed, which is what the
/// convergence driver counts.
pub fn apply(action: TableAction, j: usize, theirs: &BidVector, mine: &mut BidVector) -> bool {
    let (before_y, before_z) = (mine.y[j], mine.z[j]);
    match action {
        TableAction::Update => {
            mine.y[j] = theirs.y[j];
            mine.z[j] = theirs.z[j];
        }
        TableAction::Reset => {
            mine.y[j] = 0.0;
            mine.z[j] = None;
        }
        TableAction::Leave => {}
    }
    mine.y[j].to_bits() != before_y.to_bits() || mine.z[j] != before_z
}

/// Merge information timestamps after integrating a message.
///
/// The sender was heard DIRECTLY, so its entry becomes `now`. Everything else the
/// sender knows is second-hand, so it is merged by recency — which is precisely
/// the mechanism the timestamp rows of the table arbitrate on.
pub fn merge_stamps(
    mine: &mut BidVector,
    theirs: &BidVector,
    roster: &[u8],
    sender: u8,
    now: u16,
) -> bool {
    let mut changed = false;
    for (i, slot) in roster.iter().enumerate() {
        let Some(entry) = mine.s.get_mut(i) else {
            continue;
        };
        let next = if *slot == sender {
            now
        } else {
            (*theirs.s.get(i).unwrap_or(&0)).max(*entry)
        };
        if next != *entry {
            *entry = next;
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::TableAction::{Leave, Reset, Update};
    use super::*;

    const ROSTER: [u8; 4] = [1, 2, 3, 4];
    const ME: u8 = 1;
    const THEM: u8 = 2;
    const OTHER: u8 = 3;
    const FOURTH: u8 = 4;

    fn vecs(
        their_bid: f32,
        their_z: Option<u8>,
        my_bid: f32,
        my_z: Option<u8>,
    ) -> (BidVector, BidVector) {
        let mut theirs = BidVector::new(1, ROSTER.len());
        let mut mine = BidVector::new(1, ROSTER.len());
        theirs.y[0] = their_bid;
        theirs.z[0] = their_z;
        mine.y[0] = my_bid;
        mine.z[0] = my_z;
        (theirs, mine)
    }

    fn row(their_bid: f32, their_z: Option<u8>, my_bid: f32, my_z: Option<u8>) -> TableAction {
        let (theirs, mine) = vecs(their_bid, their_z, my_bid, my_z);
        decide(0, THEM, ME, &theirs, &mine, &ROSTER)
    }

    #[test]
    fn sender_holds_it_and_outbids_me() {
        assert_eq!(row(9.0, Some(THEM), 5.0, Some(ME)), Update);
        assert_eq!(row(1.0, Some(THEM), 5.0, Some(ME)), Leave);
        // An exact tie breaks on the slot number, and it breaks ANTISYMMETRICALLY:
        // ME=1 keeps it against THEM=2, and THEM=2 would concede to ME=1. Without
        // that, N drones with identical scores all keep the same task forever;
        // with a symmetric rule they swap it forever.
        let (theirs, mine) = vecs(5.0, Some(THEM), 5.0, Some(ME));
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Leave);
        let (theirs, mine) = vecs(5.0, Some(ME), 5.0, Some(THEM));
        assert_eq!(decide(0, ME, THEM, &theirs, &mine, &ROSTER), Update);
    }

    #[test]
    fn sender_confirms_what_i_already_believed_about_it() {
        // I thought the sender held it; the sender agrees. Take its figure —
        // it is first-hand and mine was a rumour.
        assert_eq!(row(3.0, Some(THEM), 9.0, Some(THEM)), Update);
    }

    #[test]
    fn sender_holds_it_but_i_credit_a_third_party() {
        // Neither newer news of the third party nor a better bid: keep mine.
        assert_eq!(row(1.0, Some(THEM), 5.0, Some(OTHER)), Leave);
        // A better bid alone flips it.
        assert_eq!(row(9.0, Some(THEM), 5.0, Some(OTHER)), Update);
        // Fresher news about the third party flips it even on a worse bid, which
        // is how a stale rumour gets displaced.
        let (mut theirs, mut mine) = vecs(1.0, Some(THEM), 5.0, Some(OTHER));
        theirs.s[2] = 50;
        mine.s[2] = 10;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Update);
        // ...and not when MY news is the fresher one.
        theirs.s[2] = 5;
        mine.s[2] = 50;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Leave);
    }

    #[test]
    fn sender_holds_it_and_i_thought_nobody_did() {
        assert_eq!(row(1.0, Some(THEM), 0.0, None), Update);
    }

    #[test]
    fn sender_thinks_i_hold_it() {
        assert_eq!(row(4.0, Some(ME), 4.0, Some(ME)), Leave, "we agree");
        // The sender thinks I hold it while I think the sender does: nobody can
        // be sure, so void it and re-bid.
        assert_eq!(row(4.0, Some(ME), 4.0, Some(THEM)), Reset);
        assert_eq!(row(4.0, Some(ME), 0.0, None), Leave);
        // I credit a third party; only fresher news about it voids my belief.
        let (mut theirs, mut mine) = vecs(4.0, Some(ME), 4.0, Some(OTHER));
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Leave);
        theirs.s[2] = 9;
        mine.s[2] = 1;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Reset);
    }

    #[test]
    fn sender_credits_a_third_party() {
        // I hold it: only fresher news AND a better bid dislodges me.
        let (mut theirs, mine) = vecs(9.0, Some(OTHER), 5.0, Some(ME));
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Leave);
        theirs.s[2] = 9;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Update);
        theirs.y[0] = 1.0;
        assert_eq!(
            decide(0, THEM, ME, &theirs, &mine, &ROSTER),
            Leave,
            "fresher news alone must not take a task off its holder"
        );

        // I credited the SENDER; it now credits a third party. Fresher news is
        // an update, otherwise the sender contradicted itself and it is a reset.
        let (mut theirs, mine) = vecs(9.0, Some(OTHER), 5.0, Some(THEM));
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Reset);
        theirs.s[2] = 9;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Update);

        // Same third party: recency decides.
        let (mut theirs, mut mine) = vecs(9.0, Some(OTHER), 5.0, Some(OTHER));
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Leave);
        theirs.s[2] = 3;
        mine.s[2] = 1;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Update);

        // Two different third parties — the rows that exist purely to break
        // contradictory rumours.
        let (mut theirs, mut mine) = vecs(9.0, Some(OTHER), 5.0, Some(FOURTH));
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Leave);
        theirs.s[2] = 9; // fresher on m
        theirs.s[3] = 9; // fresher on n
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Update);
        theirs.s[3] = 0;
        assert_eq!(
            decide(0, THEM, ME, &theirs, &mine, &ROSTER),
            Update,
            "better bid"
        );
        theirs.y[0] = 1.0;
        theirs.s[2] = 0;
        mine.s[2] = 9;
        theirs.s[3] = 9;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Reset);

        // Nobody in my book: recency decides.
        let (mut theirs, mine) = vecs(9.0, Some(OTHER), 0.0, None);
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Leave);
        theirs.s[2] = 1;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Update);
    }

    #[test]
    fn sender_thinks_the_task_is_unheld() {
        assert_eq!(
            row(0.0, None, 5.0, Some(ME)),
            Leave,
            "I hold it; I know better"
        );
        assert_eq!(row(0.0, None, 5.0, Some(THEM)), Update, "it released it");
        assert_eq!(row(0.0, None, 0.0, None), Leave);
        let (mut theirs, mut mine) = vecs(0.0, None, 5.0, Some(OTHER));
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Leave);
        theirs.s[2] = 7;
        mine.s[2] = 2;
        assert_eq!(decide(0, THEM, ME, &theirs, &mine, &ROSTER), Update);
    }

    #[test]
    fn an_out_of_range_task_index_is_a_no_op() {
        let (theirs, mine) = vecs(9.0, Some(THEM), 0.0, None);
        assert_eq!(decide(7, THEM, ME, &theirs, &mine, &ROSTER), Leave);
    }

    #[test]
    fn apply_reports_whether_it_changed_anything() {
        let (theirs, mut mine) = vecs(9.0, Some(THEM), 5.0, Some(ME));
        assert!(apply(Update, 0, &theirs, &mut mine));
        assert_eq!((mine.y[0], mine.z[0]), (9.0, Some(THEM)));
        assert!(
            !apply(Update, 0, &theirs, &mut mine),
            "idempotent second apply"
        );
        assert!(apply(Reset, 0, &theirs, &mut mine));
        assert_eq!((mine.y[0], mine.z[0]), (0.0, None));
        assert!(!apply(Reset, 0, &theirs, &mut mine));
        assert!(!apply(Leave, 0, &theirs, &mut mine));
    }

    #[test]
    fn stamps_take_the_sender_first_hand_and_merge_the_rest_by_recency() {
        let mut mine = BidVector::new(1, ROSTER.len());
        let mut theirs = BidVector::new(1, ROSTER.len());
        mine.s = vec![5, 5, 40, 5];
        theirs.s = vec![1, 99, 10, 7];
        assert!(merge_stamps(&mut mine, &theirs, &ROSTER, THEM, 100));
        assert_eq!(mine.s[1], 100, "the sender was heard directly");
        assert_eq!(mine.s[0], 5, "my own entry is not overwritten by a rumour");
        assert_eq!(mine.s[2], 40, "mine was fresher");
        assert_eq!(mine.s[3], 7, "theirs was fresher");
        assert!(
            !merge_stamps(&mut mine, &theirs, &ROSTER, THEM, 100),
            "idempotent"
        );
    }
}
