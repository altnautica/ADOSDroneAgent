//! A drone paired by auto-pair alone: its binds carry no fleet assignment,
//! because the ground issues slots against a device id it learns only from the
//! drone's presence beacon, which a drone parked on slot 0 never sends.
//!
//! The bound drone must hold an address its radio accepts (the default
//! fleet's provisional first slot, which is also the slot a fresh ground's
//! registry issues it, so the end-state check holds it to the registry), on
//! both commit paths: the bind's own apply, and the apply a restarted
//! supervisor finishes after being killed past its peer-evidence gate (odd
//! seeds kill the drone mid-bind). The ground is on the default fleet on
//! about half the seeds and on another fleet on the rest; the drone must end
//! where that ground listens either way (its own fleet, or the default fleet's
//! first slot it also hears). The ground's enrolment and the delivery of its
//! fleet id and slot that follow are proven in `ados-control`
//! (`gs_fleet_enroll::auto_pair_e2e`), which this crate cannot depend on.

use crate::check::Outcome;
use crate::sim::{simulate, Plan, Run};
use crate::world::{Faults, DRONE};

/// See the module docs.
pub fn unissued_drone(seed: u64) -> (Run, &'static [Outcome]) {
    let kill = (!seed.is_multiple_of(2)).then_some((DRONE, (1.0, 12.0)));
    let plan = Plan {
        gs_present: true,
        first_bind_s: [(0.0, 5.0), (0.0, 5.0)],
        attempts: [3, 3],
        kill,
        reconcile: true,
        unissued_drone: true,
        ..Plan::default()
    };
    let allowed: &[Outcome] = if kill.is_some() {
        &[Outcome::Bound, Outcome::Aborted]
    } else {
        &[Outcome::Bound]
    };
    (simulate(seed, Faults::clean(), plan), allowed)
}
