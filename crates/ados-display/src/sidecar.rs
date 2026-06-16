//! `/run/ados` LCD sidecars.
//!
//! * `lcd-state.json` (`active_page_id`) — read to restore the page on restart.
//! * `lcd-page-request.json` (`page`) — a remote page-switch request; read then
//!   unlinked so the same request can't reapply (mirrors the Python watcher).
//! * `lcd-latency.json` — the fb-writer stats snapshot, written atomically at
//!   1 Hz so the diagnostics surface sees the SPI throughput. Atomic writes
//!   model `ados-video`'s `camera_state.rs` (tmp sibling + fsync + rename).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fb_writer::WriterStats;

/// Canonical sidecar paths (`core/paths.py`).
pub const LCD_STATE_PATH: &str = "/run/ados/lcd-state.json";
pub const LCD_PAGE_REQUEST_PATH: &str = "/run/ados/lcd-page-request.json";
pub const LCD_LATENCY_PATH: &str = "/run/ados/lcd-latency.json";
/// Data-driven content for the reserved `plugin` page. A plugin contributes its
/// page (title, label/value rows, optional touch zones) by writing this file;
/// the `plugin` page reads it each render so a plugin can present a status
/// surface without recompiling the display service. Absent file = an empty
/// placeholder page with no zones.
pub const LCD_PLUGIN_PAGE_PATH: &str = "/run/ados/lcd-plugin-page.json";
/// PNG of the most recently rendered panel frame. The native page UI writes it
/// after each render so the REST snapshot endpoint can serve exactly what the
/// LCD shows without re-reading the framebuffer or depending on PIL.
pub const LCD_SNAPSHOT_PATH: &str = "/run/ados/lcd-snapshot.png";

/// Encode an RGB888 panel frame to PNG and atomically write it to `path`.
///
/// `rgb888` is tightly packed `width * height * 3` bytes (the canvas's native
/// layout, exactly what `Canvas::as_rgb888` returns). The PNG is full panel
/// resolution; the GCS preview scales it client-side. Best-effort: an encode or
/// I/O error is returned for the caller to log and discard, the same contract as
/// the other sidecars.
pub fn write_snapshot_png(
    path: &Path,
    rgb888: &[u8],
    width: u32,
    height: u32,
) -> std::io::Result<()> {
    let expected = width as usize * height as usize * 3;
    if rgb888.len() != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "snapshot frame {} bytes != expected {expected} for {width}x{height}",
                rgb888.len()
            ),
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        writer
            .write_image_data(rgb888)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }
    atomic_write(path, &buf)
}

/// `lcd-state.json` payload. `active_page_id` is the page to restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdState {
    #[serde(default)]
    pub active_page_id: Option<String>,
}

impl LcdState {
    /// Read the persisted active page id. `None` on missing/malformed file.
    pub fn load(path: &Path) -> Option<LcdState> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// `lcd-page-request.json` payload. `page` is the requested page id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdPageRequest {
    #[serde(default)]
    pub page: Option<String>,
}

/// Read a pending page request, then unlink the file so the same request never
/// reapplies. Returns the requested page id when one is present and non-empty.
/// Any malformed payload is dropped and the file is still unlinked, matching the
/// Python watcher's best-effort drain.
pub fn take_page_request(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let parsed = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<LcdPageRequest>(&t).ok());
    // Always unlink, whether the payload parsed or not.
    let _ = std::fs::remove_file(path);
    let page = parsed.and_then(|r| r.page)?;
    let page = page.trim().to_string();
    if page.is_empty() {
        None
    } else {
        Some(page)
    }
}

/// One label/value row of a plugin-contributed page. Both fields default to an
/// empty string so a partial row still deserializes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdPluginRow {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub value: String,
}

/// One declared touch zone on a plugin page. The rectangle is in page-local
/// content coordinates (origin at the top-left of the content region, the same
/// convention every page's hit zones use). `key` is the stable action id the
/// owning plugin interprets; `label` is the on-screen caption for the zone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdPluginZone {
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default)]
    pub w: i32,
    #[serde(default)]
    pub h: i32,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub label: String,
}

/// `lcd-plugin-page.json` payload — the data-driven content for the reserved
/// `plugin` page. Every field is optional/defaulted so a partial or evolving
/// payload still loads (lenient by design); unknown fields are ignored. The
/// reserved page renders the `title` and `rows`, and maps each `zone` to a
/// touch target the navigator surfaces back to the owning plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdPluginPage {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub rows: Vec<LcdPluginRow>,
    #[serde(default)]
    pub zones: Vec<LcdPluginZone>,
}

impl LcdPluginPage {
    /// Read the plugin page content. `None` on a missing or malformed file, so
    /// the reserved page falls back to its placeholder rather than failing.
    pub fn load(path: &Path) -> Option<LcdPluginPage> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Atomically write this page content to `path` (tmp sibling + fsync +
    /// rename), creating the parent. Best-effort: an I/O error is returned for
    /// the caller to log and discard, the same contract as the other sidecars.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        atomic_write(path, &body)
    }
}

/// `lcd-latency.json` payload — the fb-writer observability snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LcdLatency {
    pub writes: u64,
    pub drops: u64,
    pub skipped_duplicates: u64,
    pub last_write_ms: Option<f64>,
}

impl From<WriterStats> for LcdLatency {
    fn from(s: WriterStats) -> Self {
        Self {
            writes: s.writes,
            drops: s.drops,
            skipped_duplicates: s.skipped_duplicates,
            last_write_ms: s.last_write_ms,
        }
    }
}

impl LcdLatency {
    /// Atomically write the snapshot to `path` (tmp sibling + fsync + rename),
    /// creating the parent. Best-effort: an I/O error is returned for the caller
    /// to log and discard.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let body = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        atomic_write(path, &body)
    }
}

/// Atomic tmp-sibling write (tmp name disambiguated by pid).
fn atomic_write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ados-lcd-sidecar");
    let tmp = parent.join(format!("{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcd_state_loads_active_page_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-state.json");
        std::fs::write(&path, r#"{"active_page_id":"dashboard"}"#).unwrap();
        assert_eq!(
            LcdState::load(&path).unwrap().active_page_id.as_deref(),
            Some("dashboard")
        );
    }

    #[test]
    fn lcd_state_missing_or_malformed_is_none() {
        assert!(LcdState::load(Path::new("/nonexistent/lcd-state.json")).is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-state.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(LcdState::load(&path).is_none());
    }

    #[test]
    fn page_request_returns_page_and_unlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-page-request.json");
        std::fs::write(&path, r#"{"page":"radio"}"#).unwrap();
        assert_eq!(take_page_request(&path).as_deref(), Some("radio"));
        // The file is gone after a read.
        assert!(!path.exists());
        // A second read returns None.
        assert!(take_page_request(&path).is_none());
    }

    #[test]
    fn page_request_trims_and_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-page-request.json");
        std::fs::write(&path, r#"{"page":"  network  "}"#).unwrap();
        assert_eq!(take_page_request(&path).as_deref(), Some("network"));

        std::fs::write(&path, r#"{"page":"   "}"#).unwrap();
        assert!(take_page_request(&path).is_none());
        assert!(!path.exists());
    }

    #[test]
    fn page_request_malformed_payload_still_unlinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-page-request.json");
        std::fs::write(&path, "garbage").unwrap();
        assert!(take_page_request(&path).is_none());
        // Best-effort drain: the bad file is removed so the watcher doesn't loop.
        assert!(!path.exists());
    }

    #[test]
    fn page_request_missing_page_key_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-page-request.json");
        std::fs::write(&path, r#"{"other":"x"}"#).unwrap();
        assert!(take_page_request(&path).is_none());
        assert!(!path.exists());
    }

    #[test]
    fn latency_writes_from_stats_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-latency.json");
        let stats = WriterStats {
            writes: 42,
            drops: 3,
            skipped_duplicates: 100,
            last_write_ms: Some(12.34),
        };
        let lat: LcdLatency = stats.into();
        lat.write_to(&path).unwrap();

        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(v["writes"], 42);
        assert_eq!(v["drops"], 3);
        assert_eq!(v["skipped_duplicates"], 100);
        assert_eq!(v["last_write_ms"], 12.34);
        // No stray tmp left behind.
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray);
    }

    #[test]
    fn snapshot_png_round_trips_to_a_decodable_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-snapshot.png");
        // 2x2 RGB888: red, green, blue, white.
        let rgb: Vec<u8> = vec![
            255, 0, 0, // (0,0) red
            0, 255, 0, // (1,0) green
            0, 0, 255, // (0,1) blue
            255, 255, 255, // (1,1) white
        ];
        write_snapshot_png(&path, &rgb, 2, 2).unwrap();

        // Decode it back with the png crate and confirm the geometry + pixels.
        let decoder = png::Decoder::new(std::fs::File::open(&path).unwrap());
        let mut reader = decoder.read_info().unwrap();
        let info = reader.info();
        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);
        assert_eq!(info.color_type, png::ColorType::Rgb);
        let mut out = vec![0u8; reader.output_buffer_size()];
        let frame = reader.next_frame(&mut out).unwrap();
        assert_eq!(&out[..frame.buffer_size()], rgb.as_slice());

        // No stray tmp left behind.
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray);
    }

    #[test]
    fn snapshot_png_rejects_a_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-snapshot.png");
        // 3 bytes is not 2x2x3, so the encode is refused and nothing is written.
        let err = write_snapshot_png(&path, &[1, 2, 3], 2, 2).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!path.exists());
    }

    #[test]
    fn plugin_page_round_trips_through_write_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-plugin-page.json");
        let page = LcdPluginPage {
            title: "Sensor Status".to_string(),
            rows: vec![
                LcdPluginRow {
                    label: "Temp".to_string(),
                    value: "42 C".to_string(),
                },
                LcdPluginRow {
                    label: "State".to_string(),
                    value: "ok".to_string(),
                },
            ],
            zones: vec![LcdPluginZone {
                x: 8,
                y: 40,
                w: 100,
                h: 32,
                key: "reset".to_string(),
                label: "Reset".to_string(),
            }],
        };
        page.write_to(&path).unwrap();
        let loaded = LcdPluginPage::load(&path).unwrap();
        assert_eq!(loaded, page);

        // No stray tmp left behind.
        let stray = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
        assert!(!stray);
    }

    #[test]
    fn plugin_page_loads_the_host_written_json_shape() {
        // This is the exact JSON the plugin host writes (field names + nesting).
        // It is the cross-crate contract: the host has no build dependency on
        // this crate, so the shapes are pinned by this round-trip, not a type.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-plugin-page.json");
        std::fs::write(
            &path,
            r#"{"title":"Sensor","rows":[{"label":"Temp","value":"42 C"}],"zones":[{"x":8,"y":40,"w":100,"h":32,"key":"reset","label":"Reset"}]}"#,
        )
        .unwrap();
        let loaded = LcdPluginPage::load(&path).unwrap();
        assert_eq!(loaded.title, "Sensor");
        assert_eq!(
            loaded.rows,
            vec![LcdPluginRow {
                label: "Temp".into(),
                value: "42 C".into()
            }]
        );
        assert_eq!(
            loaded.zones,
            vec![LcdPluginZone {
                x: 8,
                y: 40,
                w: 100,
                h: 32,
                key: "reset".into(),
                label: "Reset".into()
            }]
        );
    }

    #[test]
    fn plugin_page_missing_or_malformed_is_none() {
        assert!(LcdPluginPage::load(Path::new("/nonexistent/lcd-plugin-page.json")).is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-plugin-page.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(LcdPluginPage::load(&path).is_none());
    }

    #[test]
    fn plugin_page_lenient_defaults_for_partial_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-plugin-page.json");
        // Only a title; rows/zones absent, and an unknown field is ignored.
        std::fs::write(&path, r#"{"title":"Just A Title","extra":123}"#).unwrap();
        let loaded = LcdPluginPage::load(&path).unwrap();
        assert_eq!(loaded.title, "Just A Title");
        assert!(loaded.rows.is_empty());
        assert!(loaded.zones.is_empty());

        // A row missing its value still deserializes to an empty value.
        std::fs::write(&path, r#"{"rows":[{"label":"L"}]}"#).unwrap();
        let loaded = LcdPluginPage::load(&path).unwrap();
        assert_eq!(loaded.rows.len(), 1);
        assert_eq!(loaded.rows[0].label, "L");
        assert_eq!(loaded.rows[0].value, "");

        // An empty object is a valid empty page.
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(
            LcdPluginPage::load(&path).unwrap(),
            LcdPluginPage::default()
        );
    }

    #[test]
    fn latency_null_last_write_ms_serializes_null() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lcd-latency.json");
        let lat = LcdLatency {
            writes: 0,
            drops: 0,
            skipped_duplicates: 0,
            last_write_ms: None,
        };
        lat.write_to(&path).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(v["last_write_ms"].is_null());
    }
}
