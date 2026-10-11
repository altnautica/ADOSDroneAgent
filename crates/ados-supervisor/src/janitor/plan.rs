//! What the janitor is allowed to reclaim, and how much of it must survive.
//!
//! Pure. Nothing here touches a filesystem, so every rule — which rung a free
//! ratio selects, how far a retention window tightens under pressure, what stays
//! behind after the most aggressive pass — is decided in a unit test rather than
//! on a card that is already full.
//!
//! The organising idea is that **every category has a floor**. A janitor with no
//! floor is a data-loss bug waiting for the one night the disk fills, and the
//! things on this box that fill a disk are also the things an RCA reads: the
//! quarantined copy of a store that tore, the tail of a plugin log, the audit
//! trail, the recordings someone deliberately made. Reclaiming those to zero
//! trades a full disk for no evidence, which is the worse of the two.

use std::time::Duration;

/// How hard the janitor is allowed to push, chosen from free space on the
/// filesystem holding `/var`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rung {
    /// The steady state: reclaim what is unambiguously waste.
    Routine,
    /// Space is getting short: also give up the package index, older
    /// quarantined stores, journal history, and older recordings.
    Pressure,
    /// Space is nearly gone: everything Pressure does, plus the loud event, so
    /// the reason the box is about to stop working is recorded before it does.
    Critical,
}

impl Rung {
    /// The wire/rendering name.
    pub fn as_str(self) -> &'static str {
        match self {
            Rung::Routine => "routine",
            Rung::Pressure => "pressure",
            Rung::Critical => "critical",
        }
    }
}

/// Default reconcile cadence. Hourly: the things this reclaims accumulate over
/// days, and a tighter loop would spend more disk writing about the sweep than
/// the sweep recovers.
pub const DEFAULT_INTERVAL_S: u64 = 3_600;
/// Below this fraction of free space the Pressure rung engages.
pub const DEFAULT_PRESSURE_FREE_PCT: f64 = 20.0;
/// Below this fraction of free space the Critical rung engages.
pub const DEFAULT_CRITICAL_FREE_PCT: f64 = 10.0;
/// A plugin log larger than this is trimmed. systemd cannot cap an `append:`
/// destination and nothing rotates these, so this reconciler is the only thing
/// bounding them. Kept in step with `PLUGIN_LOG_MAX_BYTES` in the plugin host,
/// which declares the same figure beside the unit that opens the file; the two
/// crates do not depend on each other, so each states the number and names the
/// other.
pub const DEFAULT_PLUGIN_LOG_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// How much of a trimmed plugin log survives — the tail, which is the part that
/// explains what the plugin was doing when it went wrong.
pub const DEFAULT_PLUGIN_LOG_KEEP_BYTES: u64 = 8 * 1024 * 1024;
/// The audit trail's trim threshold.
pub const DEFAULT_AUDIT_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// How much of the audit trail survives a trim.
pub const DEFAULT_AUDIT_KEEP_BYTES: u64 = 4 * 1024 * 1024;
/// Steady-state recording retention.
pub const DEFAULT_RECORDING_RETENTION_S: u64 = 90 * 86_400;
/// Recording retention once the disk is under pressure.
pub const DEFAULT_RECORDING_PRESSURE_RETENTION_S: u64 = 14 * 86_400;
/// Recordings that survive regardless of age, newest first.
pub const DEFAULT_RECORDING_KEEP_NEWEST: usize = 3;
/// How recently a capture file must have been written for the janitor to treat
/// it as the one an in-flight capture is appending to.
///
/// Two segment durations of slack: mediamtx closes a segment every 60s, so a
/// window shorter than that could judge a live path idle in the gap between two
/// segments, and a much longer one would protect segments nothing is writing.
pub const RECORDING_IN_FLIGHT_WINDOW_S: i64 = 120;
/// The free space the recordings volume must keep, below which recordings are
/// reclaimed oldest-first regardless of age or cap.
///
/// **Time-based retention cannot do this job.** mediamtx's own `recordDeleteAfter`
/// is a 24h window, and a high-bitrate day fills the volume long before 24h
/// elapse — at which point the recorder stops dead mid-flight, which is the
/// failure this floor exists to prevent. 1 GiB is roughly a minute of 1080p at
/// 15 Mbps: enough headroom for the recorder to keep writing while a sweep
/// reclaims, and small enough that it is not itself a large reservation.
///
/// A constant, not a tunable. This is the point below which recording stops
/// working; an operator lowering it does not gain capacity, they only move the
/// cliff closer to the edge.
pub const RECORD_MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;
/// Journal size the Pressure rung vacuums down to.
pub const DEFAULT_JOURNAL_PRESSURE_BYTES: u64 = 128 * 1024 * 1024;
/// The journal is never vacuumed below this, at any rung. A kernel oops trace
/// that survived the reboot is the whole reason the journal is persistent.
pub const DEFAULT_JOURNAL_FLOOR_BYTES: u64 = 64 * 1024 * 1024;
/// Quarantined stores that survive, newest first. One is a hard floor enforced
/// below; this is the configurable amount above it.
pub const DEFAULT_QUARANTINE_KEEP_NEWEST: usize = 1;

/// The janitor's tunables, read from `storage.janitor`. Default-ON.
#[derive(Debug, Clone, PartialEq)]
pub struct JanitorConfig {
    pub enabled: bool,
    pub interval: Duration,
    pub pressure_free_pct: f64,
    pub critical_free_pct: f64,
    pub plugin_log_max_bytes: u64,
    pub plugin_log_keep_bytes: u64,
    pub audit_max_bytes: u64,
    pub audit_keep_bytes: u64,
    pub recording_retention: Duration,
    pub recording_pressure_retention: Duration,
    pub recording_keep_newest: usize,
    pub journal_pressure_bytes: u64,
    pub journal_floor_bytes: u64,
    pub quarantine_keep_newest: usize,
    /// Total the agent may occupy on the card. The PRIMARY trigger.
    pub budget_bytes: u64,
    /// Per-category shares of that budget.
    pub caps: super::budget::Caps,
}

impl Default for JanitorConfig {
    fn default() -> Self {
        JanitorConfig {
            enabled: true,
            interval: Duration::from_secs(DEFAULT_INTERVAL_S),
            pressure_free_pct: DEFAULT_PRESSURE_FREE_PCT,
            critical_free_pct: DEFAULT_CRITICAL_FREE_PCT,
            plugin_log_max_bytes: DEFAULT_PLUGIN_LOG_MAX_BYTES,
            plugin_log_keep_bytes: DEFAULT_PLUGIN_LOG_KEEP_BYTES,
            audit_max_bytes: DEFAULT_AUDIT_MAX_BYTES,
            audit_keep_bytes: DEFAULT_AUDIT_KEEP_BYTES,
            recording_retention: Duration::from_secs(DEFAULT_RECORDING_RETENTION_S),
            recording_pressure_retention: Duration::from_secs(
                DEFAULT_RECORDING_PRESSURE_RETENTION_S,
            ),
            recording_keep_newest: DEFAULT_RECORDING_KEEP_NEWEST,
            journal_pressure_bytes: DEFAULT_JOURNAL_PRESSURE_BYTES,
            journal_floor_bytes: DEFAULT_JOURNAL_FLOOR_BYTES,
            quarantine_keep_newest: DEFAULT_QUARANTINE_KEEP_NEWEST,
            budget_bytes: super::budget::DEFAULT_BUDGET_BYTES,
            caps: super::budget::Caps::default(),
        }
    }
}

impl JanitorConfig {
    /// Which rung this pass runs at, from the footprint against the budget and
    /// free space on the card. The harsher of the two wins.
    ///
    /// **Footprint is the primary signal.** Free-space percentage would not have
    /// caught any of the failures this exists to prevent: a 128 GB card at 3%
    /// used can carry a runaway store, and no percentage threshold fires until
    /// the day there is nothing left to trim gracefully. The budget fires when
    /// the agent starts taking more than its share, which is while something can
    /// still be done. Percentage is kept as the secondary net for a card the
    /// agent shares with something else that is filling it.
    ///
    /// An **unmeasured** free ratio contributes `Routine`, never higher. The
    /// escalated rungs give up real evidence, and "I could not read the
    /// filesystem" is not grounds for that. A footprint is always measurable, so
    /// it has no such case.
    pub fn rung_for(&self, free_pct: Option<f64>, footprint: &super::budget::Footprint) -> Rung {
        let by_free = match free_pct {
            Some(pct) if pct < self.critical_free_pct => Rung::Critical,
            Some(pct) if pct < self.pressure_free_pct => Rung::Pressure,
            _ => Rung::Routine,
        };
        let by_budget =
            match super::budget::budget_state(footprint.budgeted_total(), self.budget_bytes) {
                super::budget::BudgetState::Over => Rung::Critical,
                super::budget::BudgetState::Near => Rung::Pressure,
                super::budget::BudgetState::Within => Rung::Routine,
            };
        by_free.max(by_budget)
    }

    /// Build the plan for a rung.
    pub fn plan(&self, rung: Rung) -> Plan {
        let escalated = matches!(rung, Rung::Pressure | Rung::Critical);
        Plan {
            rung,
            apt_lists: escalated,
            recording_cutoff: if escalated {
                self.recording_pressure_retention
            } else {
                self.recording_retention
            },
            // Never below the floor, whatever the config says.
            journal_target_bytes: escalated
                .then(|| self.journal_pressure_bytes.max(self.journal_floor_bytes)),
            prune_quarantines: escalated,
            // One quarantined store always survives — it is the evidence of the
            // most recent corruption, and the corruption is what this whole
            // effort is about.
            keep_quarantines: self.quarantine_keep_newest.max(1),
            recording_keep_newest: self.recording_keep_newest.max(1),
        }
    }
}

/// What one pass is authorised to do. The categories that are unconditional
/// (the apt archive cache, the plugin-log trim, the audit trim) are not
/// represented as flags because they run at every rung; a flag that is always
/// true reads as a decision that was made, and it was not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub rung: Rung,
    /// Give up the apt package index (costs one `apt-get update` later).
    pub apt_lists: bool,
    /// Recordings older than this are reclaimed, subject to `recording_keep_newest`.
    pub recording_cutoff: Duration,
    /// Vacuum the journal down to this many bytes, or leave it alone.
    pub journal_target_bytes: Option<u64>,
    /// Reclaim quarantined stores beyond `keep_quarantines`.
    pub prune_quarantines: bool,
    /// How many quarantined stores survive, newest first. Never below 1.
    pub keep_quarantines: usize,
    /// How many recordings survive regardless of age, newest first. Never below 1.
    pub recording_keep_newest: usize,
}

/// Given `(name, mtime_unix)` pairs, the names to reclaim — everything but the
/// `keep` newest. Pure.
///
/// `keep` is floored at 1 here as well as in [`JanitorConfig::plan`], because
/// this is the function that does the choosing and a caller that passes zero
/// must not be able to empty the set.
pub fn beyond_newest(entries: &[(String, i64)], keep: usize) -> Vec<String> {
    let keep = keep.max(1);
    let mut sorted: Vec<&(String, i64)> = entries.iter().collect();
    // Newest first; a tie falls back to the name so the choice is deterministic.
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted
        .into_iter()
        .skip(keep)
        .map(|(name, _)| name.clone())
        .collect()
}

/// The `keep` newest names, which no rule may reclaim. Pure.
///
/// Floored at 1: this is the function every floor is built on, so a caller that
/// passes zero must not be able to empty the set.
pub fn newest_names(entries: &[(String, i64)], keep: usize) -> std::collections::BTreeSet<String> {
    let keep = keep.max(1);
    let mut sorted: Vec<&(String, i64)> = entries.iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted
        .into_iter()
        .take(keep)
        .map(|(name, _)| name.clone())
        .collect()
}

/// The one capture per recording path that a capture is appending to right now,
/// judged by how recently the file itself was written. Pure.
///
/// **The signal is the artifact, not a process.** Process liveness is never
/// proof of work, and the janitor has no handle on mediamtx to ask anyway: a
/// running recorder that has stopped producing bytes would still answer "yes" to
/// a liveness question and "no" to this one. A file whose mtime moved inside the
/// window is a file something wrote inside the window.
///
/// The newest of each path, not every fresh file: with a 60s segment duration a
/// broader rule would pin a couple of finished segments as well, and this floor
/// has to be as small as it can be while still covering the file being appended
/// to. Paths are grouped on the directory part of the relative name, which is
/// mediamtx's `%path` segment directory.
pub fn in_flight_names(
    entries: &[(String, i64)],
    now_unix: i64,
    window_s: i64,
) -> std::collections::BTreeSet<String> {
    let cutoff = now_unix.saturating_sub(window_s.max(0));
    let mut newest: std::collections::BTreeMap<&str, (&str, i64)> =
        std::collections::BTreeMap::new();
    for (name, mtime) in entries {
        let group = match name.rfind('/') {
            Some(idx) => &name[..idx],
            None => "",
        };
        // Newest wins; a tie falls back to the lexicographically first name, the
        // same deterministic order `newest_names` picks.
        let better = match newest.get(group) {
            None => true,
            Some((best_name, best_mtime)) => {
                *mtime > *best_mtime || (*mtime == *best_mtime && name.as_str() < *best_name)
            }
        };
        if better {
            newest.insert(group, (name.as_str(), *mtime));
        }
    }
    newest
        .into_values()
        .filter(|(_, mtime)| *mtime >= cutoff)
        .map(|(name, _)| name.to_string())
        .collect()
}

/// The capture names no rule may reclaim, at any rung: the `keep_newest` newest
/// whatever their age, PLUS whatever an in-flight capture is writing. Pure.
///
/// **The in-flight rule.** mediamtx's native recorder writes a continuous
/// segment stream, so the newest segment of an actively-recording path is being
/// appended to at this instant. Reclaiming it would take the file out from under
/// the writer mid-fragment and truncate the capture an operator is deliberately
/// making — the janitor and the recorder fighting over the same file, which is
/// the defect, not the fix. `keep_newest` alone is not this guarantee: it is a
/// count, and a configured count of 1 on a box with two recording paths protects
/// one of them.
pub fn protected_recordings(
    entries: &[(String, i64)],
    keep_newest: usize,
    now_unix: i64,
) -> std::collections::BTreeSet<String> {
    let mut protected = newest_names(entries, keep_newest);
    protected.extend(in_flight_names(
        entries,
        now_unix,
        RECORDING_IN_FLIGHT_WINDOW_S,
    ));
    protected
}

/// Given `(name, mtime_unix)` pairs, the names older than `cutoff_unix` — but
/// never anything in `protected`. Pure.
pub fn older_than_excluding(
    entries: &[(String, i64)],
    cutoff_unix: i64,
    protected: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let mut out: Vec<String> = entries
        .iter()
        .filter(|(name, mtime)| *mtime < cutoff_unix && !protected.contains(name))
        .map(|(name, _)| name.clone())
        .collect();
    out.sort();
    out
}

/// Bytes that must come off the recordings volume to restore the free-space
/// floor, or zero when there is nothing to do. Pure.
///
/// An UNMEASURED free ratio contributes zero, never the whole floor: "I could
/// not read the filesystem" is not grounds for deleting an operator's
/// recordings, the same rule [`JanitorConfig::rung_for`] applies to an
/// unreadable free percentage.
pub fn free_space_deficit(free_bytes: Option<u64>, floor_bytes: u64) -> u64 {
    match free_bytes {
        Some(free) => floor_bytes.saturating_sub(free),
        None => 0,
    }
}

/// Where to cut an over-long append-only file so the last `keep_bytes` survive.
///
/// Returns `None` when the file is at or under `max_bytes` — the file is not
/// over its cap, so nothing is trimmed and the pass reclaims zero from it. This
/// is what makes a second run immediately after a first a no-op.
pub fn trim_from(len: u64, max_bytes: u64, keep_bytes: u64) -> Option<u64> {
    if len <= max_bytes {
        return None;
    }
    // The floor: the tail always survives. A keep larger than the cap would
    // trim to a size that immediately re-triggers, so it is clamped.
    let keep = keep_bytes.min(max_bytes);
    Some(len.saturating_sub(keep))
}

#[cfg(test)]
mod tests;
