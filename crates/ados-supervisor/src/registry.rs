//! The catalog of managed systemd service units and the per-service runtime
//! state used by the orchestration loop.
//!
//! The catalog is every [`ados_protocol::units::UNITS`] row that carries a
//! supervisor tier, in table order. A shipped unit with no tier (the
//! supervisor itself, boot oneshots, units pulled up by `WantedBy=`) is not
//! managed here, and neither is a unit a native daemon owns in-process: those
//! are retired and pruned by the installer. The supervisor never spawns these
//! processes itself; it issues `systemctl` against this catalog. systemd
//! remains the process manager and owns the cgroup, restart, and journald
//! wiring.

use std::collections::VecDeque;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use ados_protocol::units::{self, UnitSpec};

/// Service tier, from the unit table. Start order is core first, then
/// hardware, then on-demand.
pub use ados_protocol::units::Category;

/// Failures inside the window at which a service is labelled crash-looping.
pub const MAX_FAILURES: usize = 5;
/// Sliding window over which failures are counted for the crash-loop label.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How often the monitor re-attempts a service parked in `Failed`/`CrashLooping`
/// so it self-recovers when the underlying condition (e.g. a hot-plugged
/// camera) returns, instead of staying dead until a manual restart. Fixed, in
/// the 2-5 s recovery band and matching the monitor tick. The crash-loop label
/// never stretches it: a drone whose FC needs one more attempt after a brownout
/// gets that attempt within 5 s however many came before.
pub const PARKED_RETRY_COOLDOWN: Duration = Duration::from_secs(5);

/// Runtime lifecycle state of a single managed unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Failed,
    /// Failed [`MAX_FAILURES`] times inside [`FAILURE_WINDOW`]. A label only:
    /// the unit is still retried on the fixed parked cadence.
    CrashLooping,
    /// The unit's main process exited 0 on purpose: the node has nothing for
    /// it to run. Not a failure, and not restarted.
    Idle,
}

impl ServiceState {
    /// The lowercase wire string used in the service-transition event detail.
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Stopped => "stopped",
            ServiceState::Starting => "starting",
            ServiceState::Running => "running",
            ServiceState::Failed => "failed",
            ServiceState::CrashLooping => "crash_looping",
            ServiceState::Idle => "idle",
        }
    }
}

/// A catalog entry. `profile_gate` scopes the unit to a pipe-separated set of
/// agent profiles (`None` = any). `role_gate` is a pipe-separated set of
/// ground station roles the unit runs under (`None` = any role); only
/// consulted when the profile gate is `ground_station`.
#[derive(Debug, Clone, Copy)]
pub struct ServiceDef {
    /// The unit name without `.service` (`ados-video`).
    pub name: &'static str,
    pub category: Category,
    pub profile_gate: Option<&'static str>,
    pub role_gate: Option<&'static str>,
    /// True for the lean headless KEEP set: the minimal services a zero-Python
    /// flight node runs. When the agent is in headless mode, the gate blocks
    /// every service that is NOT in this set, so the box boots only the Rust
    /// core.
    pub headless_keep: bool,
    /// True for units whose `exit 0` means "this node has nothing for me to
    /// run" (the unit file carries `RestartPreventExitStatus=0`). Such an exit
    /// parks the row in [`ServiceState::Idle`] instead of counting a death.
    pub nothing_to_run_ok: bool,
}

/// The pipe-joined form of every multi-entry profile or role list in the
/// table, built once and owned by this static.
static JOINED_GATES: LazyLock<Vec<(&'static [&'static str], String)>> = LazyLock::new(|| {
    let mut out: Vec<(&'static [&'static str], String)> = Vec::new();
    for unit in units::UNITS {
        for list in [unit.profiles, unit.roles] {
            if list.len() > 1 && !out.iter().any(|(k, _)| *k == list) {
                out.push((list, list.join("|")));
            }
        }
    }
    out
});

/// A gate string for a table list: `None` when empty, else the pipe-joined
/// set.
fn gate(list: &'static [&'static str]) -> Option<&'static str> {
    match list {
        [] => None,
        [one] => Some(one),
        many => JOINED_GATES
            .iter()
            .find(|(k, _)| *k == many)
            .map(|(_, joined)| joined.as_str()),
    }
}

impl ServiceDef {
    fn from_unit(unit: &'static UnitSpec, category: Category) -> Self {
        ServiceDef {
            name: unit.stem(),
            category,
            profile_gate: gate(unit.profiles),
            role_gate: gate(unit.roles),
            headless_keep: unit.headless_keep,
            nothing_to_run_ok: unit.idle_ok,
        }
    }
}

/// Every service the supervisor knows about, in canonical order.
pub static SERVICE_REGISTRY: LazyLock<Vec<ServiceDef>> = LazyLock::new(|| {
    units::UNITS
        .iter()
        .filter_map(|u| u.category.map(|c| ServiceDef::from_unit(u, c)))
        .collect()
});

/// Per-service mutable runtime state held by the supervisor.
#[derive(Debug)]
pub struct ServiceSpec {
    pub name: &'static str,
    pub category: Category,
    pub profile_gate: Option<&'static str>,
    pub role_gate: Option<&'static str>,
    /// Mirrors `ServiceDef::headless_keep`: whether this unit is in the lean
    /// headless KEEP set the gate permits when the agent runs headless.
    pub headless_keep: bool,
    /// Mirrors `ServiceDef::nothing_to_run_ok`.
    pub nothing_to_run_ok: bool,
    pub state: ServiceState,
    /// Failure timestamps inside the crash-loop window.
    pub failure_times: VecDeque<Instant>,
    /// Last monitor-driven retry of a parked service (cooldown bound).
    pub last_retry_at: Option<Instant>,
    /// Set when the adoption sweep promoted this row: the unit was brought up
    /// by its own enablement (the installer enables it, `WantedBy=` the
    /// supervisor or `multi-user.target`), not by this process. Sticky for the
    /// life of the process, because a death-restart by the monitor does not
    /// change what brings the unit up. Shutdown leaves an adopted unit to the
    /// service manager (see `Supervisor::stop`).
    pub adopted: bool,
}

impl ServiceSpec {
    pub fn from_def(d: &ServiceDef) -> Self {
        ServiceSpec {
            name: d.name,
            category: d.category,
            profile_gate: d.profile_gate,
            role_gate: d.role_gate,
            headless_keep: d.headless_keep,
            nothing_to_run_ok: d.nothing_to_run_ok,
            state: ServiceState::Stopped,
            failure_times: VecDeque::new(),
            last_retry_at: None,
            adopted: false,
        }
    }

    /// Record a failure and label the unit crash-looping when the window
    /// threshold is hit. Returns true when it is now crash-looping. The label
    /// never blocks a start; the parked retry keeps its fixed cadence.
    pub fn record_failure(&mut self, now: Instant) -> bool {
        self.failure_times.push_back(now);
        self.prune_failures(now);
        if self.failure_times.len() >= MAX_FAILURES {
            self.state = ServiceState::CrashLooping;
            true
        } else {
            false
        }
    }

    /// Drop failure timestamps older than the window.
    pub fn prune_failures(&mut self, now: Instant) {
        while let Some(&front) = self.failure_times.front() {
            if now.duration_since(front) >= FAILURE_WINDOW {
                self.failure_times.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Build the ordered runtime spec list from the static registry.
pub fn build_specs() -> Vec<ServiceSpec> {
    SERVICE_REGISTRY.iter().map(ServiceSpec::from_def).collect()
}

#[cfg(test)]
mod tests;
