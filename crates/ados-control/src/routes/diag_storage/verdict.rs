//! Storage facts and the wear verdict: throttle history, store and filesystem facts.

use super::*;

/// Which throttle conditions the board has hit *at any point* in the retained
/// window.
///
/// The Pi's bitfield carries both live bits (0-3) and sticky "has occurred" bits
/// (16-19). Undervoltage is usually transient — a single poll almost always
/// misses it, which is why the one reading ever taken proved nothing. The sticky
/// bits are the record, so they are what this reports.
pub(super) fn throttle_history(rows: &[HwRow]) -> Value {
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
pub(super) fn store_facts(dir: &Path, enabled: bool) -> Value {
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
        "enabled": enabled,
        "path": dir.to_string_lossy(),
        "live_bytes": live,
        "wal_bytes": wal,
        "quarantined": quarantined,
        "quarantined_bytes": quarantined_bytes,
    })
}

/// Capacity of the mount the store sits on. `None` throughout when `statvfs`
/// could not read it, never a zero.
pub(super) fn filesystem_facts(fs: Option<(u64, u64)>) -> Value {
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
pub(super) fn verdict(
    write: &Measurement,
    filesystem: &Value,
    throttle: &Value,
) -> (&'static str, String) {
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
            // "No throttle events recorded" is a claim about something that was
            // looked at. The sticky bits live only in the logging store, so with
            // the store off nobody looked, and saying it anyway would be exactly
            // the fabricated clean bill of health this surface exists to refuse.
            let looked = throttle.get("supported").and_then(Value::as_bool).is_some();
            if looked {
                (
                    "ok",
                    format!("writing {kb:.0} KB/s sustained; no throttle events recorded"),
                )
            } else {
                (
                    "ok",
                    format!(
                        "writing {kb:.0} KB/s sustained; power and thermal history was not \
                         checked (it is recorded only by the logging store, which is off)"
                    ),
                )
            }
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
