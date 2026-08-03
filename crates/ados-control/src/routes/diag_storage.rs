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

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::ipc::logd_client::HwRow;
use crate::AppState;

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
    let rows = state.logd.hw_rows(WEAR_ROWS).await;
    let store_dir = store_dir();
    let mut body = build(rows.as_deref(), &store_dir, fs_usage(&store_dir));
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "janitor".to_string(),
            janitor_facts(&janitor_sidecar_path(), now_unix()),
        );
    }
    Json(body)
}

/// Assemble the verdict from the three independent inputs, kept as one pure
/// function so every branch — including "the store is down" — is testable without
/// a rig, a socket, or a filesystem.
pub fn build(rows: Option<&[HwRow]>, store_dir: &Path, fs: Option<(u64, u64)>) -> Value {
    let write = match rows {
        Some(rows) => write_rate(rows),
        None => Measurement::unavailable("the logging store did not answer"),
    };
    let throttle = match rows {
        Some(rows) => throttle_history(rows),
        None => json!({
            "supported": null,
            "reason": "the logging store did not answer",
        }),
    };
    let store = store_facts(store_dir);
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
}

impl Measurement {
    fn unavailable(reason: &str) -> Self {
        Self {
            kb_s: None,
            window_s: None,
            device: None,
            reason: Some(reason.to_string()),
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "kb_per_s": self.kb_s.map(round1),
            "gb_per_day": self.kb_s.map(|k| round1(k * 86_400.0 / 1_048_576.0)),
            "window_s": self.window_s.map(round1),
            "device": self.device,
            "reason": self.reason,
        })
    }
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
        };
    }

    let kb = (last.1 - first.1) * SECTOR_BYTES / 1024.0;
    Measurement {
        kb_s: Some(kb / window_s),
        window_s: Some(window_s),
        device: Some(device),
        reason: None,
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
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Which throttle conditions the board has hit *at any point* in the retained
/// window.
///
/// The Pi's bitfield carries both live bits (0-3) and sticky "has occurred" bits
/// (16-19). Undervoltage is usually transient — a single poll almost always
/// misses it, which is why the one reading ever taken proved nothing. The sticky
/// bits are the record, so they are what this reports.
fn throttle_history(rows: &[HwRow]) -> Value {
    let raw: Vec<u64> = rows
        .iter()
        .filter_map(|r| r.num("throttle.raw"))
        .map(|v| v as u64)
        .collect();

    if raw.is_empty() {
        return json!({
            "supported": false,
            "reason": "this board does not report a throttle bitfield",
        });
    }

    let union = raw.iter().fold(0u64, |acc, v| acc | v);
    let bit = |n: u32| union & (1 << n) != 0;

    json!({
        "supported": true,
        "samples": raw.len(),
        "raw_union": format!("0x{union:x}"),
        "undervoltage_occurred": bit(16),
        "arm_frequency_capped_occurred": bit(17),
        "throttling_occurred": bit(18),
        "soft_temperature_limit_occurred": bit(19),
        "undervoltage_now": bit(0),
        "throttled_now": bit(2),
        "clean": union == 0,
    })
}

/// The store's own footprint, including any quarantined corpse.
///
/// A torn store is renamed aside rather than deleted, so a box that has corrupted
/// twice carries three copies. That is the mechanism that filled the card, and it
/// is invisible unless something totals it.
fn store_facts(dir: &Path) -> Value {
    let live = file_len(&dir.join("logs.db"));
    let wal = file_len(&dir.join("logs.db-wal"));

    let mut quarantined = 0u64;
    let mut quarantined_bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("logs.db.corrupt-") {
                quarantined += 1;
                quarantined_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }

    json!({
        "path": dir.to_string_lossy(),
        "live_bytes": live,
        "wal_bytes": wal,
        "quarantined": quarantined,
        "quarantined_bytes": quarantined_bytes,
    })
}

/// Capacity of the mount the store sits on. `None` throughout when `statvfs`
/// could not read it, never a zero.
fn filesystem_facts(fs: Option<(u64, u64)>) -> Value {
    let Some((total, used)) = fs else {
        return json!({
            "total_bytes": null,
            "used_bytes": null,
            "used_pct": null,
            "reason": "could not read the filesystem capacity",
        });
    };
    let used_pct = if total > 0 {
        Some(round1(used as f64 * 100.0 / total as f64))
    } else {
        None
    };
    json!({
        "total_bytes": total,
        "used_bytes": used,
        "used_pct": used_pct,
        "reason": null,
    })
}

/// The single word an operator reads, and why it says that.
///
/// Ordered worst-first. "unknown" is a real outcome, distinct from "ok": a box
/// whose store is down has not been proven healthy.
fn verdict(write: &Measurement, filesystem: &Value, throttle: &Value) -> (&'static str, String) {
    let used_pct = filesystem.get("used_pct").and_then(Value::as_f64);
    if let Some(pct) = used_pct {
        if pct >= CRITICAL_FS_USED_PCT {
            return (
                "critical",
                format!(
                    "the filesystem is {pct:.0}% full; a store rewrite needs scratch space it \
                     no longer has, which is the step that leaves the card unbootable"
                ),
            );
        }
    }

    match write.kb_s {
        Some(kb) if kb >= CRITICAL_KB_S => (
            "critical",
            format!(
                "writing {kb:.0} KB/s sustained ({:.0} GB/day); this is the rate that wore a \
                 card out in under two days",
                kb * 86_400.0 / 1_048_576.0
            ),
        ),
        Some(kb) if kb >= WEARING_KB_S => (
            "wearing",
            format!(
                "writing {kb:.0} KB/s sustained ({:.0} GB/day); higher than an idle agent needs",
                kb * 86_400.0 / 1_048_576.0
            ),
        ),
        Some(kb) => {
            let undervoltage = throttle
                .get("undervoltage_occurred")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if undervoltage {
                return (
                    "wearing",
                    format!(
                        "write rate is fine ({kb:.0} KB/s) but the board has recorded an \
                         undervoltage; a browning-out supply corrupts cards independently of wear"
                    ),
                );
            }
            (
                "ok",
                format!("writing {kb:.0} KB/s sustained; no throttle events recorded"),
            )
        }
        None => (
            "unknown",
            write
                .reason
                .clone()
                .unwrap_or_else(|| "write rate could not be measured".to_string()),
        ),
    }
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
    json!({
        "ran": true,
        "rung": v.get("rung").cloned().unwrap_or(Value::Null),
        "ran_at_unix": ran_at,
        // Absent when the pass carried no timestamp; never a zero, which would
        // read as "just now".
        "age_s": ran_at.map(|t| (now_unix - t).max(0)),
        "reclaimed_bytes": v.get("reclaimed_bytes").cloned().unwrap_or(Value::Null),
        "reclaimed": v.get("reclaimed").cloned().unwrap_or(Value::Null),
        "reclaimable_bytes": v.get("reclaimable_bytes").cloned().unwrap_or(Value::Null),
        "reclaimable": v.get("reclaimable").cloned().unwrap_or(Value::Null),
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
mod tests {
    use super::*;
    use serde_json::Map;

    /// Build a window of snapshots one second apart carrying a write counter that
    /// advances by `per_tick` sectors.
    fn rows_with_counter(count: usize, per_tick: f64, device: &str) -> Vec<HwRow> {
        (0..count)
            .map(|i| {
                let mut signals = Map::new();
                signals.insert(
                    format!("disk.{device}.wr_sectors"),
                    json!(1_000.0 + per_tick * i as f64),
                );
                HwRow {
                    ts_us: 1_000_000 * i as i64,
                    signals,
                }
            })
            .collect()
    }

    #[test]
    fn write_rate_is_a_delta_across_the_window_not_a_reading() {
        // 2 sectors/s = 1 KB/s.
        let rows = rows_with_counter(120, 2.0, "mmcblk0");
        let m = write_rate(&rows);
        assert_eq!(m.kb_s.map(round1), Some(1.0));
        assert_eq!(m.device.as_deref(), Some("mmcblk0"));
        assert!(m.reason.is_none());
    }

    #[test]
    fn a_counter_that_went_backwards_refuses_rather_than_reporting_a_negative_rate() {
        let mut rows = rows_with_counter(120, 2.0, "mmcblk0");
        // The box rebooted: the kernel counter restarts near zero.
        let last = rows.last_mut().unwrap();
        last.signals
            .insert("disk.mmcblk0.wr_sectors".into(), json!(5.0));

        let m = write_rate(&rows);
        assert_eq!(m.kb_s, None);
        assert!(
            m.reason.as_deref().unwrap().contains("rebooted"),
            "reason should name the restart, got {:?}",
            m.reason
        );
    }

    #[test]
    fn a_window_too_short_to_be_a_rate_refuses() {
        let rows = rows_with_counter(5, 2.0, "mmcblk0");
        let m = write_rate(&rows);
        assert_eq!(m.kb_s, None);
        assert!(m.reason.as_deref().unwrap().contains("too short"));
    }

    #[test]
    fn the_busiest_device_wins_over_an_idle_one() {
        let mut rows = rows_with_counter(120, 2.0, "mmcblk0");
        for (i, row) in rows.iter_mut().enumerate() {
            // A second device that barely moves.
            row.signals.insert(
                "disk.sda.wr_sectors".into(),
                json!(50_000.0 + 0.01 * i as f64),
            );
        }
        assert_eq!(busiest_device(&rows).as_deref(), Some("mmcblk0"));
    }

    #[test]
    fn throttle_reports_sticky_bits_no_single_poll_would_catch() {
        // Every sample looks clean except one, which recorded that an
        // undervoltage had occurred (bit 16).
        let mut rows = rows_with_counter(10, 2.0, "mmcblk0");
        rows[0].signals.insert("throttle.raw".into(), json!(0));
        rows[4]
            .signals
            .insert("throttle.raw".into(), json!(1u64 << 16));
        for row in rows.iter_mut().skip(5) {
            row.signals.insert("throttle.raw".into(), json!(0));
        }

        let t = throttle_history(&rows);
        assert_eq!(t["undervoltage_occurred"], json!(true));
        assert_eq!(t["undervoltage_now"], json!(false));
        assert_eq!(t["clean"], json!(false));
    }

    #[test]
    fn a_board_without_a_throttle_bitfield_says_unsupported_not_clean() {
        let rows = rows_with_counter(10, 2.0, "mmcblk0");
        let t = throttle_history(&rows);
        assert_eq!(t["supported"], json!(false));
        assert!(t.get("clean").is_none(), "absence must not read as clean");
    }

    #[test]
    fn an_unreachable_store_is_unknown_never_ok() {
        let out = build(None, Path::new("/nonexistent"), None);
        assert_eq!(out["verdict"], json!("unknown"));
        assert_eq!(out["write"]["kb_per_s"], json!(null));
    }

    #[test]
    fn the_measured_pre_fix_rate_reads_critical() {
        // 1 714 KB/s, the rate measured on a test node before the retention fix.
        let rows = rows_with_counter(120, 1_714.0 * 1024.0 / 512.0, "mmcblk0");
        let out = build(Some(&rows), Path::new("/nonexistent"), None);
        assert_eq!(out["verdict"], json!("critical"));
        assert!(out["reason"].as_str().unwrap().contains("wore a card out"));
    }

    #[test]
    fn a_quiet_agent_on_a_healthy_card_reads_ok() {
        let rows = rows_with_counter(120, 2.0, "mmcblk0");
        let out = build(Some(&rows), Path::new("/nonexistent"), Some((100, 20)));
        assert_eq!(out["verdict"], json!("ok"));
    }

    #[test]
    fn a_full_filesystem_outranks_a_healthy_write_rate() {
        let rows = rows_with_counter(120, 2.0, "mmcblk0");
        let out = build(Some(&rows), Path::new("/nonexistent"), Some((100, 95)));
        assert_eq!(out["verdict"], json!("critical"));
        assert!(out["reason"].as_str().unwrap().contains("full"));
    }

    // --- the janitor's record ------------------------------------------------

    #[test]
    fn no_janitor_pass_reads_as_unknown_never_as_nothing_to_reclaim() {
        let out = janitor_facts(Path::new("/no/such/janitor.json"), 1_000);
        assert_eq!(out["ran"], json!(false));
        // The distinction that matters: a box nobody has swept must not report
        // zero reclaimable, which would read as "there is nothing to give".
        assert_eq!(out["reclaimable_bytes"], json!(null));
        assert_eq!(out["reclaimed_bytes"], json!(null));
        assert!(out["reason"]
            .as_str()
            .unwrap()
            .contains("has not completed"));
    }

    #[test]
    fn a_corrupt_record_says_so_rather_than_inventing_figures() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("janitor.json");
        std::fs::write(&path, b"{ truncated").unwrap();
        let out = janitor_facts(&path, 1_000);
        assert_eq!(out["ran"], json!(false));
        assert_eq!(out["reclaimable_bytes"], json!(null));
        assert!(out["reason"]
            .as_str()
            .unwrap()
            .contains("could not be read"));
    }

    #[test]
    fn the_last_pass_is_reported_with_its_age_so_staleness_is_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("janitor.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version": 1,
                "rung": "pressure",
                "reclaimed_bytes": 366_002_176u64,
                "reclaimed": { "apt_archives": 195_035_136u64, "apt_lists": 170_967_040u64 },
                "reclaimable_bytes": 1_048_576u64,
                "reclaimable": { "quarantined_stores": 1_048_576u64 },
                "free_pct": 18.5,
                "ran_at_unix": 1_700_000_000i64,
            }))
            .unwrap(),
        )
        .unwrap();

        let out = janitor_facts(&path, 1_700_002_400);
        assert_eq!(out["ran"], json!(true));
        assert_eq!(out["rung"], json!("pressure"));
        assert_eq!(out["reclaimed_bytes"], json!(366_002_176u64));
        assert_eq!(out["reclaimable_bytes"], json!(1_048_576u64));
        assert_eq!(out["reclaimed"]["apt_archives"], json!(195_035_136u64));
        // Forty minutes ago — the reader can see the figures are not live.
        assert_eq!(out["age_s"], json!(2_400));
        assert_eq!(out["reason"], json!(null));
    }

    #[test]
    fn a_record_without_a_timestamp_has_no_age_rather_than_a_zero_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("janitor.json");
        std::fs::write(&path, br#"{"rung":"routine","reclaimed_bytes":0}"#).unwrap();
        let out = janitor_facts(&path, 1_700_000_000);
        assert_eq!(out["ran"], json!(true));
        assert_eq!(
            out["age_s"],
            json!(null),
            "an unknown age is not 'just now'"
        );
    }

    #[test]
    fn a_recorded_undervoltage_downgrades_an_otherwise_healthy_box() {
        let mut rows = rows_with_counter(120, 2.0, "mmcblk0");
        rows[3]
            .signals
            .insert("throttle.raw".into(), json!(1u64 << 16));
        let out = build(Some(&rows), Path::new("/nonexistent"), Some((100, 20)));
        assert_eq!(out["verdict"], json!("wearing"));
        assert!(out["reason"].as_str().unwrap().contains("undervoltage"));
    }
}
