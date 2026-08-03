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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
/// A plugin log larger than this is trimmed. systemd appends to these with no
/// rotation configured anywhere, so nothing else bounds them.
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
        }
    }
}

impl JanitorConfig {
    /// Which rung a measured free-space fraction selects.
    ///
    /// An **unmeasured** free ratio resolves to `Routine`, never higher. The
    /// escalated rungs give up real evidence, and "I could not read the
    /// filesystem" is not grounds for that — it is the same discipline the
    /// storage diagnostic follows when it refuses to render an absent number as
    /// a zero.
    pub fn rung_for(&self, free_pct: Option<f64>) -> Rung {
        match free_pct {
            Some(pct) if pct < self.critical_free_pct => Rung::Critical,
            Some(pct) if pct < self.pressure_free_pct => Rung::Pressure,
            _ => Rung::Routine,
        }
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

/// Given `(name, mtime_unix)` pairs, the names older than `cutoff_unix` — but
/// never the `keep_newest` newest, whatever their age. Pure.
pub fn older_than_keeping_newest(
    entries: &[(String, i64)],
    cutoff_unix: i64,
    keep_newest: usize,
) -> Vec<String> {
    let protected: std::collections::BTreeSet<String> = {
        let keep = keep_newest.max(1);
        let mut sorted: Vec<&(String, i64)> = entries.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        sorted
            .into_iter()
            .take(keep)
            .map(|(name, _)| name.clone())
            .collect()
    };
    let mut out: Vec<String> = entries
        .iter()
        .filter(|(name, mtime)| *mtime < cutoff_unix && !protected.contains(name))
        .map(|(name, _)| name.clone())
        .collect();
    out.sort();
    out
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
mod tests {
    use super::*;

    #[test]
    fn free_space_selects_the_rung() {
        let cfg = JanitorConfig::default();
        assert_eq!(cfg.rung_for(Some(50.0)), Rung::Routine);
        assert_eq!(cfg.rung_for(Some(20.0)), Rung::Routine);
        assert_eq!(cfg.rung_for(Some(19.9)), Rung::Pressure);
        assert_eq!(cfg.rung_for(Some(10.0)), Rung::Pressure);
        assert_eq!(cfg.rung_for(Some(9.9)), Rung::Critical);
        assert_eq!(cfg.rung_for(Some(0.0)), Rung::Critical);
    }

    #[test]
    fn an_unmeasured_filesystem_never_escalates() {
        // Refusing to read free space is not evidence that space is short, and
        // the escalated rungs give up real evidence.
        assert_eq!(JanitorConfig::default().rung_for(None), Rung::Routine);
    }

    #[test]
    fn the_escalated_rungs_widen_the_plan() {
        let cfg = JanitorConfig::default();
        let routine = cfg.plan(Rung::Routine);
        assert!(!routine.apt_lists);
        assert!(!routine.prune_quarantines);
        assert_eq!(routine.journal_target_bytes, None);
        assert_eq!(routine.recording_cutoff, cfg.recording_retention);

        for rung in [Rung::Pressure, Rung::Critical] {
            let p = cfg.plan(rung);
            assert!(p.apt_lists, "{rung:?} must give up the package index");
            assert!(p.prune_quarantines, "{rung:?} must prune old quarantines");
            assert!(p.journal_target_bytes.is_some());
            assert_eq!(p.recording_cutoff, cfg.recording_pressure_retention);
            assert!(p.recording_cutoff < cfg.recording_retention);
        }
    }

    #[test]
    fn the_journal_is_never_vacuumed_below_its_floor() {
        // An operator (or a bad config) asks for something below the floor.
        let cfg = JanitorConfig {
            journal_pressure_bytes: 1024,
            ..JanitorConfig::default()
        };
        let p = cfg.plan(Rung::Critical);
        assert_eq!(p.journal_target_bytes, Some(cfg.journal_floor_bytes));
    }

    #[test]
    fn the_newest_quarantine_survives_every_rung() {
        let entries = vec![
            ("logs.db.corrupt-1000".to_string(), 1_000),
            ("logs.db.corrupt-3000".to_string(), 3_000),
            ("logs.db.corrupt-2000".to_string(), 2_000),
        ];
        let cfg = JanitorConfig::default();
        for rung in [Rung::Routine, Rung::Pressure, Rung::Critical] {
            let plan = cfg.plan(rung);
            let doomed = beyond_newest(&entries, plan.keep_quarantines);
            assert!(
                !doomed.contains(&"logs.db.corrupt-3000".to_string()),
                "the newest quarantined store must survive at {rung:?}, got {doomed:?}"
            );
        }
    }

    #[test]
    fn a_zero_keep_cannot_empty_the_set() {
        let entries = vec![
            ("a".to_string(), 1),
            ("b".to_string(), 2),
            ("c".to_string(), 3),
        ];
        // Even asked for zero, the newest is kept.
        let doomed = beyond_newest(&entries, 0);
        assert_eq!(doomed, vec!["b".to_string(), "a".to_string()]);
        assert!(!doomed.contains(&"c".to_string()));
    }

    #[test]
    fn beyond_newest_keeps_the_newest_not_the_first_listed() {
        let entries = vec![
            ("old".to_string(), 100),
            ("new".to_string(), 900),
            ("mid".to_string(), 500),
        ];
        assert_eq!(
            beyond_newest(&entries, 1),
            vec!["mid".to_string(), "old".to_string()]
        );
        assert_eq!(beyond_newest(&entries, 2), vec!["old".to_string()]);
        assert!(beyond_newest(&entries, 5).is_empty());
    }

    #[test]
    fn retention_never_takes_the_newest_recordings_however_old() {
        // Every recording predates the cutoff — a box that has not flown for a
        // year. The newest three still survive.
        let entries: Vec<(String, i64)> = (0..10)
            .map(|i| (format!("flight-{i:02}.mp4"), 1_000 + i as i64))
            .collect();
        let doomed = older_than_keeping_newest(&entries, 100_000, 3);
        assert_eq!(doomed.len(), 7);
        for keeper in ["flight-09.mp4", "flight-08.mp4", "flight-07.mp4"] {
            assert!(
                !doomed.contains(&keeper.to_string()),
                "{keeper} must survive"
            );
        }
    }

    #[test]
    fn retention_leaves_everything_inside_the_window() {
        let entries = vec![
            ("recent-a.mp4".to_string(), 9_000),
            ("recent-b.mp4".to_string(), 9_500),
        ];
        assert!(older_than_keeping_newest(&entries, 1_000, 1).is_empty());
    }

    #[test]
    fn a_file_under_its_cap_is_not_trimmed() {
        assert_eq!(trim_from(100, 200, 50), None);
        assert_eq!(trim_from(200, 200, 50), None, "at the cap is not over it");
    }

    #[test]
    fn a_trim_leaves_the_tail_and_a_second_trim_does_nothing() {
        // 1000 bytes, cap 400, keep 100 -> drop 900, leaving 100.
        assert_eq!(trim_from(1_000, 400, 100), Some(900));
        // The file is now 100 bytes, under the cap: idempotent.
        assert_eq!(trim_from(100, 400, 100), None);
    }

    #[test]
    fn a_keep_larger_than_the_cap_cannot_cause_a_trim_loop() {
        // keep 800 > cap 400 would leave the file over its cap and re-trigger
        // on every pass. It is clamped to the cap instead.
        assert_eq!(trim_from(1_000, 400, 800), Some(600));
        assert_eq!(trim_from(400, 400, 800), None, "the result is not over cap");
    }
}
