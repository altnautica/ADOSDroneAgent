//! The reserved plugin display page: the page sidecar writer and the touch-zone
//! tap watcher.

use super::*;

// ---------------------------------------------------------------------
// Display page sidecar
// ---------------------------------------------------------------------

/// One label/value row of a plugin-contributed display page. The serde shape is
/// the contract `ados_display::sidecar::LcdPluginRow` reads; the display crate
/// is not a build dependency of the host, so the shape is shared by JSON, not a
/// shared type.
#[derive(serde::Serialize)]
pub(super) struct DisplayRow {
    pub(super) label: String,
    pub(super) value: String,
}

/// One declared touch zone on a plugin display page. Mirrors
/// `ados_display::sidecar::LcdPluginZone`.
#[derive(serde::Serialize)]
pub(super) struct DisplayZone {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) w: i32,
    pub(super) h: i32,
    pub(super) key: String,
    pub(super) label: String,
}

/// The full plugin display-page content. Mirrors
/// `ados_display::sidecar::LcdPluginPage`.
#[derive(serde::Serialize)]
pub(super) struct DisplayPage {
    pub(super) title: String,
    pub(super) rows: Vec<DisplayRow>,
    pub(super) zones: Vec<DisplayZone>,
}

/// Coerce a msgpack value to an i32 zone coordinate, defaulting to 0 for an
/// absent or non-numeric value (lenient, matching the display loader's
/// defaulted fields).
pub(super) fn zone_i32(value: Option<&Value>) -> i32 {
    value
        .and_then(|v| v.as_i64())
        .map(|n| n.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
        .unwrap_or(0)
}

/// Read a string field from a msgpack-map element, defaulting to an empty
/// string for an absent or non-string value.
pub(super) fn elem_str(value: &Value, key: &str) -> String {
    arg_str(value, key).unwrap_or("").to_string()
}

/// Most rows a plugin display page may carry. Kept in sync with
/// `ados_display::sidecar::PLUGIN_PAGE_MAX_ROWS` by value (the display crate is
/// not a build dependency). The panel shows about a dozen; the cap bounds what
/// every 1 Hz render reads and walks.
pub(super) const DISPLAY_PAGE_MAX_ROWS: usize = 32;

/// Most touch zones a plugin display page may declare. Kept in sync with
/// `ados_display::sidecar::PLUGIN_PAGE_MAX_ZONES`.
pub(super) const DISPLAY_PAGE_MAX_ZONES: usize = 16;

/// Longest title, row label or value, zone key or caption, in characters.
/// Kept in sync with `ados_display::sidecar::PLUGIN_PAGE_MAX_TEXT`.
pub(super) const DISPLAY_PAGE_MAX_TEXT: usize = 64;

/// Refuse a text field longer than [`DISPLAY_PAGE_MAX_TEXT`] characters.
fn check_text_len(field: &str, text: &str) -> Result<(), HostError> {
    if text.chars().count() > DISPLAY_PAGE_MAX_TEXT {
        return Err(HostError::Rpc(format!(
            "{field} longer than {DISPLAY_PAGE_MAX_TEXT} characters"
        )));
    }
    Ok(())
}

/// Refuse a list longer than `max` entries.
fn check_count(field: &str, len: usize, max: usize) -> Result<(), HostError> {
    if len > max {
        return Err(HostError::Rpc(format!(
            "{field} has more than {max} entries"
        )));
    }
    Ok(())
}

/// Parse a `display.page.set` request into the page content. Lenient by design:
/// `title`/`rows`/`zones` are all optional, a row defaults its label/value to
/// empty, and a zone defaults its coordinates to 0, so a partial payload still
/// produces a valid (possibly empty) page. A non-array `rows`/`zones` is
/// rejected so a misshaped request is a clear error rather than a silent empty,
/// and so is a page past the row, zone or text caps: the display renders every
/// page once a second, so its size is bounded here, at the gate.
pub(super) fn parse_display_page(args: &Value) -> Result<DisplayPage, HostError> {
    let title = arg_str(args, "title").unwrap_or("").to_string();
    check_text_len("title", &title)?;

    let rows = match map_get(args, "rows") {
        None | Some(Value::Nil) => Vec::new(),
        Some(Value::Array(items)) => {
            check_count("rows", items.len(), DISPLAY_PAGE_MAX_ROWS)?;
            items
                .iter()
                .map(|item| {
                    let row = DisplayRow {
                        label: elem_str(item, "label"),
                        value: elem_str(item, "value"),
                    };
                    check_text_len("row label", &row.label)?;
                    check_text_len("row value", &row.value)?;
                    Ok(row)
                })
                .collect::<Result<Vec<_>, HostError>>()?
        }
        Some(_) => return Err(HostError::Rpc("rows must be a list".to_string())),
    };

    let zones = match map_get(args, "zones") {
        None | Some(Value::Nil) => Vec::new(),
        Some(Value::Array(items)) => {
            check_count("zones", items.len(), DISPLAY_PAGE_MAX_ZONES)?;
            items
                .iter()
                .map(|item| {
                    let zone = DisplayZone {
                        x: zone_i32(map_get(item, "x")),
                        y: zone_i32(map_get(item, "y")),
                        w: zone_i32(map_get(item, "w")),
                        h: zone_i32(map_get(item, "h")),
                        key: elem_str(item, "key"),
                        label: elem_str(item, "label"),
                    };
                    check_text_len("zone key", &zone.key)?;
                    check_text_len("zone label", &zone.label)?;
                    Ok(zone)
                })
                .collect::<Result<Vec<_>, HostError>>()?
        }
        Some(_) => return Err(HostError::Rpc("zones must be a list".to_string())),
    };

    Ok(DisplayPage { title, rows, zones })
}

/// Serialize a display page to JSON and write `path` via an atomic
/// temp-then-rename, creating the parent. The sidecar is read by the display
/// service (a separate process), so it uses default file perms like the other
/// `/run/ados/lcd-*.json` sidecars, not the owner-only mode the config store
/// uses for its secret-bearing file.
pub(super) fn write_display_page(
    path: &std::path::Path,
    page: &DisplayPage,
) -> std::io::Result<()> {
    use std::io::Write;
    let json = serde_json::to_vec(page).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.flush()?;
        f.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_result;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// A process-global watcher for touch-zone taps on the reserved plugin OLED
/// page. The display sidecar (`/run/ados/lcd-plugin-tap.json`) receives a new
/// tap record each time an operator taps a zone; this watcher polls the file
/// (the writer uses tmp+rename, so the read is atomic) and on a change
/// broadcasts the tapped zone `key` to every subscriber. Follows
/// [`BUTTON_CLIENT`]: one poller for the whole host, N plugins share it.
/// Started on first `display.zone.subscribe`; a board with no display never
/// starts it.
pub(super) struct DisplayTapWatcher {
    pub(super) tx: broadcast::Sender<Vec<u8>>,
}

impl DisplayTapWatcher {
    pub(super) fn spawn() -> Self {
        let (tx, _rx) = broadcast::channel(DISPLAY_TAP_BROADCAST_DEPTH);
        let task_tx = tx.clone();
        let path = PathBuf::from(LCD_PLUGIN_TAP_PATH);
        // Seed with the record already on disk, read before the first
        // subscriber exists, so a tap left over from before this host started
        // is never delivered as a new one.
        let seen = read_tap_record_sync(&path);
        tokio::spawn(display_tap_poll_loop(task_tx, path, seen));
        Self { tx }
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.tx.subscribe()
    }
}

pub(super) static DISPLAY_TAP_CLIENT: std::sync::OnceLock<DisplayTapWatcher> =
    std::sync::OnceLock::new();

/// Parse a display-tap record into `(ts_ms, key)`. `None` for a malformed one.
pub(super) fn parse_tap_record(record: &str) -> Option<(u64, String)> {
    let v = serde_json::from_str::<serde_json::Value>(record).ok()?;
    Some((
        v.get("ts_ms")?.as_u64()?,
        v.get("key")?.as_str()?.to_string(),
    ))
}

/// The tap record currently on disk. One small tmpfs read, done once when the
/// watcher starts.
pub(super) fn read_tap_record_sync(path: &std::path::Path) -> Option<(u64, String)> {
    parse_tap_record(&std::fs::read_to_string(path).ok()?)
}

/// Poll the display-tap sidecar and broadcast each changed `key`, forever.
/// Tolerates an absent file (no display / no page yet) by polling quietly; a
/// malformed record is skipped. The read is atomic because the writer replaces
/// the file via tmp+rename, so we never observe a torn middle. `seen` is the
/// record present when the watcher started; a tap is emitted only when the
/// `(ts_ms, key)` pair moves off it.
pub(super) async fn display_tap_poll_loop(
    tx: broadcast::Sender<Vec<u8>>,
    path: PathBuf,
    mut seen: Option<(u64, String)>,
) {
    loop {
        if let Ok(record) = tokio::fs::read_to_string(&path).await {
            if let Some(tap) = parse_tap_record(&record) {
                if seen.as_ref() != Some(&tap) {
                    // `send` fails only when nobody is subscribed, the normal
                    // resting state; not an error. Lagged subscribers drop
                    // oldest by design (human-rate taps).
                    let _ = tx.send(tap.1.clone().into_bytes());
                    seen = Some(tap);
                }
            }
        }
        tokio::time::sleep(DISPLAY_TAP_POLL_INTERVAL).await;
    }
}

/// Canonical sidecar the reserved data-driven display page reads. Kept in sync
/// with `ados_display::sidecar::LCD_PLUGIN_PAGE_PATH` by the cross-crate JSON
/// shape, not a build dependency (the display crate is not on the host's
/// dependency path).
pub(super) const LCD_PLUGIN_PAGE_PATH: &str = "/run/ados/lcd-plugin-page.json";

/// Canonical sidecar the display writes each touch-zone tap on the reserved
/// plugin page to. Kept in sync with
/// `ados_display::sidecar::LCD_PLUGIN_TAP_PATH` by the cross-crate JSON shape,
/// not a build dependency (the display crate is not on the host's dependency
/// path).
pub(super) const LCD_PLUGIN_TAP_PATH: &str = "/run/ados/lcd-plugin-tap.json";

/// Broadcast depth of the display-tap watcher. Taps are human-rate; a subscriber
/// this far behind is wedged, and dropping the oldest beats growing without
/// bound. Matches the button bus depth.
pub(super) const DISPLAY_TAP_BROADCAST_DEPTH: usize = 64;

/// Poll interval for the display-tap watcher. A tap is a human event, so 100 ms
/// is far faster than the operator; the read is atomic (the writer tmp+renames).
pub(super) const DISPLAY_TAP_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(100);
