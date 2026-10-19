//! The cap-enforcing sweep and the reclaimable-bytes estimate.

use super::*;

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
pub(super) async fn enforce_caps(
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
                // Both layouts under the recordings root, and the same floor
                // every other recordings rule honours: the keep-newest count
                // PLUS whatever a capture is appending to right now. The cap is
                // not licence to reclaim the segment mediamtx is mid-fragment on.
                let entries = reclaim::recording_entries(&roots.recordings);
                let protected = plan::protected_recordings(
                    &entries,
                    cfg.recording_keep_newest.max(1),
                    now_unix(),
                );
                out.recordings =
                    out.recordings
                        .saturating_add(reclaim::reclaim_entries_to_cap_oldest_first(
                            &roots.recordings,
                            entries,
                            cap,
                            &protected,
                        ));
            }
            Category::QuarantinedStores => {
                let entries = reclaim::entries_with_mtime(&roots.logd_store, |n| {
                    n.starts_with("logs.db.corrupt-")
                });
                // The floor that outranks the cap. A single corpse larger than
                // the whole quarantine share is the case that actually happens,
                // and it stays.
                let protected = plan::newest_names(&entries, cfg.quarantine_keep_newest.max(1));
                out.quarantined_stores = out.quarantined_stores.saturating_add(
                    reclaim::reclaim_entries_to_cap_oldest_first(
                        &roots.logd_store,
                        entries,
                        cap,
                        &protected,
                    ),
                );
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
///
/// `free_pct` selects the rung; `free_bytes` is the recordings volume's absolute
/// headroom, which drives the recording floor. Both are injected for the same
/// reason: a `statvfs` belongs to the reconciler, not to the decision.
pub async fn sweep(
    roots: &Roots,
    cfg: &JanitorConfig,
    free_pct: Option<f64>,
    free_bytes: Option<u64>,
) -> (Rung, Reclaimed) {
    let now = now_unix();
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

    let cutoff = now.saturating_sub(p.recording_cutoff.as_secs() as i64);
    let mut recordings =
        reclaim::reclaim_recordings(&roots.recordings, cutoff, p.recording_keep_newest, now);

    // Things with residual value, only once space is short. They run before
    // the recording floor below: recordings are the one category a human
    // deliberately created, so they are never deleted for space that the
    // package index, the journal or a quarantined store could have given.
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

    // The recording volume's space floor. Time-based retention cannot cover
    // this: mediamtx's own 24h `recordDeleteAfter` window does not fire until
    // long after a high-bitrate day has filled the card, and a full card stops
    // the recorder dead mid-flight. Reclaims oldest-first through the SAME
    // machinery the age and cap rules use, so there is one reclaimer over this
    // directory rather than two fighting. The deficit is what is still missing
    // after everything freed above (the node keeps all of it on one volume; on
    // a split layout the next sweep's fresh reading corrects an over-credit).
    let freed_before_floor = apt_archives
        .saturating_add(plugin_logs)
        .saturating_add(audit_log)
        .saturating_add(recordings)
        .saturating_add(apt_lists)
        .saturating_add(journal)
        .saturating_add(quarantined_stores);
    recordings = recordings.saturating_add(reclaim::reclaim_recordings_to_free_floor(
        &roots.recordings,
        plan::free_space_deficit(
            free_bytes.map(|f| f.saturating_add(freed_before_floor)),
            plan::RECORD_MIN_FREE_BYTES,
        ),
        p.recording_keep_newest,
        now,
    ));

    let mut freed = Reclaimed {
        apt_archives,
        apt_lists,
        plugin_logs,
        audit_log,
        recordings,
        journal,
        quarantined_stores,
        // The capture category runs its own pass against its own budget; the
        // reconciler folds its figure in.
        capture: 0,
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

    let now = now_unix();
    let cutoff = now.saturating_sub(p.recording_cutoff.as_secs() as i64);
    let recordings = reclaim::recording_reclaimable_bytes(
        &roots.recordings,
        cutoff,
        p.recording_keep_newest,
        now,
    );

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
        // Capture sessions are evicted only against the capture budget and
        // floor, which `capture-storage.json` reports.
        capture: 0,
    }
}
