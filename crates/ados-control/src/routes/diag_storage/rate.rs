//! The write rate: a direct `/proc/diskstats` delta and the store's retained window.

use super::*;

/// The kernel's per-device IO counter file.
pub(super) const DISKSTATS: &str = "/proc/diskstats";

/// How long the direct measurement watches the counter.
///
/// Long enough that a quiet agent still moves it — at the 60 KB/s the box is
/// being held to, five seconds is 300 KB, or 600 sectors, which is far above
/// the granularity of the counter. Short enough that an operator running
/// `ados diag storage` does not think the command has hung.
pub(super) const LIVE_WINDOW: Duration = Duration::from_secs(5);

/// Field index of "sectors written" in a `/proc/diskstats` line, counting from
/// the device name. The line is `major minor name` then 11 or more counters;
/// sectors-written is the 7th of those. A kernel ABI position, stable since 2.6.
const DISKSTATS_WR_SECTORS_FIELD: usize = 6;

/// Read `/proc/diskstats`, wait, read it again. `None` when the file cannot be
/// read at all — a platform without it says so rather than reporting zero.
pub(super) async fn sample_write_window(
    path: &Path,
    window: Duration,
) -> Option<(String, String, f64)> {
    let first = tokio::fs::read_to_string(path).await.ok()?;
    tokio::time::sleep(window).await;
    let second = tokio::fs::read_to_string(path).await.ok()?;
    Some((first, second, window.as_secs_f64()))
}

/// Parse `/proc/diskstats` into device → sectors written. Pure.
///
/// Lines that are too short to carry the counter are skipped rather than
/// treated as zero: a truncated or unfamiliar line is missing data, and
/// pretending it means "no writes" is the failure mode this whole surface
/// exists to avoid. RAM-backed devices are skipped too (see
/// `is_ram_backed_device`).
pub fn parse_diskstats(text: &str) -> std::collections::BTreeMap<String, u64> {
    let mut out = std::collections::BTreeMap::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        // major, minor, then the device name.
        let (Some(_), Some(_), Some(name)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if is_ram_backed_device(name) {
            continue;
        }
        let counters: Vec<&str> = fields.collect();
        let Some(raw) = counters.get(DISKSTATS_WR_SECTORS_FIELD) else {
            continue;
        };
        let Ok(sectors) = raw.parse::<u64>() else {
            continue;
        };
        out.insert(name.to_string(), sectors);
    }
    out
}

/// Whether a block device never touches storage: compressed-RAM swap (`zram*`),
/// RAM disks (`ram*`) and loop devices (`loop*`, whose writes land on a backing
/// file already counted on the real disk). Swap on zram can easily be the busiest
/// writer on an SBC, and counting it would report RAM traffic as card wear.
fn is_ram_backed_device(name: &str) -> bool {
    ["zram", "ram", "loop"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// The write rate this process measured itself, from two `/proc/diskstats`
/// bodies taken `window_s` apart. Pure.
///
/// Returns `None` — not a zero, and not an "unavailable" measurement — when
/// there is no usable device in the pair, so the caller falls through to the
/// store's longer window instead of shadowing it with a failed direct read.
pub(super) fn live_write_rate(first: &str, second: &str, window_s: f64) -> Option<Measurement> {
    if window_s <= 0.0 {
        return None;
    }
    let a = parse_diskstats(first);
    let b = parse_diskstats(second);
    if a.is_empty() || b.is_empty() {
        return None;
    }

    // The device that moved the most in the window, the same rule the retained
    // path uses. A counter that went backwards is skipped rather than read as a
    // huge negative: the only way that happens over five seconds is a device
    // disappearing and being re-added.
    let mut best: Option<(String, u64)> = None;
    for (name, before) in &a {
        let Some(after) = b.get(name) else { continue };
        let Some(moved) = after.checked_sub(*before) else {
            continue;
        };
        if best.as_ref().is_none_or(|(_, m)| moved > *m) {
            best = Some((name.clone(), moved));
        }
    }
    let (device, sectors) = best?;
    let kb = sectors as f64 * SECTOR_BYTES / 1024.0;
    Some(Measurement {
        kb_s: Some(kb / window_s),
        window_s: Some(window_s),
        device: Some(device),
        reason: None,
        source: "direct",
    })
}

/// Sustained write rate as a delta between the oldest and newest snapshot that
/// carry the counter.
///
/// Refuses rather than guesses in three cases a single reading cannot survive: a
/// window too short to be a rate, a counter that went backwards (the box rebooted
/// mid-window and the kernel counter restarted at zero), and a device the
/// collector never sampled.
pub(super) fn write_rate(rows: &[HwRow]) -> Measurement {
    let Some(device) = busiest_device(rows) else {
        return Measurement::unavailable("no disk write counter in the retained window");
    };
    let key = format!("disk.{device}.wr_sectors");

    let mut samples: Vec<(i64, f64)> = rows
        .iter()
        .filter_map(|r| r.num(&key).map(|v| (r.ts_us, v)))
        .collect();
    samples.sort_by_key(|(ts, _)| *ts);

    let (Some(first), Some(last)) = (samples.first(), samples.last()) else {
        return Measurement::unavailable("no disk write counter in the retained window");
    };
    if samples.len() < 2 {
        return Measurement::unavailable("only one sample in the retained window");
    }

    let window_s = (last.0 - first.0) as f64 / 1_000_000.0;
    if window_s < MIN_WINDOW_S {
        return Measurement {
            kb_s: None,
            window_s: Some(window_s),
            device: Some(device),
            reason: Some(format!(
                "window too short to be a rate ({window_s:.0}s, need {MIN_WINDOW_S:.0}s)"
            )),
            source: "store",
        };
    }
    if last.1 < first.1 {
        return Measurement {
            kb_s: None,
            window_s: Some(window_s),
            device: Some(device),
            reason: Some(
                "the counter went backwards in the window (the box rebooted); \
                 rate is not computable across a restart"
                    .to_string(),
            ),
            source: "store",
        };
    }

    let kb = (last.1 - first.1) * SECTOR_BYTES / 1024.0;
    Measurement {
        kb_s: Some(kb / window_s),
        window_s: Some(window_s),
        device: Some(device),
        reason: None,
        source: "store",
    }
}

/// The device that moved the most sectors in the window — the one backing the
/// writes that matter. Picking by name would need a mount lookup the collector
/// does not record; picking by movement needs nothing and cannot name a device
/// that is idle.
pub(super) fn busiest_device(rows: &[HwRow]) -> Option<String> {
    let mut best: Option<(String, f64)> = None;
    for name in device_names(rows) {
        let key = format!("disk.{name}.wr_sectors");
        let values: Vec<f64> = rows.iter().filter_map(|r| r.num(&key)).collect();
        let (Some(min), Some(max)) = (
            values.iter().cloned().reduce(f64::min),
            values.iter().cloned().reduce(f64::max),
        ) else {
            continue;
        };
        let moved = max - min;
        if best.as_ref().is_none_or(|(_, b)| moved > *b) {
            best = Some((name, moved));
        }
    }
    best.map(|(name, _)| name)
}

/// Every device name appearing as `disk.<name>.wr_sectors` in the window.
fn device_names(rows: &[HwRow]) -> Vec<String> {
    let mut names: Vec<String> = rows
        .iter()
        .flat_map(|r| r.signals.keys())
        .filter_map(|k| {
            let rest = k.strip_prefix("disk.")?;
            let name = rest.strip_suffix(".wr_sectors")?;
            (!name.is_empty() && !is_ram_backed_device(name)).then(|| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}
