//! Calibration file load and atomic save.

use super::*;

/// Read and validate the on-disk blob: present, parseable, the current file
/// version, and marked `calibrated`. `None` (fall back to identity) otherwise.
fn read_valid_blob(path: &Path) -> Option<CalibBlob> {
    let text = std::fs::read_to_string(path).ok()?;
    let blob: CalibBlob = serde_json::from_str(&text).ok()?;
    if blob.version != CALIB_FILE_VERSION || !blob.calibrated {
        return None;
    }
    Some(blob)
}

/// The 2x3 affine from a validated blob, or `None` when the matrix is absent or
/// the wrong length.
fn blob_affine(blob: &CalibBlob) -> Option<Affine> {
    let matrix = blob.matrix.as_ref()?;
    if matrix.len() != 6 {
        return None;
    }
    Affine::from_list(matrix)
}

/// Read the persisted affine from `path`, WITHOUT checking the panel geometry.
///
/// Returns `None` when the file is missing, malformed, marked
/// `calibrated=false` (a skip marker), or the version field does not match this
/// build. The caller falls back to [`identity_for`] in any of those cases.
/// Prefer [`load_for`] on the render path, a calibration is only valid for the
/// panel + rotation it was captured on.
pub fn load(path: &Path) -> Option<Affine> {
    blob_affine(&read_valid_blob(path)?)
}

/// Read the persisted affine only if it was captured on THIS panel geometry and
/// rotation.
///
/// A calibration matrix is fit against one panel's raw ADC range mapped to one
/// LCD size at one rotation. Swap the panel (a different `lcd_size`) or change
/// the display rotation and the stored matrix mis-maps every touch by tens of
/// pixels, silently, because [`load`] never looked. This gate rejects a
/// calibration whose saved `lcd_size` or `rotation_applied_at_save` does not
/// match what the reader is running now, so the caller falls back to the
/// rotation-aware identity and the wizard re-prompts rather than the operator
/// fighting a mis-aligned screen. A blob that predates geometry tagging (no
/// `lcd_size`) is also rejected, its geometry cannot be proven.
pub fn load_for(path: &Path, expected_lcd: (i32, i32), expected_rotation: i32) -> Option<Affine> {
    let blob = read_valid_blob(path)?;
    if blob.lcd_size != Some([expected_lcd.0, expected_lcd.1]) {
        return None;
    }
    if blob.rotation_applied_at_save != Some(expected_rotation.rem_euclid(360)) {
        return None;
    }
    blob_affine(&blob)
}

/// Persist the affine atomically to `path`.
///
/// The blob captures version, the device hint, raw ADC range, the LCD geometry
/// the calibration was taken at, the rotation that was applied, the matrix
/// itself, the RMS residual, and a UNIX timestamp. Atomic write via tmpfile +
/// fsync + rename so a power loss mid-save can never half-write the file.
pub fn save(affine: &Affine, path: &Path, params: &SaveParams) -> std::io::Result<()> {
    let blob = CalibBlob {
        version: CALIB_FILE_VERSION,
        calibrated: true,
        device: Some(params.device.clone()),
        raw_range: Some([params.raw_range.0, params.raw_range.1]),
        lcd_size: Some([params.lcd_size.0, params.lcd_size.1]),
        rotation_applied_at_save: Some(params.rotation.rem_euclid(360)),
        matrix: Some(affine.to_list().to_vec()),
        rms_residual_px: Some(params.rms),
        skipped: None,
        saved_at: now_unix_secs(),
    };
    atomic_write_json(path, &blob)
}

/// Persist a marker that says the operator chose to skip calibration.
///
/// The reader treats this the same as "no calibration file" and uses the
/// identity transform, but the marker stops the wizard from auto-launching
/// every boot.
pub fn save_skip_marker(path: &Path) -> std::io::Result<()> {
    let blob = CalibBlob {
        version: CALIB_FILE_VERSION,
        calibrated: false,
        device: None,
        raw_range: None,
        lcd_size: None,
        rotation_applied_at_save: None,
        matrix: None,
        rms_residual_px: None,
        skipped: Some(true),
        saved_at: now_unix_secs(),
    };
    atomic_write_json(path, &blob)
}

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn atomic_write_json(path: &Path, blob: &CalibBlob) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Compact separators.
    let body = serde_json::to_vec(blob).map_err(std::io::Error::other)?;

    // tmp sibling in the same directory so the rename is atomic on the same
    // filesystem. Name disambiguated by pid so two writers do not collide.
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("touch.calib");
    let tmp = parent.join(format!("{}.{}.tmp", file_name, std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(&body)?;
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
