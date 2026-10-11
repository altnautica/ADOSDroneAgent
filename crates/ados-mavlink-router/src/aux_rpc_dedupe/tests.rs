use super::*;

fn frags(n: usize, len: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| vec![i as u8; len]).collect()
}

#[test]
fn a_second_arrival_while_the_first_runs_is_a_duplicate() {
    let d = RequestDedupe::new();
    assert!(matches!(d.admit(9), Admit::Fresh));
    assert!(
        matches!(d.admit(9), Admit::Duplicate),
        "the original call is still running and will answer both"
    );
}

#[test]
fn a_completed_request_replays_the_identical_bytes() {
    let d = RequestDedupe::new();
    let answer = frags(3, 64);
    assert!(matches!(d.admit(9), Admit::Fresh));
    d.complete(9, &answer);
    match d.admit(9) {
        Admit::Replay(got) => assert_eq!(
            got, answer,
            "a replay must be byte-identical, or a retried write is a second write"
        ),
        other => panic!("expected a replay, got {other:?}"),
    }
}

#[test]
fn abandon_reopens_an_id_that_produced_nothing_but_spares_an_answer() {
    let d = RequestDedupe::new();
    assert!(matches!(d.admit(3), Admit::Fresh));
    assert!(matches!(d.admit(3), Admit::Duplicate));
    d.abandon(3);
    assert!(
        matches!(d.admit(3), Admit::Fresh),
        "a call that answered nothing must be retryable"
    );

    d.complete(3, &frags(1, 8));
    d.abandon(3);
    assert!(
        matches!(d.admit(3), Admit::Replay(_)),
        "abandon clears an in-flight marker only, never a cached answer"
    );
}

#[test]
fn an_in_flight_marker_is_never_evicted_to_make_room() {
    let d = RequestDedupe::new();
    // One request is still running. Its marker is what makes a retransmit a
    // dropped duplicate instead of a second execution.
    let running = 9_999u32;
    assert!(matches!(d.admit(running), Admit::Fresh));

    // Now flood the cache with completed answers, well past both caps.
    let big = frags(1, 100 * 1024);
    for id in 0..(DEDUPE_MAX_ENTRIES as u32 + 16) {
        d.admit(id);
        d.complete(id, &big);
    }

    // The in-flight marker survived, so a resend of that request is still
    // refused. Before preferring completed records for eviction, the
    // byte-cap loop would shed zero-byte in-flight markers that could never
    // reduce the byte total, and this read Fresh — re-running the write.
    assert!(
        matches!(d.admit(running), Admit::Duplicate),
        "an unfinished request must still be deduplicated after cache pressure"
    );
    assert!(d.lock().total_bytes() <= DEDUPE_MAX_BYTES);
}

#[test]
fn one_past_the_cap_evicts_the_oldest() {
    let d = RequestDedupe::new();
    for id in 0..DEDUPE_MAX_ENTRIES as u32 {
        assert!(matches!(d.admit(id), Admit::Fresh));
        d.complete(id, &frags(1, 8));
    }
    assert!(
        matches!(d.admit(0), Admit::Replay(_)),
        "at exactly the cap nothing has been dropped yet"
    );

    assert!(matches!(d.admit(DEDUPE_MAX_ENTRIES as u32), Admit::Fresh));
    assert!(
        matches!(d.admit(0), Admit::Fresh),
        "the oldest entry is evicted to make room for the 65th"
    );
}

#[test]
fn an_entry_past_the_ttl_is_not_replayed() {
    let d = RequestDedupe::new();
    let t0 = Instant::now();
    assert!(matches!(d.admit_at(7, t0), Admit::Fresh));
    d.complete_at(7, &frags(1, 16), t0);

    assert!(matches!(
        d.admit_at(7, t0 + DEDUPE_TTL - Duration::from_secs(1)),
        Admit::Replay(_)
    ));
    assert!(
        matches!(
            d.admit_at(7, t0 + DEDUPE_TTL + Duration::from_secs(1)),
            Admit::Fresh
        ),
        "a stale answer must never be handed to a new call that reuses the id"
    );
}

#[test]
fn an_in_flight_marker_does_not_outlive_the_ttl() {
    let d = RequestDedupe::new();
    let t0 = Instant::now();
    assert!(matches!(d.admit_at(4, t0), Admit::Fresh));
    assert!(
        matches!(
            d.admit_at(4, t0 + DEDUPE_TTL + Duration::from_secs(1)),
            Admit::Fresh
        ),
        "a handler that died without completing must not wedge its id shut"
    );
}

#[test]
fn the_cache_never_holds_more_than_its_byte_cap() {
    let d = RequestDedupe::new();
    let big = frags(1, 100 * 1024);
    for id in 0..4u32 {
        assert!(matches!(d.admit(id), Admit::Fresh));
        d.complete(id, &big);
        assert!(
            d.lock().total_bytes() <= DEDUPE_MAX_BYTES,
            "byte cap breached after id {id}"
        );
    }
    assert!(
        matches!(d.admit(3), Admit::Replay(_)),
        "the newest answer survives the eviction it caused"
    );
    assert!(
        matches!(d.admit(0), Admit::Fresh),
        "the oldest answers were evicted to stay under the byte cap"
    );
}
