use super::*;

fn roster(n: usize) -> Vec<u8> {
    (1..=n as u8).collect()
}

/// `n` tasks strung out along east at 100 m spacing, unit reward.
fn line_tasks(n: usize) -> Vec<CbbaTask> {
    (0..n)
        .map(|i| CbbaTask::new(format!("t{i}"), Ned::new(0.0, 100.0 * i as f64, 0.0), 10.0))
        .collect()
}

/// One drone parked on each task, so every drone's greedy first pick differs.
fn agents_on_tasks(tasks: &[CbbaTask], n_agents: usize, capacity: usize) -> Vec<CbbaAgent> {
    (0..n_agents)
        .map(|i| {
            CbbaAgent::new(i as u8 + 1, tasks[i].pos, tasks.len(), n_agents).with_capacity(capacity)
        })
        .collect()
}

#[test]
fn converges_in_one_round_when_greedy_bids_are_conflict_free() {
    // Four drones each sitting on their own task: every drone's best pick is
    // distinct, so ONE build plus ONE broadcast is the whole auction. This is the
    // diameter-1 property — no bid needs a second hop to reach anybody.
    let tasks = line_tasks(4);
    let mut agents = agents_on_tasks(&tasks, 4, 1);
    let r = converge_broadcast(&mut agents, &tasks, &roster(4));
    assert!(r.converged, "{r:?}");
    assert_eq!(r.rounds, 1, "{r:?}");
    assert!(r.conflict_free && r.consensus, "{r:?}");
    for (slot, path) in &r.assignment {
        assert_eq!(
            path,
            &vec![*slot as usize - 1],
            "slot {slot} took its own task"
        );
    }
}

#[test]
fn a_fully_contested_auction_still_converges_conflict_free() {
    // Every drone's best pick is the SAME task: the auction must serialise the
    // losers onto their next-best picks. This needs more than one round by
    // construction, and the bound is the task count, not the fleet size times
    // the diameter.
    let tasks = line_tasks(4);
    let mut agents: Vec<CbbaAgent> = (1..=4u8)
        .map(|slot| CbbaAgent::new(slot, tasks[0].pos, tasks.len(), 4).with_capacity(1))
        .collect();
    let r = converge_broadcast(&mut agents, &tasks, &roster(4));
    assert!(r.converged, "{r:?}");
    assert!(r.conflict_free, "conflicts remain: {r:?}");
    assert!(r.consensus, "drones disagree: {r:?}");
    assert!(
        r.rounds <= tasks.len(),
        "diameter-1 bound is the task count, took {} rounds",
        r.rounds
    );
    let taken: Vec<usize> = r.assignment.values().flatten().copied().collect();
    assert_eq!(taken.len(), 4, "every drone got exactly one task: {r:?}");
}

#[test]
fn every_task_is_claimed_at_most_once_across_fleet_sizes() {
    for (n_agents, n_tasks, capacity) in [
        (1, 1, 1),
        (2, 5, 3),
        (7, 7, 1),
        (8, 20, 3),
        (24, 20, 1),
        (24, 24, 2),
    ] {
        let tasks = line_tasks(n_tasks);
        let mut agents: Vec<CbbaAgent> = (1..=n_agents as u8)
            .map(|slot| {
                // Spread the drones out so the greedy step is not degenerate.
                let pos = Ned::new(50.0 * slot as f64, 130.0 * slot as f64, 0.0);
                CbbaAgent::new(slot, pos, n_tasks, n_agents).with_capacity(capacity)
            })
            .collect();
        let r = converge_broadcast(&mut agents, &tasks, &roster(n_agents));
        assert!(r.converged, "n={n_agents} m={n_tasks}: {r:?}");
        assert!(r.conflict_free, "n={n_agents} m={n_tasks}: {r:?}");
        assert!(r.consensus, "n={n_agents} m={n_tasks}: {r:?}");
        for path in r.assignment.values() {
            assert!(path.len() <= capacity, "capacity exceeded: {path:?}");
        }
    }
}

#[test]
fn every_task_gets_taken_when_there_is_capacity_for_all_of_them() {
    let tasks = line_tasks(6);
    let mut agents: Vec<CbbaAgent> = (1..=3u8)
        .map(|slot| {
            CbbaAgent::new(slot, Ned::new(0.0, 180.0 * slot as f64, 0.0), 6, 3).with_capacity(2)
        })
        .collect();
    let r = converge_broadcast(&mut agents, &tasks, &roster(3));
    assert!(r.converged && r.conflict_free && r.consensus, "{r:?}");
    let mut taken: Vec<usize> = r.assignment.values().flatten().copied().collect();
    taken.sort_unstable();
    assert_eq!(
        taken,
        vec![0, 1, 2, 3, 4, 5],
        "no task left on the table: {r:?}"
    );
}

#[test]
fn the_nearer_drone_wins_a_contested_task() {
    // The discounted-reward score rewards arriving sooner, so the drone parked on
    // the task must outbid the one 400 m away.
    let tasks = line_tasks(1);
    let mut agents = vec![
        CbbaAgent::new(1, Ned::new(0.0, 400.0, 0.0), 1, 2).with_capacity(1),
        CbbaAgent::new(2, tasks[0].pos, 1, 2).with_capacity(1),
    ];
    let r = converge_broadcast(&mut agents, &tasks, &roster(2));
    assert!(r.converged && r.conflict_free, "{r:?}");
    assert_eq!(r.assignment[&2], vec![0], "the drone on the spot wins");
    assert!(r.assignment[&1].is_empty());
}

#[test]
fn a_bundle_is_ordered_by_travel_cost_not_by_task_index() {
    // Tasks laid out so the cheapest path visits them in reverse index order.
    let tasks = vec![
        CbbaTask::new("far", Ned::new(0.0, 300.0, 0.0), 10.0),
        CbbaTask::new("mid", Ned::new(0.0, 200.0, 0.0), 10.0),
        CbbaTask::new("near", Ned::new(0.0, 100.0, 0.0), 10.0),
    ];
    let mut a = CbbaAgent::new(1, Ned::ZERO, 3, 1).with_capacity(3);
    a.build_bundle(&tasks);
    let order: Vec<&str> = a.path().iter().map(|&j| tasks[j].id.as_str()).collect();
    assert_eq!(
        order,
        vec!["near", "mid", "far"],
        "path must be a cheap tour"
    );
    // The bundle records ADD order, which is a different thing and is what
    // release semantics walk.
    assert_eq!(a.bundle().len(), 3);
}

#[test]
fn losing_a_task_releases_it_and_everything_added_after_it() {
    let tasks = line_tasks(3);
    let mut a = CbbaAgent::new(1, Ned::ZERO, 3, 2).with_capacity(3);
    a.build_bundle(&tasks);
    assert_eq!(a.bundle().len(), 3);
    let first_added = a.bundle()[0];
    let last_added = a.bundle()[2];

    // A peer outbids us on the FIRST task we added.
    let mut theirs = BidVector::new(3, 2);
    theirs.y[first_added] = 1e6;
    theirs.z[first_added] = Some(2);
    assert!(a.receive(2, &theirs, &roster(2), 1));
    assert!(a.release_lost());
    assert!(
        a.bundle().is_empty(),
        "the whole tail goes: {:?}",
        a.bundle()
    );
    assert!(a.path().is_empty());
    // Bids priced against the vanished path are voided, but the LOST task keeps
    // the winner's figure — that is the peer's news, not ours to erase.
    assert_eq!(a.vector().z[last_added], None);
    assert_eq!(a.vector().y[last_added], 0.0);
    assert_eq!(a.vector().z[first_added], Some(2));
}

#[test]
fn releasing_keeps_tasks_added_before_the_lost_one() {
    let tasks = line_tasks(3);
    let mut a = CbbaAgent::new(1, Ned::ZERO, 3, 2).with_capacity(3);
    a.build_bundle(&tasks);
    let kept = a.bundle()[0];
    let lost = a.bundle()[1];
    let mut theirs = BidVector::new(3, 2);
    theirs.y[lost] = 1e6;
    theirs.z[lost] = Some(2);
    a.receive(2, &theirs, &roster(2), 1);
    a.release_lost();
    assert_eq!(a.bundle(), &[kept], "only the tail is released");
    assert!(a.path().contains(&kept));
    assert!(!a.path().contains(&lost));
    assert!(!a.release_lost(), "nothing left to release");
}

#[test]
fn a_peer_auctioning_a_different_problem_is_ignored() {
    let tasks = line_tasks(3);
    let mut a = CbbaAgent::new(1, Ned::ZERO, 3, 2).with_capacity(1);
    a.build_bundle(&tasks);
    let before = a.vector().clone();
    // Wrong task count, and wrong agent count.
    assert!(!a.receive(2, &BidVector::new(2, 2), &roster(2), 1));
    assert!(!a.receive(2, &BidVector::new(3, 5), &roster(2), 1));
    assert_eq!(a.vector(), &before);
}

#[test]
fn an_empty_task_set_settles_immediately() {
    let mut agents: Vec<CbbaAgent> = (1..=3u8)
        .map(|s| CbbaAgent::new(s, Ned::ZERO, 0, 3))
        .collect();
    let r = converge_broadcast(&mut agents, &[], &roster(3));
    assert!(r.converged);
    assert_eq!(r.rounds, 0, "nothing to auction is not a round: {r:?}");
    assert!(r.conflict_free && r.consensus);
    for path in r.assignment.values() {
        assert!(path.is_empty());
    }
}

#[test]
fn a_lone_drone_takes_what_it_can_carry_and_no_more() {
    let tasks = line_tasks(9);
    let mut agents = vec![CbbaAgent::new(1, Ned::ZERO, 9, 1).with_capacity(4)];
    let r = converge_broadcast(&mut agents, &tasks, &roster(1));
    assert!(r.converged, "{r:?}");
    assert_eq!(r.assignment[&1].len(), 4);
    assert_eq!(r.rounds, 1);
}

#[test]
fn conflicts_and_consensus_detect_a_genuine_split() {
    let tasks = line_tasks(2);
    let mut a = CbbaAgent::new(1, tasks[0].pos, 2, 2).with_capacity(1);
    let mut b = CbbaAgent::new(2, tasks[0].pos, 2, 2).with_capacity(1);
    // Phase 1 only, no exchange: both grabbed task 0.
    a.build_bundle(&tasks);
    b.build_bundle(&tasks);
    assert_eq!(conflicts(&[a.clone(), b.clone()]), vec![0]);
    assert!(!consensus_reached(&[a.clone(), b.clone()]));
    assert!(consensus_reached(&[]));
    assert!(consensus_reached(std::slice::from_ref(&a)));
}

#[test]
fn assignment_surfaces_the_next_task_and_its_bundle_position() {
    let tasks = vec![
        CbbaTask::new("beta", Ned::new(0.0, 400.0, 0.0), 10.0),
        CbbaTask::new("alpha", Ned::new(0.0, 100.0, 0.0), 10.0),
    ];
    let mut a = CbbaAgent::new(1, Ned::ZERO, 2, 1).with_capacity(2);
    assert_eq!(a.assignment(&tasks), TaskAssignment::default());
    a.build_bundle(&tasks);
    let got = a.assignment(&tasks);
    assert_eq!(
        got.task_id.as_deref(),
        Some("alpha"),
        "the NEXT task, not the first bid"
    );
    assert_eq!(got.bundle_position, Some(0), "alpha was also added first");
}

#[test]
fn the_score_is_time_discounted_so_a_closer_task_is_worth_more() {
    let near = vec![CbbaTask::new("n", Ned::new(0.0, 10.0, 0.0), 10.0)];
    let far = vec![CbbaTask::new("f", Ned::new(0.0, 1000.0, 0.0), 10.0)];
    let s_near = path_score(Ned::ZERO, CBBA_SPEED_MPS, CBBA_DISCOUNT, &near, &[0]);
    let s_far = path_score(Ned::ZERO, CBBA_SPEED_MPS, CBBA_DISCOUNT, &far, &[0]);
    assert!(s_near > s_far, "{s_near} vs {s_far}");
    assert!(s_far > 0.0, "discounting must not go negative: {s_far}");
    // Diminishing marginal gain: inserting a task into a path that already has
    // one is worth less than that task was worth alone, which is the property
    // CBBA's convergence proof rests on. The two tasks must NOT be collinear with
    // the start — on a straight line the detour is free and the marginal gain
    // equals the standalone value exactly, so a strict inequality there would be
    // testing floating-point noise rather than the property.
    let both = vec![
        CbbaTask::new("east", Ned::new(0.0, 200.0, 0.0), 10.0),
        CbbaTask::new("north", Ned::new(200.0, 0.0, 0.0), 10.0),
    ];
    let east_only = path_score(Ned::ZERO, CBBA_SPEED_MPS, CBBA_DISCOUNT, &both, &[0]);
    let north_alone = path_score(Ned::ZERO, CBBA_SPEED_MPS, CBBA_DISCOUNT, &both, &[1]);
    let marginal = path_score(Ned::ZERO, CBBA_SPEED_MPS, CBBA_DISCOUNT, &both, &[0, 1]) - east_only;
    assert!(
        marginal < north_alone * 0.9,
        "marginal {marginal} should be well under {north_alone}"
    );
    assert!(marginal > 0.0, "a reachable task still has positive value");
    // A degenerate speed falls back rather than dividing by zero.
    assert!(path_score(Ned::ZERO, 0.0, CBBA_DISCOUNT, &near, &[0]).is_finite());
    assert!(path_score(Ned::ZERO, f64::NAN, CBBA_DISCOUNT, &near, &[0]).is_finite());
}

#[test]
fn a_bid_vector_from_the_wire_drives_the_same_decision_as_the_local_one() {
    // The codec sits between phase 1 and phase 2 in the real system, so a lossy
    // encode would change the auction outcome. Prove the round trip is inert.
    let tasks = line_tasks(3);
    let mut a = CbbaAgent::new(1, Ned::ZERO, 3, 2).with_capacity(2);
    let mut b = CbbaAgent::new(2, tasks[2].pos, 3, 2).with_capacity(2);
    a.build_bundle(&tasks);
    b.build_bundle(&tasks);

    let mut direct = a.clone();
    let mut wired = a.clone();
    let on_wire = BidVector::decode(&b.vector().encode(), 3, 2).expect("round trip");
    assert!(
        direct.receive(2, b.vector(), &roster(2), 1) == wired.receive(2, &on_wire, &roster(2), 1)
    );
    assert_eq!(direct.vector(), wired.vector());
}
