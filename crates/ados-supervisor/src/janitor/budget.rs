//! The footprint budget: how much of the card the agent is allowed to occupy.
//!
//! Pure. The whole point of separating this out is that the arithmetic deciding
//! what gets deleted is decided in a test, not on a card that is already full.
//!
//! **Space, not throughput.** Write rate is wear: it shortens the life of a
//! card over months. Occupied space is what actually breaks these nodes, and it
//! breaks them fast — the card fills, the store cannot get the scratch it needs,
//! a write tears, the filesystem corrupts, and the box will not boot. Four
//! reflashes in eight days came from that sequence, not from wear. So the
//! janitor's primary trigger is footprint against a budget, and free-space
//! percentage is only the secondary net.
//!
//! Percentage would not have caught any of it. A 128 GB card at 3% used can
//! carry a runaway store growing by a gigabyte a day, and no percentage
//! threshold fires until the day it matters, by which point there is nothing
//! left to trim gracefully. An absolute budget fires when the agent starts
//! taking more than its share, which is the moment something can still be done
//! about it.
//!
//! **Per-category caps that sum to the budget.** A category at its cap is
//! trimmed even when the total is under budget, so recordings cannot quietly
//! eat the store's share on a node that happens not to be logging much. Each cap
//! has a floor beneath which nothing is reclaimed, and a floor may legitimately
//! hold a category above its cap — a single quarantined store larger than the
//! quarantine cap is the case that actually happens. That is reported as an
//! over-cap the janitor declined to fix, never silently ignored.

/// The categories the budget covers. Ordered as they are swept and reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    LogStore,
    QuarantinedStores,
    Recordings,
    PluginLogs,
    AuditLog,
    Journal,
    Apt,
}

impl Category {
    pub const ALL: [Category; 7] = [
        Category::LogStore,
        Category::QuarantinedStores,
        Category::Recordings,
        Category::PluginLogs,
        Category::AuditLog,
        Category::Journal,
        Category::Apt,
    ];

    /// The wire name, shared by the event detail, the sidecar and the renderer.
    pub fn as_str(self) -> &'static str {
        match self {
            Category::LogStore => "log_store",
            Category::QuarantinedStores => "quarantined_stores",
            Category::Recordings => "recordings",
            Category::PluginLogs => "plugin_logs",
            Category::AuditLog => "audit_log",
            Category::Journal => "journal",
            Category::Apt => "apt",
        }
    }
}

const MB: u64 = 1024 * 1024;

/// The total the agent is allowed to occupy on the card.
pub const DEFAULT_BUDGET_BYTES: u64 = 5 * 1024 * MB;

// The default caps, which sum EXACTLY to the budget. Sized from what the two
// rigs were actually holding when they were measured: 1.1 GB under `/var/ados`
// on the drone (1.0 GB of it a single quarantined store), 308 MB of journal, and
// 349 MB of apt on a ground station two days after a flash.
/// The live store plus its WAL. Sized above the store's own 1 GB retention cap
/// so the two bounds do not fight; with the store off this sits near zero.
pub const DEFAULT_CAP_LOG_STORE: u64 = 1_200 * MB;
/// Quarantined copies of a torn store. One 1 GB corpse was what filled the
/// drone, and the newest one is never reclaimed, so this cap is regularly held
/// above by its own floor — which is reported rather than hidden.
pub const DEFAULT_CAP_QUARANTINED: u64 = 400 * MB;
/// Operator recordings. The largest share, because they are the one category a
/// human deliberately created and the one most likely to be wanted.
pub const DEFAULT_CAP_RECORDINGS: u64 = 2_400 * MB;
/// Per-plugin logs.
pub const DEFAULT_CAP_PLUGIN_LOGS: u64 = 256 * MB;
/// The audit trail.
pub const DEFAULT_CAP_AUDIT: u64 = 64 * MB;
/// The persistent journal. Matches the `SystemMaxUse` the installer writes, so
/// the janitor and journald's own bound agree instead of racing.
pub const DEFAULT_CAP_JOURNAL: u64 = 400 * MB;
/// Downloaded packages plus the package index.
pub const DEFAULT_CAP_APT: u64 = 400 * MB;

/// Per-category caps. Sum to [`DEFAULT_BUDGET_BYTES`] by default, which is
/// asserted in a test so a later edit to one cap cannot silently break the
/// relationship between the parts and the whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub log_store: u64,
    pub quarantined_stores: u64,
    pub recordings: u64,
    pub plugin_logs: u64,
    pub audit_log: u64,
    pub journal: u64,
    pub apt: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            log_store: DEFAULT_CAP_LOG_STORE,
            quarantined_stores: DEFAULT_CAP_QUARANTINED,
            recordings: DEFAULT_CAP_RECORDINGS,
            plugin_logs: DEFAULT_CAP_PLUGIN_LOGS,
            audit_log: DEFAULT_CAP_AUDIT,
            journal: DEFAULT_CAP_JOURNAL,
            apt: DEFAULT_CAP_APT,
        }
    }
}

impl Caps {
    pub fn get(&self, c: Category) -> u64 {
        match c {
            Category::LogStore => self.log_store,
            Category::QuarantinedStores => self.quarantined_stores,
            Category::Recordings => self.recordings,
            Category::PluginLogs => self.plugin_logs,
            Category::AuditLog => self.audit_log,
            Category::Journal => self.journal,
            Category::Apt => self.apt,
        }
    }

    pub fn total(&self) -> u64 {
        Category::ALL
            .iter()
            .fold(0u64, |a, c| a.saturating_add(self.get(*c)))
    }
}

/// What each category is currently occupying, plus the installed product.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Footprint {
    pub log_store: u64,
    pub quarantined_stores: u64,
    pub recordings: u64,
    pub plugin_logs: u64,
    pub audit_log: u64,
    pub journal: u64,
    pub apt: u64,
    /// `/opt/ados` — the venv, the Python runtime, the models and the binaries.
    ///
    /// **Reported, never reclaimed, and EXCLUDED from the budget.** It is the
    /// installed product, not accumulation: it is whatever size the release is,
    /// it does not grow while the box runs, and deleting any of it does not free
    /// space so much as break the agent. Counting it inside the budget would
    /// mean a larger release silently ate the allowance for recordings, which is
    /// a strange way for a version bump to behave. It is reported because it is
    /// still 605 MB of a card somebody has to size, and a footprint report that
    /// omitted it would understate what the agent costs.
    pub installed: u64,
}

impl Footprint {
    pub fn get(&self, c: Category) -> u64 {
        match c {
            Category::LogStore => self.log_store,
            Category::QuarantinedStores => self.quarantined_stores,
            Category::Recordings => self.recordings,
            Category::PluginLogs => self.plugin_logs,
            Category::AuditLog => self.audit_log,
            Category::Journal => self.journal,
            Category::Apt => self.apt,
        }
    }

    /// Everything inside the budget. Deliberately excludes `installed`.
    pub fn budgeted_total(&self) -> u64 {
        Category::ALL
            .iter()
            .fold(0u64, |a, c| a.saturating_add(self.get(*c)))
    }

    /// The per-category pairs, for the event detail and the sidecar.
    pub fn pairs(&self) -> Vec<(&'static str, u64)> {
        Category::ALL
            .iter()
            .map(|c| (c.as_str(), self.get(*c)))
            .collect()
    }
}

/// How much a category is over its cap, or zero. Pure.
pub fn over_cap(footprint: &Footprint, caps: &Caps, c: Category) -> u64 {
    footprint.get(c).saturating_sub(caps.get(c))
}

/// Every category currently over its cap, worst first. Pure.
///
/// Worst first because a pass is bounded work on a box whose disk is the
/// problem: if only some of it gets done, the category holding the most excess
/// is the one worth doing.
pub fn over_cap_categories(footprint: &Footprint, caps: &Caps) -> Vec<(Category, u64)> {
    let mut out: Vec<(Category, u64)> = Category::ALL
        .iter()
        .map(|c| (*c, over_cap(footprint, caps, *c)))
        .filter(|(_, over)| *over > 0)
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// How the total stands against the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetState {
    /// Comfortably inside.
    Within,
    /// Close enough that the escalated reclaim is worth running.
    Near,
    /// At or past the budget.
    Over,
}

/// Fraction of the budget at which the escalated rungs engage before the budget
/// is actually breached. Acting only at 100% means acting when there is no room
/// left to act gracefully.
pub const NEAR_BUDGET_FRACTION: f64 = 0.85;

/// Where the total sits relative to the budget. Pure.
pub fn budget_state(budgeted_total: u64, budget: u64) -> BudgetState {
    if budget == 0 {
        // A zero budget is a misconfiguration, not an instruction to delete
        // everything. Treat it as no budget rather than as "everything is over".
        return BudgetState::Within;
    }
    if budgeted_total >= budget {
        BudgetState::Over
    } else if (budgeted_total as f64) >= (budget as f64) * NEAR_BUDGET_FRACTION {
        BudgetState::Near
    } else {
        BudgetState::Within
    }
}

#[cfg(test)]
mod tests;
