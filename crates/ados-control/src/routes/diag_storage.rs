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

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::ipc::logd_client::HwRow;
use crate::state::AppState;

mod janitor;
mod rate;
mod verdict;

pub use janitor::janitor_facts;
use janitor::*;
pub use rate::parse_diskstats;
use rate::*;
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

pub async fn get_storage_diagnostics(State(state): State<AppState>) -> Response {
    // Take the direct measurement first. It costs one window of wall clock and
    // it is the reading that does not depend on anything else on the box being
    // alive, which is the whole point of it.
    let live = sample_write_window(Path::new(DISKSTATS), LIVE_WINDOW).await;
    let rows = state.logd.hw_rows(WEAR_ROWS).await;
    // The config read, the store-directory walk, `statvfs` and the janitor
    // sidecar read are blocking filesystem work, so they run off the runtime.
    let assembled = tokio::task::spawn_blocking(move || {
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
        body
    })
    .await;
    match assembled {
        Ok(body) => Json(body).into_response(),
        Err(e) => crate::routes::detail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("storage diagnostics failed: {e}"),
        ),
    }
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
