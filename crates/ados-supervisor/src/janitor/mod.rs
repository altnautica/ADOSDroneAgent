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

/// Parse `storage.janitor`. Absent or malformed → enabled defaults, matching
/// every other reconciler here: a config the box cannot read must not be the
/// reason a safety net is off.
pub fn read_config_from(text: &str) -> JanitorConfig {
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        storage: Storage,
    }
    #[derive(serde::Deserialize, Default)]
    struct Storage {
        #[serde(default)]
        janitor: Option<Janitor>,
    }
    #[derive(serde::Deserialize)]
    struct Janitor {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        interval_s: Option<u64>,
        #[serde(default)]
        pressure_free_pct: Option<f64>,
        #[serde(default)]
        critical_free_pct: Option<f64>,
        #[serde(default)]
        plugin_log_max_mb: Option<u64>,
        #[serde(default)]
        plugin_log_keep_mb: Option<u64>,
        #[serde(default)]
        audit_max_mb: Option<u64>,
        #[serde(default)]
        audit_keep_mb: Option<u64>,
        #[serde(default)]
        recording_retention_days: Option<u64>,
        #[serde(default)]
        recording_pressure_retention_days: Option<u64>,
        #[serde(default)]
        recording_keep_newest: Option<usize>,
        #[serde(default)]
        journal_pressure_mb: Option<u64>,
        #[serde(default)]
        journal_floor_mb: Option<u64>,
        #[serde(default)]
        quarantine_keep_newest: Option<usize>,
        #[serde(default)]
        budget_mb: Option<u64>,
        #[serde(default)]
        caps: Option<RawCaps>,
    }
    #[derive(serde::Deserialize)]
    struct RawCaps {
        #[serde(default)]
        log_store_mb: Option<u64>,
        #[serde(default)]
        quarantined_stores_mb: Option<u64>,
        #[serde(default)]
        recordings_mb: Option<u64>,
        #[serde(default)]
        plugin_logs_mb: Option<u64>,
        #[serde(default)]
        audit_log_mb: Option<u64>,
        #[serde(default)]
        journal_mb: Option<u64>,
        #[serde(default)]
        apt_mb: Option<u64>,
    }
    fn default_true() -> bool {
        true
    }
    const MB: u64 = 1024 * 1024;

    let d = JanitorConfig::default();
    let Ok(raw) = serde_norway::from_str::<Raw>(text) else {
        return d;
    };
    let Some(j) = raw.storage.janitor else {
        return d;
    };
    JanitorConfig {
        enabled: j.enabled,
        interval: Duration::from_secs(j.interval_s.unwrap_or(plan::DEFAULT_INTERVAL_S).max(60)),
        pressure_free_pct: j.pressure_free_pct.unwrap_or(d.pressure_free_pct),
        critical_free_pct: j.critical_free_pct.unwrap_or(d.critical_free_pct),
        plugin_log_max_bytes: j
            .plugin_log_max_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.plugin_log_max_bytes),
        plugin_log_keep_bytes: j
            .plugin_log_keep_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.plugin_log_keep_bytes),
        audit_max_bytes: j
            .audit_max_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.audit_max_bytes),
        audit_keep_bytes: j
            .audit_keep_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.audit_keep_bytes),
        recording_retention: j
            .recording_retention_days
            .map(|days| Duration::from_secs(days.saturating_mul(86_400)))
            .unwrap_or(d.recording_retention),
        recording_pressure_retention: j
            .recording_pressure_retention_days
            .map(|days| Duration::from_secs(days.saturating_mul(86_400)))
            .unwrap_or(d.recording_pressure_retention),
        recording_keep_newest: j.recording_keep_newest.unwrap_or(d.recording_keep_newest),
        journal_pressure_bytes: j
            .journal_pressure_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.journal_pressure_bytes),
        journal_floor_bytes: j
            .journal_floor_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.journal_floor_bytes),
        quarantine_keep_newest: j.quarantine_keep_newest.unwrap_or(d.quarantine_keep_newest),
        budget_bytes: j
            .budget_mb
            .map(|m| m.saturating_mul(MB))
            .unwrap_or(d.budget_bytes),
        caps: match j.caps {
            None => d.caps,
            Some(c) => budget::Caps {
                log_store: c.log_store_mb.map(|m| m * MB).unwrap_or(d.caps.log_store),
                quarantined_stores: c
                    .quarantined_stores_mb
                    .map(|m| m * MB)
                    .unwrap_or(d.caps.quarantined_stores),
                recordings: c.recordings_mb.map(|m| m * MB).unwrap_or(d.caps.recordings),
                plugin_logs: c
                    .plugin_logs_mb
                    .map(|m| m * MB)
                    .unwrap_or(d.caps.plugin_logs),
                audit_log: c.audit_log_mb.map(|m| m * MB).unwrap_or(d.caps.audit_log),
                journal: c.journal_mb.map(|m| m * MB).unwrap_or(d.caps.journal),
                apt: c.apt_mb.map(|m| m * MB).unwrap_or(d.caps.apt),
            },
        },
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

/// Enforce the per-category caps, oldest-first within each category.
///
/// This runs regardless of the rung and regardless of the total, which is the
/// whole point of having caps as well as a budget: a category over its share is
/// trimmed even on a box with plenty of room, so recordings cannot quietly
/// occupy the store's allowance on a node that happens not to be logging.
///
/// The append-only categories (plugin logs, the audit trail) are trimmed to
/// their per-file keep floor rather than deleted, because there is only ever one
/// file per plugin and deleting it would take the live log out from under a
/// writer that has it open.
async fn enforce_caps(
    roots: &Roots,
    cfg: &JanitorConfig,
    footprint: &budget::Footprint,
) -> Reclaimed {
    use budget::Category;
    let mut out = Reclaimed::default();
    for (category, _over) in budget::over_cap_categories(footprint, &cfg.caps) {
        let cap = cfg.caps.get(category);
        match category {
            Category::Recordings => {
                out.recordings =
                    out.recordings
                        .saturating_add(reclaim::reclaim_to_cap_oldest_first(
                            &roots.recordings,
                            |n| n.ends_with(".mp4") || n.ends_with(".mkv") || n.ends_with(".ts"),
                            cap,
                            cfg.recording_keep_newest.max(1),
                        ));
            }
            Category::QuarantinedStores => {
                out.quarantined_stores =
                    out.quarantined_stores
                        .saturating_add(reclaim::reclaim_to_cap_oldest_first(
                            &roots.logd_store,
                            |n| n.starts_with("logs.db.corrupt-"),
                            cap,
                            // The floor that outranks the cap. A single corpse
                            // larger than the whole quarantine share is the case
                            // that actually happens, and it stays.
                            cfg.quarantine_keep_newest.max(1),
                        ));
            }
            Category::Apt => {
                out.apt_archives = out
                    .apt_archives
                    .saturating_add(reclaim::reclaim_apt_archives(&roots.apt_archives));
                out.apt_lists = out
                    .apt_lists
                    .saturating_add(reclaim::reclaim_apt_lists(&roots.apt_lists));
            }
            Category::Journal => {
                let target = cap.max(cfg.journal_floor_bytes);
                out.journal = out
                    .journal
                    .saturating_add(reclaim::vacuum_journal(&roots.journal, target).await);
            }
            Category::PluginLogs => {
                out.plugin_logs = out.plugin_logs.saturating_add(reclaim::trim_plugin_logs(
                    &roots.plugin_logs,
                    cfg.plugin_log_keep_bytes,
                    cfg.plugin_log_keep_bytes,
                ));
            }
            Category::AuditLog => {
                out.audit_log = out.audit_log.saturating_add(reclaim::trim_append_only(
                    &roots.audit_log,
                    cfg.audit_keep_bytes,
                    cfg.audit_keep_bytes,
                ));
            }
            // The store bounds itself: its own retention evicts to a low-water
            // mark, and the janitor deleting rows out from under the single
            // writer would corrupt exactly the file this whole effort is trying
            // to keep intact. Over-cap here is reported, not acted on.
            Category::LogStore => {}
        }
    }
    out
}

/// Run one pass over `roots` under `cfg`, returning what it freed and the rung
/// it ran at. Separated from the reconciler so a test drives a whole sweep
/// against a temporary tree with no timer, no config file and no clock.
pub async fn sweep(roots: &Roots, cfg: &JanitorConfig, free_pct: Option<f64>) -> (Rung, Reclaimed) {
    let footprint = measure_footprint(roots);
    let rung = cfg.rung_for(free_pct, &footprint);
    let p = cfg.plan(rung);

    // Unambiguous waste, every rung.
    let apt_archives = reclaim::reclaim_apt_archives(&roots.apt_archives);
    let plugin_logs = reclaim::trim_plugin_logs(
        &roots.plugin_logs,
        cfg.plugin_log_max_bytes,
        cfg.plugin_log_keep_bytes,
    );
    let audit_log =
        reclaim::trim_append_only(&roots.audit_log, cfg.audit_max_bytes, cfg.audit_keep_bytes);

    let cutoff = now_unix().saturating_sub(p.recording_cutoff.as_secs() as i64);
    let recordings =
        reclaim::reclaim_recordings(&roots.recordings, cutoff, p.recording_keep_newest);

    // Things with residual value, only once space is short.
    let apt_lists = if p.apt_lists {
        reclaim::reclaim_apt_lists(&roots.apt_lists)
    } else {
        0
    };
    let journal = match p.journal_target_bytes {
        Some(target) => reclaim::vacuum_journal(&roots.journal, target).await,
        None => 0,
    };
    let quarantined_stores = if p.prune_quarantines {
        reclaim::prune_quarantines(&roots.logd_store, p.keep_quarantines)
    } else {
        0
    };

    let mut freed = Reclaimed {
        apt_archives,
        apt_lists,
        plugin_logs,
        audit_log,
        recordings,
        journal,
        quarantined_stores,
    };

    // Then the caps. Re-measure first: the sweep above has already taken things
    // off the disk, and enforcing a cap against a stale figure would delete more
    // than the cap actually calls for.
    let after_sweep = measure_footprint(roots);
    freed.add(&enforce_caps(roots, cfg, &after_sweep).await);

    (rung, freed)
}

/// What a full sweep at the **Pressure** rung would free right now, measured
/// without removing anything.
///
/// This is the number an operator wants when they ask "why is the card full and
/// what is left to give". Reporting the raw footprint of each directory instead
/// would overstate it badly: most of a plugin log is not reclaimable, because
/// its tail is a floor, and most recordings are not reclaimable, because the
/// newest ones survive at any age.
///
/// Pressure, not Critical, because the two reclaim the same categories —
/// Critical differs only in being loud — so a Pressure figure is the honest
/// ceiling rather than a larger number that no rung would actually reach.
pub fn reclaimable(roots: &Roots, cfg: &JanitorConfig) -> Reclaimed {
    let p = cfg.plan(Rung::Pressure);

    let apt_archives = reclaim::apt_archive_bytes(&roots.apt_archives);
    let apt_lists = reclaim::dir_bytes(&roots.apt_lists);
    let plugin_logs = reclaim::plugin_log_trimmable(
        &roots.plugin_logs,
        cfg.plugin_log_max_bytes,
        cfg.plugin_log_keep_bytes,
    );
    let audit_log = std::fs::metadata(&roots.audit_log)
        .map(|m| m.len())
        .ok()
        .and_then(|len| plan::trim_from(len, cfg.audit_max_bytes, cfg.audit_keep_bytes))
        .unwrap_or(0);

    let cutoff = now_unix().saturating_sub(p.recording_cutoff.as_secs() as i64);
    let recordings =
        reclaim::recording_reclaimable_bytes(&roots.recordings, cutoff, p.recording_keep_newest);

    let journal = p
        .journal_target_bytes
        .map(|target| reclaim::dir_bytes(&roots.journal).saturating_sub(target))
        .unwrap_or(0);
    let quarantined_stores =
        reclaim::quarantine_reclaimable_bytes(&roots.logd_store, p.keep_quarantines);

    Reclaimed {
        apt_archives,
        apt_lists,
        plugin_logs,
        audit_log,
        recordings,
        journal,
        quarantined_stores,
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
    /// reads and one `statvfs`.
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
            let (rung, freed) = sweep(&roots, &cfg, before).await;
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
fn write_json_atomic<T: serde::Serialize>(
    path: &std::path::Path,
    value: &T,
    mode: u32,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn an_absent_section_is_enabled_with_defaults() {
        let cfg = read_config_from("agent:\n  name: x\n");
        assert!(cfg.enabled);
        assert_eq!(cfg, JanitorConfig::default());
    }

    #[test]
    fn a_malformed_config_leaves_the_janitor_on() {
        assert!(read_config_from(": : : not yaml").enabled);
    }

    #[test]
    fn tunables_are_read_and_the_cadence_is_floored() {
        let cfg = read_config_from(
            "storage:\n  janitor:\n    enabled: false\n    interval_s: 5\n    \
             plugin_log_max_mb: 8\n    recording_retention_days: 30\n    \
             quarantine_keep_newest: 2\n",
        );
        assert!(!cfg.enabled);
        // A five-second sweep would spend more disk logging than it recovers.
        assert_eq!(cfg.interval, Duration::from_secs(60));
        assert_eq!(cfg.plugin_log_max_bytes, 8 * 1024 * 1024);
        assert_eq!(cfg.recording_retention, Duration::from_secs(30 * 86_400));
        assert_eq!(cfg.quarantine_keep_newest, 2);
    }

    /// Lay down one of everything the janitor sweeps, and return the roots.
    fn populate(base: &std::path::Path) -> Roots {
        let archives = base.join("apt-archives");
        let lists = base.join("apt-lists");
        let plugins = base.join("plugin-logs");
        let recordings = base.join("recordings");
        let store = base.join("logd");
        for d in [&archives, &lists, &plugins, &recordings, &store] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(archives.join("libfoo_1.0_arm64.deb"), vec![0u8; 4_000]).unwrap();
        fs::write(
            lists.join("deb.example.org_dists_stable_Packages"),
            vec![0u8; 5_000],
        )
        .unwrap();
        fs::write(lists.join("lock"), b"").unwrap();
        let long: String = (0..200).map(|i| format!("{i:0>99}\n")).collect();
        fs::write(plugins.join("com-example-a.log"), &long).unwrap();
        fs::write(base.join("audit.jsonl"), &long).unwrap();
        for i in 0..4 {
            fs::write(recordings.join(format!("flight-{i}.mp4")), vec![0u8; 1_000]).unwrap();
        }
        for i in 0..3 {
            fs::write(store.join(format!("logs.db.corrupt-{i}")), vec![0u8; 2_000]).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1100));
        }
        fs::write(store.join("logs.db"), vec![0u8; 900]).unwrap();

        Roots {
            apt_archives: archives,
            apt_lists: lists,
            plugin_logs: plugins,
            audit_log: base.join("audit.jsonl"),
            recordings,
            journal: base.join("no-journal-here"),
            logd_store: store,
            installed: base.join("no-opt-ados-here"),
        }
    }

    fn tight_config() -> JanitorConfig {
        JanitorConfig {
            plugin_log_max_bytes: 4_000,
            plugin_log_keep_bytes: 1_000,
            audit_max_bytes: 4_000,
            audit_keep_bytes: 1_000,
            // Every recording is "old" for the purposes of the sweep.
            recording_retention: Duration::from_secs(0),
            recording_pressure_retention: Duration::from_secs(0),
            recording_keep_newest: 2,
            ..JanitorConfig::default()
        }
    }

    #[tokio::test]
    async fn a_routine_sweep_leaves_the_things_with_value_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let cfg = tight_config();

        // Plenty of free space.
        let (rung, freed) = sweep(&roots, &cfg, Some(80.0)).await;
        assert_eq!(rung, Rung::Routine);
        assert_eq!(freed.apt_archives, 4_000);
        assert!(freed.plugin_logs > 0);
        assert!(freed.audit_log > 0);
        assert_eq!(
            freed.recordings, 2_000,
            "four recordings, the newest two kept"
        );
        // The escalated categories did not run.
        assert_eq!(freed.apt_lists, 0);
        assert_eq!(freed.quarantined_stores, 0);
        assert!(
            roots
                .apt_lists
                .join("deb.example.org_dists_stable_Packages")
                .exists(),
            "the package index survives a routine sweep"
        );
        assert_eq!(
            fs::read_dir(&roots.logd_store).unwrap().count(),
            4,
            "three quarantined stores plus the live one survive a routine sweep"
        );
    }

    #[tokio::test]
    async fn nothing_is_reclaimed_without_a_per_category_number() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let (_, freed) = sweep(&roots, &tight_config(), Some(5.0)).await;

        // The invariant: whatever came off the disk is attributed to a named
        // category, so the event can carry it. A category that freed bytes with
        // no accounting line would show up here as a total that is larger than
        // the parts.
        let summed: u64 = freed.pairs().iter().map(|(_, b)| *b).sum();
        assert_eq!(summed, freed.total());
        assert!(freed.total() > 0, "this fixture has plenty to reclaim");
        for (name, bytes) in freed.pairs() {
            if bytes > 0 {
                assert!(!name.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn the_critical_rung_still_leaves_the_newest_quarantined_store() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let (rung, freed) = sweep(&roots, &tight_config(), Some(1.0)).await;

        assert_eq!(rung, Rung::Critical);
        assert_eq!(freed.quarantined_stores, 4_000, "two of three corpses");
        assert!(
            roots.logd_store.join("logs.db.corrupt-2").exists(),
            "the most recent corruption is the evidence of the last failure"
        );
        assert!(
            roots.logd_store.join("logs.db").exists(),
            "the live store is never a janitor target"
        );
        // The floors held across the board.
        assert!(
            fs::metadata(roots.audit_log).unwrap().len() > 0,
            "the audit trail keeps its tail"
        );
        assert_eq!(
            fs::read_dir(&roots.recordings).unwrap().count(),
            2,
            "the newest recordings survive however old they are"
        );
    }

    #[tokio::test]
    async fn a_second_sweep_reclaims_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let cfg = tight_config();

        let (_, first) = sweep(&roots, &cfg, Some(1.0)).await;
        assert!(first.total() > 0);
        let (_, second) = sweep(&roots, &cfg, Some(1.0)).await;
        assert_eq!(
            second.total(),
            0,
            "a janitor that keeps finding work on an unchanged disk is deleting \
             something it should not: {second:?}"
        );
    }

    #[tokio::test]
    async fn what_is_left_to_give_is_measured_not_the_raw_footprint() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let cfg = tight_config();

        let before = reclaimable(&roots, &cfg);
        // Everything the fixture holds is visible as something a pressure sweep
        // could take.
        assert_eq!(before.apt_archives, 4_000);
        assert_eq!(
            before.apt_lists, 5_000,
            "the index is reclaimable under pressure"
        );
        assert_eq!(before.quarantined_stores, 4_000, "two of three corpses");
        assert!(before.plugin_logs > 0);
        assert!(before.audit_log > 0);

        // A log's reclaimable figure is what a trim would free, never its whole
        // size — the tail is a floor and reporting it as reclaimable would
        // promise space the janitor will not take.
        let log_len = fs::metadata(roots.plugin_logs.join("com-example-a.log"))
            .unwrap()
            .len();
        assert!(
            before.plugin_logs < log_len,
            "reported {} for a {log_len}-byte log; the tail must not be counted",
            before.plugin_logs
        );

        // After a full pass there is nothing left to give.
        let (_, freed) = sweep(&roots, &cfg, Some(1.0)).await;
        assert!(freed.total() > 0);
        assert_eq!(
            reclaimable(&roots, &cfg).total(),
            0,
            "a swept box must not keep advertising space it cannot free"
        );
    }

    // --- the footprint budget ------------------------------------------------

    #[tokio::test]
    async fn a_category_over_its_cap_is_trimmed_on_a_box_with_plenty_of_room() {
        // The reason caps exist alongside a total budget: recordings must not
        // take the store's share just because the store is idle. Free space is
        // fine, the total is far under budget, and this still trims.
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let cfg = JanitorConfig {
            // Keep the age-based sweep inert so this test isolates the cap.
            recording_retention: Duration::from_secs(86_400 * 3650),
            recording_pressure_retention: Duration::from_secs(86_400 * 3650),
            recording_keep_newest: 1,
            caps: budget::Caps {
                recordings: 2_500,
                ..budget::Caps::default()
            },
            ..JanitorConfig::default()
        };

        // Four 1 000-byte recordings against a 2 500-byte cap.
        let (rung, freed) = sweep(&roots, &cfg, Some(90.0)).await;
        assert_eq!(rung, Rung::Routine, "plenty of free space, tiny footprint");
        assert_eq!(
            freed.recordings, 2_000,
            "trimmed down to the cap, oldest first"
        );
        assert_eq!(
            measure_footprint(&roots).recordings,
            2_000,
            "left at or under the cap"
        );
    }

    #[tokio::test]
    async fn the_cap_never_takes_the_newest_even_when_it_alone_exceeds_it() {
        // The floor outranks the cap. This is not hypothetical: a single 1 GB
        // quarantined store against a 400 MB quarantine share is what the drone
        // was actually holding.
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let cfg = JanitorConfig {
            caps: budget::Caps {
                // Far below the size of even one corpse in the fixture.
                quarantined_stores: 100,
                ..budget::Caps::default()
            },
            ..JanitorConfig::default()
        };

        let (_, freed) = sweep(&roots, &cfg, Some(90.0)).await;
        assert_eq!(freed.quarantined_stores, 4_000, "two of the three corpses");
        assert!(
            roots.logd_store.join("logs.db.corrupt-2").exists(),
            "the newest quarantined store survives its own cap"
        );
        // And the residue is visible rather than silently accepted.
        let after = measure_footprint(&roots);
        assert!(
            budget::over_cap(&after, &cfg.caps, budget::Category::QuarantinedStores) > 0,
            "a category a floor holds above its cap must still read as over"
        );
    }

    #[tokio::test]
    async fn a_footprint_over_budget_escalates_the_rung_on_a_mostly_empty_card() {
        // The failure a percentage trigger cannot see: a big card barely used,
        // and an agent whose own footprint has run away. Free space says 97%,
        // and the pass still escalates.
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let cfg = JanitorConfig {
            budget_bytes: 1_000,
            ..JanitorConfig::default()
        };
        let footprint = measure_footprint(&roots);
        assert!(footprint.budgeted_total() > 1_000);
        assert_eq!(cfg.rung_for(Some(97.0), &footprint), Rung::Critical);
    }

    #[tokio::test]
    async fn the_installed_product_is_measured_and_never_touched() {
        let tmp = tempfile::tempdir().unwrap();
        let mut roots = populate(tmp.path());
        let installed = tmp.path().join("opt-ados");
        fs::create_dir_all(installed.join("venv/bin")).unwrap();
        fs::write(installed.join("venv/bin/python"), vec![0u8; 9_000]).unwrap();
        roots.installed = installed.clone();

        let before = measure_footprint(&roots);
        assert_eq!(before.installed, 9_000, "reported");
        assert!(
            !before.pairs().iter().any(|(n, _)| *n == "installed"),
            "and excluded from the budgeted categories"
        );

        let cfg = JanitorConfig {
            budget_bytes: 1,
            caps: budget::Caps {
                recordings: 0,
                quarantined_stores: 0,
                ..budget::Caps::default()
            },
            ..JanitorConfig::default()
        };
        // The most aggressive pass this config can produce.
        let (rung, _) = sweep(&roots, &cfg, Some(0.5)).await;
        assert_eq!(rung, Rung::Critical);
        assert_eq!(
            measure_footprint(&roots).installed,
            9_000,
            "the installed product is never reclaimed, at any rung"
        );
    }

    #[tokio::test]
    async fn cap_enforcement_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let cfg = JanitorConfig {
            recording_retention: Duration::from_secs(86_400 * 3650),
            recording_pressure_retention: Duration::from_secs(86_400 * 3650),
            recording_keep_newest: 1,
            caps: budget::Caps {
                recordings: 2_500,
                quarantined_stores: 2_500,
                ..budget::Caps::default()
            },
            ..JanitorConfig::default()
        };
        let (_, first) = sweep(&roots, &cfg, Some(90.0)).await;
        assert!(first.total() > 0);
        let (_, second) = sweep(&roots, &cfg, Some(90.0)).await;
        assert_eq!(
            second.total(),
            0,
            "a settled box reclaims nothing: {second:?}"
        );
    }

    #[test]
    fn the_budget_and_caps_are_configurable() {
        let cfg = read_config_from(
            "storage:\n  janitor:\n    budget_mb: 2048\n    caps:\n      recordings_mb: 512\n      journal_mb: 64\n",
        );
        assert_eq!(cfg.budget_bytes, 2048 * 1024 * 1024);
        assert_eq!(cfg.caps.recordings, 512 * 1024 * 1024);
        assert_eq!(cfg.caps.journal, 64 * 1024 * 1024);
        // Unspecified caps keep their defaults rather than collapsing to zero,
        // which would make one configured cap silently delete everything else.
        assert_eq!(cfg.caps.apt, budget::DEFAULT_CAP_APT);
        assert_eq!(cfg.caps.log_store, budget::DEFAULT_CAP_LOG_STORE);
    }

    #[tokio::test]
    async fn an_unmeasured_disk_sweeps_routinely_not_critically() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = populate(tmp.path());
        let (rung, freed) = sweep(&roots, &tight_config(), None).await;
        assert_eq!(rung, Rung::Routine);
        assert_eq!(freed.quarantined_stores, 0);
        assert_eq!(freed.apt_lists, 0);
    }
}
