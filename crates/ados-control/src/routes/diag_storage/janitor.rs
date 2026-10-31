//! The disk janitor's last pass, read back from its sidecar.

use super::*;

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
pub(super) fn janitor_sidecar_path() -> PathBuf {
    match std::env::var_os("ADOS_RUN_DIR") {
        Some(dir) => PathBuf::from(dir).join("janitor.json"),
        None => PathBuf::from("/run/ados/janitor.json"),
    }
}

pub(super) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
