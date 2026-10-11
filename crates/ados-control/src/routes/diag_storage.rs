//! `GET /api/diag/storage` — the storage-wear verdict.
//!
//! Four SD cards were reflashed in eight days before anyone could say how much
//! the box was writing, because nothing read the answer back. The collector was
//! already recording it: `disk.<dev>.wr_sectors` and the Pi throttle bitfield go
//! into the durable store on every tick, and the store survives the reboot that
//! destroys the evidence in RAM. What was missing was a reader.
//!
//! This route is that reader. It takes the write counter as a **delta across a
//! window** rather than a single reading — a cumulative counter's instantaneous
//! value says nothing about rate — and it reports the throttle bitfield's
//! *sticky* bits, which record that an undervoltage happened at all, not merely
//! that one is happening during this poll.
//!
//! Every field can come back `null` with a stated reason. A box that cannot
//! measure its own wear must say so; a fabricated zero here would read as "this
//! card is fine" on exactly the card that is dying.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::ipc::logd_client::HwRow;
use crate::state::AppState;

mod verdict;

use verdict::*;

/// How many hardware snapshots to pull for the delta. The collector's disk class
/// fires well below its base tick, so this reaches back minutes — long enough
/// that a slow counter still moves, short enough to stay a few hundred KiB.
const WEAR_ROWS: u32 = 2_000;

/// Linux reports `/proc/diskstats` sectors in fixed 512-byte units regardless of
/// the device's physical block size. This is a kernel ABI constant, not a
/// property of the card.
const SECTOR_BYTES: f64 = 512.0;

/// Below this the delta is noise rather than a rate.
const MIN_WINDOW_S: f64 = 30.0;

/// Sustained write rate above which an SD card is being consumed fast enough to
/// matter. The measured pre-fix rate on a live rig was ~1 714 KB/s (~144 GB/day);
/// a healthy idle agent sits near two orders of magnitude below this.
const WEARING_KB_S: f64 = 250.0;

/// Sustained write rate that reproduces the observed failure: a card at this rate
/// wore out and corrupted inside two days.
const CRITICAL_KB_S: f64 = 1_000.0;

/// Filesystem fullness at which a full store rewrite no longer has the scratch
/// space it needs, which is the step that turned wear into an unbootable card.
const CRITICAL_FS_USED_PCT: f64 = 90.0;

pub async fn get_storage_diagnostics(State(state): State<AppState>) -> Json<Value> {
    // Take the direct measurement first. It costs one window of wall clock and
    // it is the reading that does not depend on anything else on the box being
    // alive, which is the whole point of it.
    let live = sample_write_window(Path::new(DISKSTATS), LIVE_WINDOW).await;
    let rows = state.logd.hw_rows(WEAR_ROWS).await;
    let store_dir = store_dir();
    let mut body = build(
        rows.as_deref(),
        &store_dir,
        fs_usage(&store_dir),
        live.as_ref().map(|(a, b, w)| (a.as_str(), b.as_str(), *w)),
        ados_config::log_store::store_enabled(),
    );
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "janitor".to_string(),
            janitor_facts(&janitor_sidecar_path(), now_unix()),
        );
    }
    Json(body)
}

/// Assemble the verdict from the four independent inputs, kept as one pure
/// function so every branch — including "the store is down" — is testable without
/// a rig, a socket, or a filesystem.
///
/// `live` is a pair of raw `/proc/diskstats` bodies and the seconds between
/// them. It is passed as text rather than a parsed rate so the parsing is
/// exercised by the same fixtures that exercise the verdict.
pub fn build(
    rows: Option<&[HwRow]>,
    store_dir: &Path,
    fs: Option<(u64, u64)>,
    live: Option<(&str, &str, f64)>,
    store_enabled: bool,
) -> Value {
    // Prefer the reading this process took itself.
    //
    // The store used to be the only source, which made the tool that diagnosed
    // the dying cards depend on the component that is now off by default — it
    // would have gone blind exactly when it was needed. A direct
    // `/proc/diskstats` delta needs nothing but the kernel. The store is still
    // read, because its retained window is hours long where this one is
    // seconds, and because the sticky throttle bits live only there.
    let direct = live.and_then(|(a, b, window)| live_write_rate(a, b, window));
    let write = match (direct, rows) {
        (Some(m), _) => m,
        (None, Some(rows)) => write_rate(rows),
        (None, None) => Measurement::unavailable(
            "could not read /proc/diskstats, and the logging store did not answer",
        ),
    };
    // The throttle bitfield's sticky bits exist only in the store, so with the
    // store off they are genuinely unavailable — and the reason is "it is off",
    // not "it did not answer". Those read very differently to somebody deciding
    // whether the box is broken.
    let throttle = match (rows, store_enabled) {
        (Some(rows), _) => throttle_history(rows),
        (None, false) => json!({
            "supported": null,
            "reason": "the logging store is disabled, and the sticky throttle bits are \
                       recorded nowhere else",
        }),
        (None, true) => json!({
            "supported": null,
            "reason": "the logging store did not answer",
        }),
    };
    let store = store_facts(store_dir, store_enabled);
    let filesystem = filesystem_facts(fs);

    let (verdict, reason) = verdict(&write, &filesystem, &throttle);

    json!({
        "verdict": verdict,
        "reason": reason,
        "write": write.to_json(),
        "throttle": throttle,
        "store": store,
        "filesystem": filesystem,
    })
}

/// A number that may not exist, carrying why when it does not. The pair travels
/// together so a consumer can never render the absence as a zero.
struct Measurement {
    kb_s: Option<f64>,
    window_s: Option<f64>,
    device: Option<String>,
    reason: Option<String>,
    /// Where the number came from. A rate measured over five seconds and one
    /// averaged over hours answer different questions, and an operator deciding
    /// whether a change worked needs to know which one is on screen.
    source: &'static str,
}

impl Measurement {
    fn unavailable(reason: &str) -> Self {
        Self {
            kb_s: None,
            window_s: None,
            device: None,
            reason: Some(reason.to_string()),
            source: "none",
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "kb_per_s": self.kb_s.map(round1),
            "gb_per_day": self.kb_s.map(|k| round1(k * 86_400.0 / 1_048_576.0)),
            "window_s": self.window_s.map(round1),
            "device": self.device,
            "source": self.source,
            "reason": self.reason,
        })
    }
}

/// The kernel's per-device IO counter file.
const DISKSTATS: &str = "/proc/diskstats";

/// How long the direct measurement watches the counter.
///
/// Long enough that a quiet agent still moves it — at the 60 KB/s the box is
/// being held to, five seconds is 300 KB, or 600 sectors, which is far above
/// the granularity of the counter. Short enough that an operator running
/// `ados diag storage` does not think the command has hung.
const LIVE_WINDOW: Duration = Duration::from_secs(5);

/// Field index of "sectors written" in a `/proc/diskstats` line, counting from
/// the device name. The line is `major minor name` then 11 or more counters;
/// sectors-written is the 7th of those. A kernel ABI position, stable since 2.6.
const DISKSTATS_WR_SECTORS_FIELD: usize = 6;

/// Read `/proc/diskstats`, wait, read it again. `None` when the file cannot be
/// read at all — a platform without it says so rather than reporting zero.
async fn sample_write_window(path: &Path, window: Duration) -> Option<(String, String, f64)> {
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
/// [`is_ram_backed_device`]).
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
fn live_write_rate(first: &str, second: &str, window_s: f64) -> Option<Measurement> {
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
fn write_rate(rows: &[HwRow]) -> Measurement {
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
fn busiest_device(rows: &[HwRow]) -> Option<String> {
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

/// What the disk janitor did on its last pass, and what is left for it to take.
///
/// The wear figures above say how fast the card is being written; this says what
/// is *sitting* on it and which of that can still be given back. The two
/// together are the whole picture: a box can be writing gently and still fill
/// up, which is exactly what happened — the ground station whose card filled was
/// not writing quickly, it was holding 349 MB of downloaded packages that
/// nothing ever removed.
///
/// The janitor mirrors its pass to a sidecar; this reads it. When the sidecar is
/// absent the answer is "the janitor has not completed a pass", stated as such —
/// never a set of zeroes, which would read as "there is nothing to reclaim" on a
/// box where nobody has looked. The pass age travels with the figures so a stale
/// sidecar (the janitor is hourly) is visible rather than passed off as current.
pub fn janitor_facts(sidecar: &Path, now_unix: i64) -> Value {
    let Ok(text) = std::fs::read_to_string(sidecar) else {
        return json!({
            "ran": false,
            "rung": null,
            "reclaimed_bytes": null,
            "reclaimable_bytes": null,
            "reason": "the janitor has not completed a pass since this box booted",
        });
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return json!({
            "ran": false,
            "rung": null,
            "reclaimed_bytes": null,
            "reclaimable_bytes": null,
            "reason": "the janitor's record could not be read",
        });
    };

    let ran_at = v.get("ran_at_unix").and_then(Value::as_i64);
    let get = |k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    json!({
        "ran": true,
        "rung": get("rung"),
        "ran_at_unix": ran_at,
        // Absent when the pass carried no timestamp; never a zero, which would
        // read as "just now".
        "age_s": ran_at.map(|t| (now_unix - t).max(0)),
        "reclaimed_bytes": get("reclaimed_bytes"),
        "reclaimed": get("reclaimed"),
        "reclaimable_bytes": get("reclaimable_bytes"),
        "reclaimable": get("reclaimable"),
        // The headline. Space is what breaks these nodes: the card fills, a
        // rewrite tears, the filesystem corrupts and the box will not boot.
        // Write rate is wear, which is slower and more forgiving, so it is
        // reported below this rather than above it.
        "footprint_bytes": get("footprint_bytes"),
        "footprint": get("footprint"),
        "budget_bytes": get("budget_bytes"),
        "caps": get("caps"),
        "over_cap": get("over_cap"),
        // `/opt/ados`: reported so the figure an operator sizes a card against
        // is complete, excluded from the budget because it is the installed
        // product rather than accumulation — a release shipping a bigger model
        // must not silently eat the allowance for recordings.
        "installed_bytes": get("installed_bytes"),
        "reason": Value::Null,
    })
}

/// Where the janitor mirrors its last pass, honouring the run-dir override the
/// sibling daemons read so a rootless install resolves to its own run directory.
fn janitor_sidecar_path() -> PathBuf {
    match std::env::var_os("ADOS_RUN_DIR") {
        Some(dir) => PathBuf::from(dir).join("janitor.json"),
        None => PathBuf::from("/run/ados/janitor.json"),
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Where the store lives, honouring the same override the daemon resolves with so
/// a test and a moved data root both land on the real directory.
fn store_dir() -> PathBuf {
    std::env::var("ADOS_LOGD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/ados/logd"))
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn fs_usage(path: &Path) -> Option<(u64, u64)> {
    let st = nix::sys::statvfs::statvfs(path).ok()?;
    let block = st.fragment_size() as u64;
    let total = st.blocks() as u64 * block;
    let avail = st.blocks_available() as u64 * block;
    Some((total, total.saturating_sub(avail)))
}

#[cfg(not(target_os = "linux"))]
fn fs_usage(_path: &Path) -> Option<(u64, u64)> {
    None
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests;
