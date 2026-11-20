//! Give back the disk apt borrowed to do the install.
//!
//! `apt-get install` downloads every `.deb` into `/var/cache/apt/archives` and
//! leaves it there. Nothing on the box ever removes it. On a freshly flashed
//! ground station the downloaded packages plus the package index were measured
//! at 349 MB — the single largest consumer on the card, and every byte of it is
//! a file whose only purpose ended the moment the package was unpacked.
//!
//! Two things are reclaimed, and they are not equally safe:
//!
//! * **The archive cache** (`apt-get clean`). Pure waste. A cached `.deb` is
//!   only ever re-read by a re-install of the identical version, which is not
//!   something that happens on an appliance. Removing it costs nothing.
//! * **The package index** (`/var/lib/apt/lists`). Not waste — it is what lets
//!   `apt install <pkg>` resolve a name without a network round trip, and what
//!   spares the next install or upgrade a full `apt-get update` (minutes on a
//!   slow link). So it is pruned **only when the card is short of space**: free
//!   space below [`LISTS_KEEP_MIN_FREE_BYTES`] or below
//!   [`LISTS_KEEP_MIN_FREE_PCT`] of the filesystem, or unreadable. Pruning is
//!   safe when it happens because every apt path the agent owns runs
//!   `apt-get update` before it installs anything; an operator installing
//!   something by hand pays one `apt update`, which apt itself tells them to
//!   run. On a card that is filling up, that one-command inconvenience is worth
//!   a few hundred megabytes; on a card with room it is not.
//!
//! The step runs right after `deps`, not at the end, so the space is back
//! *before* the venv, the fetched binaries and the DKMS build ask for it.
//!
//! Optional by kind, and deliberately so: a reclaim that fails is a reason to
//! try harder later, never a reason to abort an otherwise working install. A
//! full disk is the condition this step exists to relieve, so failing the
//! install on it would be exactly backwards.
//!
//! Idempotent: a second run finds an empty cache and either a kept index (room
//! to spare) or an already-pruned one, reclaims nothing, and reports zero.

use std::path::{Path, PathBuf};

use ados_config::capture_storage::{statvfs_space, FsSpace};

use crate::ctx::Ctx;
use crate::graph::{Step, StepKind, StepOutcome};
use ados_term::exec;

/// Where apt parks the `.deb` files it downloaded.
const APT_ARCHIVE_DIR: &str = "/var/cache/apt/archives";
/// Where apt keeps the fetched package index.
const APT_LISTS_DIR: &str = "/var/lib/apt/lists";

/// Entries under `/var/lib/apt/lists` that must survive a prune.
///
/// `lock` is apt's own flock target: deleting it while another apt holds it
/// breaks that apt's mutual exclusion rather than merely inconveniencing it.
/// `partial` and `auxfiles` are directories apt expects to exist and recreates
/// awkwardly; their *contents* are prunable, the directories themselves are not.
const LISTS_KEEP: &[&str] = &["lock", "partial", "auxfiles"];

/// Free space below which the package index is pruned, absolute.
///
/// The steps after this one (the venv, the fetched binaries, the DKMS build)
/// still have to land on the card, and the agent's own logs and recordings
/// follow them. Below 2 GiB that headroom is what matters, not a saved
/// `apt-get update`; above it the index's few hundred megabytes are noise.
pub const LISTS_KEEP_MIN_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Free space below which the package index is pruned, as a percentage of the
/// filesystem. The same 20 % at which the supervisor janitor's Pressure rung
/// engages (`DEFAULT_PRESSURE_FREE_PCT` in `ados-supervisor`, which this crate
/// does not depend on, so the figure is restated here and moves with it):
/// keeping the index must not be what pushes a card into a pressure sweep.
pub const LISTS_KEEP_MIN_FREE_PCT: u64 = 20;

/// What happens to the package index on this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListsVerdict {
    /// Both free-space floors hold: the index stays for the next install.
    Keep,
    /// Free space is below one of the floors: the index is pruned.
    PruneLowSpace,
    /// The filesystem could not be measured: pruned, because this step exists
    /// to relieve a full card and an unmeasurable one is not known to have room.
    PruneUnknownSpace,
}

impl ListsVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            ListsVerdict::Keep => "keep",
            ListsVerdict::PruneLowSpace => "prune_low_space",
            ListsVerdict::PruneUnknownSpace => "prune_unknown_space",
        }
    }
}

/// Decide whether the package index stays, from the free space of the
/// filesystem that holds it. Pure.
pub fn lists_verdict(space: Option<FsSpace>) -> ListsVerdict {
    let Some(space) = space.filter(|s| s.total_bytes > 0) else {
        return ListsVerdict::PruneUnknownSpace;
    };
    let pct_ok = u128::from(space.free_bytes) * 100
        >= u128::from(space.total_bytes) * u128::from(LISTS_KEEP_MIN_FREE_PCT);
    if space.free_bytes >= LISTS_KEEP_MIN_FREE_BYTES && pct_ok {
        ListsVerdict::Keep
    } else {
        ListsVerdict::PruneLowSpace
    }
}

/// Decide which names directly under `/var/lib/apt/lists` may be removed. Pure,
/// so the keep-list is asserted without a filesystem.
///
/// Everything apt fetched is a regular file named after its source; those are
/// the reclaim. The three entries in [`LISTS_KEEP`] are apt's own plumbing and
/// stay. A name is judged on the name alone, so a future index file cannot be
/// mistaken for plumbing by having an unusual size or mode.
pub fn lists_prunable(names: &[String]) -> Vec<String> {
    names
        .iter()
        .filter(|n| !LISTS_KEEP.contains(&n.as_str()))
        .cloned()
        .collect()
}

/// Sum the bytes of the regular files directly under `dir`, and of one level of
/// files inside its subdirectories.
///
/// Not a full recursive walk: apt's layout is flat plus `partial/`, and a
/// bounded two-level sum cannot be walked into a pathological tree by anything
/// that happens to appear under the directory. Unreadable entries contribute
/// zero rather than aborting the count — this number is a report, not a gate.
fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            if let Ok(inner) = std::fs::read_dir(entry.path()) {
                for f in inner.flatten() {
                    if let Ok(m) = f.metadata() {
                        if m.is_file() {
                            total = total.saturating_add(m.len());
                        }
                    }
                }
            }
        }
    }
    total
}

/// Names directly under `dir`, or an empty vector when it cannot be read.
fn entry_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .collect()
}

/// Remove the prunable entries under `dir`, keeping apt's plumbing. Returns the
/// bytes that were there before, measured first so the figure is real rather
/// than assumed. Best-effort throughout.
fn prune_lists(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    let before = dir_bytes(dir);
    for name in lists_prunable(&entry_names(dir)) {
        let path: PathBuf = dir.join(&name);
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed {
            tracing::debug!(path = %path.display(), error = %e, "apt list entry not removed");
        }
    }
    // Empty the contents of `partial/`, which holds interrupted downloads and is
    // kept as a directory but never as content.
    let partial = dir.join("partial");
    if partial.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&partial) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    before.saturating_sub(dir_bytes(dir))
}

/// Measure the filesystem holding `dir` with `stat`, then keep or prune the
/// index under it. Returns the verdict, the measurement it was made on, and the
/// bytes freed (zero when kept). `stat` is `statvfs` on a node and a fixed
/// figure in tests.
fn reclaim_lists(
    dir: &Path,
    stat: fn(&Path) -> Option<FsSpace>,
) -> (ListsVerdict, Option<FsSpace>, u64) {
    let space = stat(dir);
    let verdict = lists_verdict(space);
    let freed = match verdict {
        ListsVerdict::Keep => 0,
        ListsVerdict::PruneLowSpace | ListsVerdict::PruneUnknownSpace => prune_lists(dir),
    };
    (verdict, space, freed)
}

/// Reclaim the disk apt used during the install.
pub struct AptReclaim;

impl Step for AptReclaim {
    fn id(&self) -> &str {
        "apt_reclaim"
    }
    fn requires(&self) -> &[&str] {
        &["deps"]
    }
    fn checkpoint(&self) -> Option<&str> {
        // Deliberately no checkpoint: the cache refills on every subsequent
        // upgrade, so this must run again on the next install, not be skipped
        // as already-done.
        None
    }
    fn kind(&self) -> StepKind {
        StepKind::Optional
    }
    fn run(&self, ctx: &mut Ctx) -> StepOutcome {
        if let Some(skip) = crate::host::personal_skip(ctx, "apt_reclaim") {
            return skip;
        }
        let archives = Path::new(APT_ARCHIVE_DIR);
        let cache_before = dir_bytes(archives);
        // `apt-get clean` rather than deleting the files ourselves: it knows the
        // full set of cache locations for this apt, including any the layout
        // moves in a future release.
        if !exec::run_ok("apt-get", &["clean"]) {
            tracing::debug!("apt-get clean did not run; leaving the archive cache alone");
        }
        let cache_freed = cache_before.saturating_sub(dir_bytes(archives));

        // Measured after `apt-get clean`, so the cache just given back counts
        // toward the room the index is judged against.
        let (verdict, space, lists_freed) = reclaim_lists(Path::new(APT_LISTS_DIR), statvfs_space);
        tracing::info!(
            lists = verdict.as_str(),
            free_bytes = space.map(|s| s.free_bytes),
            total_bytes = space.map(|s| s.total_bytes),
            min_free_bytes = LISTS_KEEP_MIN_FREE_BYTES,
            min_free_pct = LISTS_KEEP_MIN_FREE_PCT,
            "apt package index {}",
            if verdict == ListsVerdict::Keep {
                "kept: the card has room, the next install skips a full apt-get update"
            } else {
                "pruned: the card is short of space or could not be measured"
            }
        );

        let total = cache_freed.saturating_add(lists_freed);
        tracing::info!(
            cache_bytes = cache_freed,
            lists_bytes = lists_freed,
            "reclaimed apt disk"
        );
        if total > 0 {
            ctx.progress.activity(
                "apt_reclaim",
                format!("reclaimed {} of downloaded packages", human_bytes(total)),
            );
        }
        StepOutcome::Ok
    }
}

/// Render a byte count the way an operator reads it. Pure.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    for (i, unit) in UNITS.iter().enumerate() {
        if size < 1024.0 || i == UNITS.len() - 1 {
            return if i == 0 {
                format!("{bytes} {unit}")
            } else {
                format!("{size:.1} {unit}")
            };
        }
        size /= 1024.0;
    }
    format!("{bytes} B")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn apt_plumbing_survives_the_prune() {
        let names: Vec<String> = [
            "lock",
            "partial",
            "auxfiles",
            "deb.debian.org_debian_dists_bookworm_InRelease",
            "deb.debian.org_debian_dists_bookworm_main_binary-arm64_Packages",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let prunable = lists_prunable(&names);
        assert_eq!(
            prunable,
            vec![
                "deb.debian.org_debian_dists_bookworm_InRelease".to_string(),
                "deb.debian.org_debian_dists_bookworm_main_binary-arm64_Packages".to_string(),
            ]
        );
        for keep in LISTS_KEEP {
            assert!(
                !prunable.contains(&keep.to_string()),
                "{keep} must never be pruned"
            );
        }
    }

    #[test]
    fn an_empty_index_prunes_nothing() {
        assert!(lists_prunable(&[]).is_empty());
    }

    #[test]
    fn pruning_reports_the_bytes_it_actually_freed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join("deb.example.org_dists_stable_InRelease"),
            vec![0u8; 4096],
        )
        .unwrap();
        fs::write(
            dir.join("deb.example.org_dists_stable_Packages"),
            vec![0u8; 8192],
        )
        .unwrap();
        fs::write(dir.join("lock"), b"").unwrap();
        fs::create_dir(dir.join("partial")).unwrap();
        fs::write(dir.join("partial").join("half.deb"), vec![0u8; 1024]).unwrap();

        let freed = prune_lists(dir);
        assert_eq!(freed, 4096 + 8192 + 1024);
        assert!(dir.join("lock").exists(), "apt's lock file must survive");
        assert!(
            dir.join("partial").is_dir(),
            "partial/ must survive as a dir"
        );
        assert!(
            !dir.join("partial").join("half.deb").exists(),
            "an interrupted download is not worth keeping"
        );
        assert!(!dir.join("deb.example.org_dists_stable_Packages").exists());
    }

    #[test]
    fn a_second_prune_reclaims_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join("deb.example.org_dists_stable_InRelease"),
            vec![0u8; 4096],
        )
        .unwrap();
        fs::write(dir.join("lock"), b"").unwrap();

        assert_eq!(prune_lists(dir), 4096);
        assert_eq!(prune_lists(dir), 0, "idempotent: nothing left to reclaim");
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    fn space(total_gib: u64, free_gib: u64) -> Option<FsSpace> {
        Some(FsSpace {
            total_bytes: total_gib * GIB,
            free_bytes: free_gib * GIB,
        })
    }

    /// An apt lists dir holding one 8 KiB index file and apt's `lock`.
    fn seeded_lists() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join("deb.example.org_dists_stable_Packages"),
            vec![0u8; 8192],
        )
        .unwrap();
        fs::write(dir.join("lock"), b"").unwrap();
        tmp
    }

    #[test]
    fn the_index_stays_when_the_card_has_room() {
        // 32 GB card, 12 GiB free: over both floors.
        assert_eq!(lists_verdict(space(32, 12)), ListsVerdict::Keep);
        // Exactly on both floors is room enough.
        assert_eq!(lists_verdict(space(10, 2)), ListsVerdict::Keep);

        let tmp = seeded_lists();
        let (verdict, _, freed) = reclaim_lists(tmp.path(), |_| space(32, 12));
        assert_eq!(verdict, ListsVerdict::Keep);
        assert_eq!(freed, 0);
        assert!(
            tmp.path()
                .join("deb.example.org_dists_stable_Packages")
                .exists(),
            "a kept index spares the next install a full apt-get update"
        );
    }

    #[test]
    fn the_index_is_pruned_when_the_card_is_short_of_space() {
        // 8 GB card, 1 GiB free: under the absolute floor (and the share).
        assert_eq!(lists_verdict(space(8, 1)), ListsVerdict::PruneLowSpace);
        // 128 GB card, 10 GiB free: plenty of bytes, but under 20 % of the card.
        assert_eq!(lists_verdict(space(128, 10)), ListsVerdict::PruneLowSpace);
        // 4 GB card, 1 GiB free: 25 %, but under the absolute floor.
        assert_eq!(lists_verdict(space(4, 1)), ListsVerdict::PruneLowSpace);

        let tmp = seeded_lists();
        let (verdict, _, freed) = reclaim_lists(tmp.path(), |_| space(8, 1));
        assert_eq!(verdict, ListsVerdict::PruneLowSpace);
        assert_eq!(freed, 8192);
        assert!(!tmp
            .path()
            .join("deb.example.org_dists_stable_Packages")
            .exists());
        assert!(
            tmp.path().join("lock").exists(),
            "apt's lock file must survive"
        );
    }

    #[test]
    fn an_unmeasurable_card_is_pruned() {
        assert_eq!(lists_verdict(None), ListsVerdict::PruneUnknownSpace);
        assert_eq!(lists_verdict(space(0, 0)), ListsVerdict::PruneUnknownSpace);

        let tmp = seeded_lists();
        let (verdict, _, freed) = reclaim_lists(tmp.path(), |_| None);
        assert_eq!(verdict, ListsVerdict::PruneUnknownSpace);
        assert_eq!(freed, 8192);
    }

    #[test]
    fn an_absent_directory_is_zero_not_an_error() {
        assert_eq!(prune_lists(Path::new("/no/such/apt/lists")), 0);
        assert_eq!(dir_bytes(Path::new("/no/such/apt/lists")), 0);
    }

    #[test]
    fn byte_rendering_reads_the_way_an_operator_expects() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(349 * 1024 * 1024), "349.0 MB");
    }
}
