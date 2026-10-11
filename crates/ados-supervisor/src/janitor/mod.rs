//! The disk janitor — the runtime half of keeping the card from filling.
//!
//! Two field nodes were reflashed four times in eight days because their SD
//! cards filled and then corrupted. The measurement afterwards found five
//! separate things accumulating with nothing anywhere reclaiming them: the apt
//! archive cache and package index (349 MB on a ground station two days after a
//! flash), quarantined copies of a store that had torn (1.0 GB on a drone),
//! per-plugin logs systemd appends to with no rotation configured, operator
//! recordings, and the audit trail.
//!
//! The one-shot half of the fix lives in the installer, which reclaims apt right
//! after the packages land. This is the half that runs forever: an hourly
//! reconciler inside the supervisor — not a new unit, because seven reconcilers
//! already live here and an eighth service is eight more things that can fail to
//! start.
//!
//! **Space is the trigger, not throughput.** Write rate is wear: it shortens a
//! card's life over months. Occupied space is what actually breaks these nodes,
//! and it breaks them in days — the card fills, a store rewrite cannot get the
//! scratch it needs, a write tears, the filesystem corrupts and the box will not
//! boot. So the primary signal is the agent's own footprint against a budget
//! (default 5 GB, per-category caps in [`budget`]), with free-space percentage
//! kept only as the secondary net for a card shared with something else.
//!
//! Percentage alone would have caught none of it: a 128 GB card at 3% used can
//! carry a runaway store, and no percentage threshold fires until the day there
//! is nothing left to trim gracefully.
//!
//! Three rungs, described in [`plan`], from whichever of those two signals is
//! harsher. Routine reclaims what is unambiguously waste. Pressure and Critical
//! also give up things that have some value, in ascending order of how much.
//! Per-category caps are enforced at every rung, so a category over its share is
//! trimmed even on a box with room to spare — recordings must not quietly take
//! the store's allowance on a node that happens not to be logging.
//!
//! Two rules hold at every rung, because a janitor that quietly deletes evidence
//! is worse than the full disk it was fitted to prevent:
//!
//! 1. **Nothing is reclaimed without being recorded.** Every pass emits one
//!    `janitor.reclaimed` event carrying the bytes freed per category. A silent
//!    janitor cannot be told apart from a disk that stopped filling on its own,
//!    and an operator reading a storage diagnostic needs to know which of those
//!    happened.
//! 2. **Every category has a floor.** The newest quarantined store, the tail of
//!    each log, the newest recordings and a minimum journal all survive the most
//!    aggressive pass. `/etc/ados` and `/opt/ados` are refused outright at the
//!    removal helper, so no category can reach the config, the radio keys or the
//!    installed runtime.
//!
//! Idempotent by construction: each category's trigger is a threshold on the
//! current size, so a second pass immediately after a first finds everything
//! under its cap and reclaims nothing.

pub mod budget;
pub mod plan;
pub mod reclaim;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ados_protocol::logd::emitter::EventEmitter;
use ados_protocol::logd::{Fields, Level, Value};

use plan::{JanitorConfig, Rung};

mod settings;
pub use settings::read_config_from;

mod sweep;
pub use sweep::{reclaimable, sweep};

#[cfg(target_os = "linux")]
use crate::config::CONFIG_YAML;

/// The event every pass emits, whether or not it freed anything.
pub const JANITOR_EVENT_KIND: &str = "janitor.reclaimed";

/// Where the last pass is mirrored for the storage diagnostic to read.
#[cfg(target_os = "linux")]
const SIDECAR_PATH: &str = "/run/ados/janitor.json";
/// Schema version of `janitor.json`. Kept in step with the contracts registry.
#[cfg(target_os = "linux")]
const SIDECAR_VERSION: u16 = 1;

/// The filesystem whose free space selects the rung. `/var` is where every
/// accumulating thing on this box lives.
#[cfg(target_os = "linux")]
const PRESSURE_PATH: &str = "/var";

/// Per-category byte totals for one pass. Ordered as the categories are swept.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct Reclaimed {
    pub apt_archives: u64,
    pub apt_lists: u64,
    pub plugin_logs: u64,
    pub audit_log: u64,
    pub recordings: u64,
    pub journal: u64,
    pub quarantined_stores: u64,
}

impl Reclaimed {
    /// Everything this pass freed.
    pub fn total(&self) -> u64 {
        [
            self.apt_archives,
            self.apt_lists,
            self.plugin_logs,
            self.audit_log,
            self.recordings,
            self.journal,
            self.quarantined_stores,
        ]
        .iter()
        .fold(0u64, |a, b| a.saturating_add(*b))
    }

    /// Fold another pass's totals in. Saturating, so a pathological figure can
    /// never wrap the accounting the event is built from.
    pub fn add(&mut self, other: &Reclaimed) {
        self.apt_archives = self.apt_archives.saturating_add(other.apt_archives);
        self.apt_lists = self.apt_lists.saturating_add(other.apt_lists);
        self.plugin_logs = self.plugin_logs.saturating_add(other.plugin_logs);
        self.audit_log = self.audit_log.saturating_add(other.audit_log);
        self.recordings = self.recordings.saturating_add(other.recordings);
        self.journal = self.journal.saturating_add(other.journal);
        self.quarantined_stores = self
            .quarantined_stores
            .saturating_add(other.quarantined_stores);
    }

    /// The per-category pairs, for the event detail and the sidecar. One place
    /// so the two can never disagree about what a pass did.
    pub fn pairs(&self) -> [(&'static str, u64); 7] {
        [
            ("apt_archives", self.apt_archives),
            ("apt_lists", self.apt_lists),
            ("plugin_logs", self.plugin_logs),
            ("audit_log", self.audit_log),
            ("recordings", self.recordings),
            ("journal", self.journal),
            ("quarantined_stores", self.quarantined_stores),
        ]
    }
}

#[cfg(target_os = "linux")]
fn read_config() -> JanitorConfig {
    match std::fs::read_to_string(CONFIG_YAML) {
        Ok(t) => read_config_from(&t),
        Err(_) => JanitorConfig::default(),
    }
}

/// The directories one pass sweeps, resolved once so a test can point every one
/// of them at a temporary tree.
pub struct Roots {
    pub apt_archives: PathBuf,
    pub apt_lists: PathBuf,
    pub plugin_logs: PathBuf,
    pub audit_log: PathBuf,
    pub recordings: PathBuf,
    pub journal: PathBuf,
    pub logd_store: PathBuf,
    /// The installed product. Measured so the footprint report is honest about
    /// what the agent costs on a card, and never touched.
    pub installed: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Roots {
            apt_archives: PathBuf::from("/var/cache/apt/archives"),
            apt_lists: PathBuf::from("/var/lib/apt/lists"),
            plugin_logs: PathBuf::from("/var/log/ados/plugins"),
            // The audit trail sits beside the agent's other persistent data, not
            // under /var/log — a detail worth stating because the tmpfiles
            // retention drop-in ages `/var/log/ados`, which is a different place.
            audit_log: PathBuf::from("/var/ados/audit.jsonl"),
            recordings: reclaim::path_from_env("ADOS_RECORDINGS_DIR", "/var/ados/recordings"),
            journal: PathBuf::from("/var/log/journal"),
            logd_store: reclaim::path_from_env("ADOS_LOGD_DIR", "/var/ados/logd"),
            installed: PathBuf::from("/opt/ados"),
        }
    }
}

/// What the agent is occupying right now, per category, plus the installed
/// product measured separately.
///
/// The live store's own footprint is its database and WAL, not the whole
/// directory: the quarantined corpses live beside them and are their own
/// category, because "the store is large" and "a store tore once and its corpse
/// is still here" call for completely different responses.
pub fn measure_footprint(roots: &Roots) -> budget::Footprint {
    let quarantined = reclaim::quarantine_bytes(&roots.logd_store);
    let store_dir_total = reclaim::tree_bytes(&roots.logd_store);
    budget::Footprint {
        log_store: store_dir_total.saturating_sub(quarantined),
        quarantined_stores: quarantined,
        recordings: reclaim::tree_bytes(&roots.recordings),
        plugin_logs: reclaim::tree_bytes(&roots.plugin_logs),
        audit_log: std::fs::metadata(&roots.audit_log)
            .map(|m| m.len())
            .unwrap_or(0),
        journal: reclaim::tree_bytes(&roots.journal),
        apt: reclaim::apt_archive_bytes(&roots.apt_archives)
            .saturating_add(reclaim::dir_bytes(&roots.apt_lists)),
        installed: reclaim::tree_bytes(&roots.installed),
    }
}

/// The hourly disk janitor. Owns only its own cadence; the supervisor drives it
/// from the monitor pass like the other reconcilers.
///
/// Inert off Linux: the sweep reaches real on-box paths and the pressure signal
/// is a `statvfs`, neither of which belongs on a developer's machine. The
/// decision core and every reclaim operation still compile and unit-test there.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Janitor {
    last_tick: Option<Instant>,
    events: EventEmitter,
}

impl Janitor {
    pub fn new(events: EventEmitter) -> Self {
        Janitor {
            last_tick: None,
            events,
        }
    }

    /// One reconcile. Cheap when nothing is over its cap: a handful of directory
    /// reads and two `statvfs` calls.
    pub async fn tick(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let cfg = read_config();
            if !cfg.enabled {
                return;
            }
            let now = Instant::now();
            let due = match self.last_tick {
                None => true,
                Some(last) => now.duration_since(last) >= cfg.interval,
            };
            if !due {
                return;
            }
            self.last_tick = Some(now);

            let roots = Roots::default();
            let before = reclaim::free_pct(std::path::Path::new(PRESSURE_PATH));
            // The recordings volume's own headroom, which may be a different
            // filesystem from the one the rung is chosen on.
            let record_free = reclaim::free_bytes(&roots.recordings);
            let (rung, freed) = sweep(&roots, &cfg, before, record_free).await;
            let after = reclaim::free_pct(std::path::Path::new(PRESSURE_PATH));

            // Measured AFTER the sweep, so the sidecar answers "what is left to
            // give" rather than "what was there before this pass took some".
            let left = reclaimable(&roots, &cfg);
            let footprint = measure_footprint(&roots);
            self.emit(rung, &freed, &footprint, &cfg, before, after);
            write_sidecar(rung, &freed, &left, &footprint, &cfg, after);
        }
    }

    /// Record the pass. Emitted even when the pass freed nothing, because "the
    /// janitor ran and there was nothing to do" and "the janitor did not run"
    /// are different facts and only one of them is fine.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn emit(
        &self,
        rung: Rung,
        freed: &Reclaimed,
        footprint: &budget::Footprint,
        cfg: &JanitorConfig,
        before: Option<f64>,
        after: Option<f64>,
    ) {
        let mut detail = Fields::new();
        detail.insert("rung".to_string(), Value::from(rung.as_str()));
        detail.insert(
            "reclaimed_bytes".to_string(),
            Value::from(freed.total() as i64),
        );
        for (name, bytes) in freed.pairs() {
            detail.insert(name.to_string(), Value::from(bytes as i64));
        }
        // The footprint the pass left behind, against the budget it is held to.
        // Space is what breaks these nodes, so the event carries the figure the
        // decision was made on rather than only what came off the disk.
        detail.insert(
            "footprint_bytes".to_string(),
            Value::from(footprint.budgeted_total() as i64),
        );
        detail.insert(
            "budget_bytes".to_string(),
            Value::from(cfg.budget_bytes as i64),
        );
        // Categories still over their share after the pass — the ones a floor
        // held above the cap. Named rather than left implicit, because "the
        // janitor ran and this is still over" is the state an operator has to
        // act on themselves.
        let over: Vec<String> = budget::over_cap_categories(footprint, &cfg.caps)
            .into_iter()
            .map(|(c, bytes)| format!("{}:{}", c.as_str(), bytes))
            .collect();
        if !over.is_empty() {
            detail.insert("over_cap".to_string(), Value::from(over.join(",")));
        }
        // Absent free space stays absent rather than becoming a zero, which
        // would read as "the disk is full" on a box that merely could not
        // measure it.
        if let Some(pct) = before {
            detail.insert("free_pct_before".to_string(), Value::from(pct));
        }
        if let Some(pct) = after {
            detail.insert("free_pct_after".to_string(), Value::from(pct));
        }
        // Critical is loud: at this point the box is close to the state that
        // ends in a card that will not boot, and that has to be in the record
        // before it gets there.
        let level = match rung {
            Rung::Critical => Level::Warn,
            _ => Level::Info,
        };
        self.events.emit(JANITOR_EVENT_KIND, level, detail);
    }
}

/// Mirror the last pass so the storage diagnostic can report it.
#[cfg(target_os = "linux")]
fn write_sidecar(
    rung: Rung,
    freed: &Reclaimed,
    reclaimable_now: &Reclaimed,
    footprint: &budget::Footprint,
    cfg: &JanitorConfig,
    free_pct: Option<f64>,
) {
    #[derive(serde::Serialize)]
    struct Snap<'a> {
        version: u16,
        rung: &'a str,
        reclaimed_bytes: u64,
        reclaimed: &'a Reclaimed,
        /// What a Pressure-rung sweep would still free, measured after this
        /// pass. The storage diagnostic reads this to answer "what is left".
        reclaimable_bytes: u64,
        reclaimable: &'a Reclaimed,
        /// What the agent occupies now, per category, against the budget it is
        /// held to. The headline the storage diagnostic leads with.
        footprint_bytes: u64,
        footprint: &'a budget::Footprint,
        budget_bytes: u64,
        caps: std::collections::BTreeMap<&'static str, u64>,
        over_cap: std::collections::BTreeMap<&'static str, u64>,
        /// The installed product, reported and EXCLUDED from the budget.
        installed_bytes: u64,
        free_pct: Option<f64>,
        ran_at_unix: i64,
        updated_at_unix: i64,
    }
    let now = now_unix();
    let snap = Snap {
        version: SIDECAR_VERSION,
        rung: rung.as_str(),
        reclaimed_bytes: freed.total(),
        reclaimed: freed,
        reclaimable_bytes: reclaimable_now.total(),
        reclaimable: reclaimable_now,
        footprint_bytes: footprint.budgeted_total(),
        footprint,
        budget_bytes: cfg.budget_bytes,
        caps: budget::Category::ALL
            .iter()
            .map(|c| (c.as_str(), cfg.caps.get(*c)))
            .collect(),
        over_cap: budget::over_cap_categories(footprint, &cfg.caps)
            .into_iter()
            .map(|(c, bytes)| (c.as_str(), bytes))
            .collect(),
        installed_bytes: footprint.installed,
        free_pct,
        ran_at_unix: now,
        updated_at_unix: now,
    };
    if let Err(e) = write_json_atomic(std::path::Path::new(SIDECAR_PATH), &snap, 0o644) {
        tracing::debug!(error = %e, "janitor sidecar write failed");
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
use ados_protocol::sidecar::write_json_atomic;

#[cfg(test)]
mod tests;
