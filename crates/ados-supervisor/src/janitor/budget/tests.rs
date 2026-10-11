use super::*;

#[test]
fn the_default_caps_sum_to_the_default_budget() {
    // The parts have to add up to the whole, or the budget is a number that
    // describes nothing. Asserted so a later edit to one cap is caught here
    // rather than by a node quietly exceeding a budget it was said to obey.
    assert_eq!(Caps::default().total(), DEFAULT_BUDGET_BYTES);
}

#[test]
fn the_installed_product_is_outside_the_budget() {
    // A release that ships a bigger model must not eat the allowance for
    // recordings. `/opt/ados` is reported and never counted.
    let f = Footprint {
        recordings: 100,
        installed: 605 * MB,
        ..Footprint::default()
    };
    assert_eq!(f.budgeted_total(), 100);
    assert!(!f.pairs().iter().any(|(name, _)| *name == "installed"));
}

#[test]
fn a_category_over_its_cap_is_found_even_when_the_total_is_fine() {
    // The point of per-category caps: recordings cannot take the store's
    // share just because the store happens to be idle.
    let caps = Caps::default();
    let f = Footprint {
        recordings: caps.recordings + 500 * MB,
        ..Footprint::default()
    };
    assert!(
        f.budgeted_total() < DEFAULT_BUDGET_BYTES,
        "the total is well within budget"
    );
    let over = over_cap_categories(&f, &caps);
    assert_eq!(over, vec![(Category::Recordings, 500 * MB)]);
}

#[test]
fn categories_within_their_caps_are_not_listed() {
    let caps = Caps::default();
    let f = Footprint {
        recordings: caps.recordings,
        journal: caps.journal - 1,
        ..Footprint::default()
    };
    assert!(over_cap_categories(&f, &caps).is_empty());
}

#[test]
fn the_worst_over_cap_category_is_reported_first() {
    let caps = Caps::default();
    let f = Footprint {
        recordings: caps.recordings + 10 * MB,
        journal: caps.journal + 900 * MB,
        apt: caps.apt + 100 * MB,
        ..Footprint::default()
    };
    let over = over_cap_categories(&f, &caps);
    assert_eq!(
        over.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        vec![Category::Journal, Category::Apt, Category::Recordings]
    );
}

#[test]
fn the_budget_engages_before_it_is_breached() {
    let b = DEFAULT_BUDGET_BYTES;
    assert_eq!(budget_state(0, b), BudgetState::Within);
    assert_eq!(budget_state(b / 2, b), BudgetState::Within);
    // 85% is where acting still leaves room to act.
    assert_eq!(budget_state((b as f64 * 0.86) as u64, b), BudgetState::Near);
    assert_eq!(budget_state(b, b), BudgetState::Over);
    assert_eq!(budget_state(b + 1, b), BudgetState::Over);
}

#[test]
fn a_zero_budget_is_a_misconfiguration_not_an_order_to_delete_everything() {
    assert_eq!(budget_state(10_000, 0), BudgetState::Within);
}

#[test]
fn the_measured_drone_footprint_is_over_its_quarantine_cap() {
    // What the drone actually held: 1.1 GB under /var/ados, 1.0 GB of it a
    // single quarantined store, plus 308 MB of journal and 186 MB of apt.
    // The total is inside 5 GB, so a total-only budget would have said the
    // box was fine while one category held five sixths of the agent's space.
    let caps = Caps::default();
    let f = Footprint {
        log_store: 100 * MB,
        quarantined_stores: 1_024 * MB,
        journal: 308 * MB,
        apt: 186 * MB,
        installed: 605 * MB,
        ..Footprint::default()
    };
    assert_eq!(
        budget_state(f.budgeted_total(), DEFAULT_BUDGET_BYTES),
        BudgetState::Within
    );
    let over = over_cap_categories(&f, &caps);
    assert_eq!(over, vec![(Category::QuarantinedStores, 624 * MB)]);
}
