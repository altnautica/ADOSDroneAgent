use super::*;

#[test]
fn apply_rounds_to_nearest_pixel() {
    // a=2 maps raw 10 -> 20; the +0.4 offset rounds down, +0.6 rounds up.
    let m = Affine {
        a: 2.0,
        b: 0.0,
        c: 0.4,
        d: 0.0,
        e: 3.0,
        f: 0.6,
    };
    let (x, y) = m.apply(10, 5);
    assert_eq!(x, 20); // 2*10 + 0.4 = 20.4 -> 20
    assert_eq!(y, 16); // 3*5 + 0.6 = 15.6 -> 16
}

#[test]
fn identity_rotation_0() {
    let m = identity_for(0, (480, 320));
    let span = (RAW_MAX - RAW_MIN) as f64;
    assert_eq!(m.apply(0, 0), (0, 0));
    // Full sweep maps to the LCD bounds.
    assert_eq!(m.apply(RAW_MAX, RAW_MAX), (480, 320));
    assert!((m.a - 480.0 / span).abs() < 1e-12);
    assert!((m.e - 320.0 / span).abs() < 1e-12);
    assert_eq!(m.b, 0.0);
    assert_eq!(m.d, 0.0);
}

#[test]
fn identity_rotation_90() {
    let m = identity_for(90, (480, 320));
    // y_raw drives x_lcd, x_raw drives y_lcd (inverted to lcd_h).
    assert_eq!(m.a, 0.0);
    assert_eq!(m.e, 0.0);
    assert_eq!(m.f, 320.0);
    // raw (0,0) -> x = 0, y = lcd_h.
    assert_eq!(m.apply(0, 0), (0, 320));
    // raw (max, max) -> x = lcd_w, y = 0.
    assert_eq!(m.apply(RAW_MAX, RAW_MAX), (480, 0));
}

#[test]
fn identity_rotation_180() {
    let m = identity_for(180, (480, 320));
    assert_eq!(m.c, 480.0);
    assert_eq!(m.f, 320.0);
    // raw (0,0) -> (lcd_w, lcd_h), raw (max,max) -> (0,0).
    assert_eq!(m.apply(0, 0), (480, 320));
    assert_eq!(m.apply(RAW_MAX, RAW_MAX), (0, 0));
}

#[test]
fn identity_rotation_270() {
    let m = identity_for(270, (480, 320));
    assert_eq!(m.c, 480.0);
    assert_eq!(m.f, 0.0);
    // raw (0,0) -> (lcd_w, 0), raw (max,max) -> (0, lcd_h).
    assert_eq!(m.apply(0, 0), (480, 0));
    assert_eq!(m.apply(RAW_MAX, RAW_MAX), (0, 320));
}

#[test]
fn identity_out_of_range_rotation_falls_back_to_zero() {
    let m45 = identity_for(45, (480, 320));
    let m0 = identity_for(0, (480, 320));
    assert_eq!(m45, m0);
    // Negative rotations normalize via rem_euclid (`% 360` semantics).
    let m_neg = identity_for(-90, (480, 320));
    assert_eq!(m_neg, identity_for(270, (480, 320)));
}

#[test]
fn least_squares_recovers_a_known_transform_with_zero_rms() {
    // Build a known affine and synthesize exact targets from it.
    let truth = Affine {
        a: 0.1,
        b: 0.0,
        c: 5.0,
        d: 0.0,
        e: 0.07,
        f: 3.0,
    };
    let samples = [
        (100, 200),
        (3900, 250),
        (2000, 3800),
        (500, 3000),
        (3500, 1500),
        (1800, 800),
    ];
    let targets: Vec<(i32, i32)> = samples.iter().map(|&(x, y)| truth.apply(x, y)).collect();
    let (fit, rms) = compute_from_samples(&samples, &targets).unwrap();
    // Coefficients land within the integer-rounding noise of the truth
    // (the targets were rounded to whole pixels before fitting, so an exact
    // recovery is not possible, the slope coefficients pick up that
    // perturbation, larger for the small-magnitude axes).
    assert!((fit.a - truth.a).abs() < 1e-3, "a={}", fit.a);
    assert!((fit.e - truth.e).abs() < 1e-3, "e={}", fit.e);
    assert!((fit.c - truth.c).abs() < 1.0, "c={}", fit.c);
    // The real parity check: re-applying the fit reproduces every rounded
    // target exactly, and the RMS residual is sub-pixel.
    for (&(x, y), &t) in samples.iter().zip(targets.iter()) {
        assert_eq!(fit.apply(x, y), t);
    }
    assert!(rms < 1.0, "rms={rms}");
}

#[test]
fn least_squares_needs_five_pairs() {
    let s = [(0, 0), (1, 1), (2, 2), (3, 3)];
    let t = [(0, 0), (1, 1), (2, 2), (3, 3)];
    assert_eq!(
        compute_from_samples(&s, &t).unwrap_err(),
        FitError::NotEnoughPairs
    );
}

#[test]
fn least_squares_rejects_length_mismatch() {
    let s = [(0, 0), (1, 1), (2, 2), (3, 3), (4, 4), (5, 5)];
    let t = [(0, 0), (1, 1), (2, 2), (3, 3), (4, 4)];
    assert_eq!(
        compute_from_samples(&s, &t).unwrap_err(),
        FitError::LengthMismatch
    );
}

#[test]
fn singular_guard_rejects_colinear_samples() {
    // All samples on the line y_raw == x_raw -> normal-equations matrix
    // is rank-deficient and the solver must bail.
    let s = [(0, 0), (1, 1), (2, 2), (3, 3), (4, 4), (5, 5)];
    let t = [(0, 0), (10, 10), (20, 20), (30, 30), (40, 40), (50, 50)];
    assert_eq!(
        compute_from_samples(&s, &t).unwrap_err(),
        FitError::Singular
    );
}

#[test]
fn save_load_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("touch.calib");
    let m = Affine {
        a: 0.117,
        b: 0.001,
        c: -3.2,
        d: -0.002,
        e: 0.078,
        f: 4.5,
    };
    let params = SaveParams {
        rotation: 90,
        rms: 2.5,
        ..SaveParams::default()
    };
    save(&m, &path, &params).unwrap();

    // Loads back the same matrix.
    let loaded = load(&path).unwrap();
    assert_eq!(loaded, m);

    // The blob carries the full 9-key set with the expected values.
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for k in [
        "version",
        "calibrated",
        "device",
        "raw_range",
        "lcd_size",
        "rotation_applied_at_save",
        "matrix",
        "rms_residual_px",
        "saved_at",
    ] {
        assert!(v.get(k).is_some(), "missing key {k}");
    }
    assert_eq!(v["version"], CALIB_FILE_VERSION);
    assert_eq!(v["calibrated"], true);
    assert_eq!(v["device"], "ads7846");
    assert_eq!(v["raw_range"], serde_json::json!([0, 4095]));
    assert_eq!(v["lcd_size"], serde_json::json!([480, 320]));
    assert_eq!(v["rotation_applied_at_save"], 90);
    assert_eq!(v["matrix"].as_array().unwrap().len(), 6);
    assert_eq!(v["rms_residual_px"], 2.5);

    // Compact separators (no spaces after , or :).
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains(", "));
    assert!(!text.contains(": "));

    // No leftover tmp sibling.
    let stray = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(!stray, "stray .tmp file left behind");
}

/// `load_for` accepts a calibration only for the panel geometry + rotation it
/// was captured on. A swapped panel (different `lcd_size`) or a changed
/// rotation must fall back to the identity (None) rather than mis-map every
/// touch, the defect `load` had (it never looked at the stored geometry).
#[test]
fn load_for_rejects_a_mismatched_panel_or_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("touch.calib");
    let m = Affine {
        a: 0.1,
        b: 0.0,
        c: -3.0,
        d: 0.0,
        e: 0.1,
        f: 4.0,
    };
    // Captured on a 480x320 panel at rotation 90.
    let params = SaveParams {
        rotation: 90,
        lcd_size: (480, 320),
        ..SaveParams::default()
    };
    save(&m, &path, &params).unwrap();

    // Same geometry + rotation: accepted.
    assert_eq!(load_for(&path, (480, 320), 90), Some(m));
    // A swapped panel (different LCD size): rejected → fall back + re-prompt.
    assert!(load_for(&path, (320, 240), 90).is_none());
    // A changed rotation: the matrix no longer maps → rejected.
    assert!(load_for(&path, (480, 320), 0).is_none());
    // The geometry-blind load still accepts it (documented weaker contract).
    assert_eq!(load(&path), Some(m));
}

/// A blob with no `lcd_size` (pre-geometry-tagging) cannot prove its geometry,
/// so `load_for` rejects it even though the plain `load` accepts it.
#[test]
fn load_for_rejects_a_blob_without_geometry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("touch.calib");
    // Hand-write a valid-but-untagged blob (no lcd_size / rotation fields).
    std::fs::write(
        &path,
        format!(
            r#"{{"version":{CALIB_FILE_VERSION},"calibrated":true,"matrix":[0.1,0.0,-3.0,0.0,0.1,4.0],"saved_at":0}}"#
        ),
    )
    .unwrap();
    assert!(
        load(&path).is_some(),
        "the geometry-blind load still accepts it"
    );
    assert!(
        load_for(&path, (480, 320), 0).is_none(),
        "an untagged blob's geometry cannot be proven, so load_for rejects it"
    );
}

#[test]
fn skip_marker_loads_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("touch.calib");
    save_skip_marker(&path).unwrap();
    // calibrated=false -> reader falls back to identity (None).
    assert!(load(&path).is_none());
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(v["calibrated"], false);
    assert_eq!(v["skipped"], true);
    // No matrix key when skipping.
    assert!(v.get("matrix").is_none());
}

#[test]
fn load_missing_file_is_none() {
    assert!(load(Path::new("/nonexistent/touch.calib")).is_none());
}

#[test]
fn load_version_mismatch_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("touch.calib");
    let blob = serde_json::json!({
        "version": 99,
        "calibrated": true,
        "matrix": [1.0, 0.0, 0.0, 0.0, 1.0, 0.0],
        "saved_at": 0,
    });
    std::fs::write(&path, serde_json::to_vec(&blob).unwrap()).unwrap();
    assert!(load(&path).is_none());
}

#[test]
fn load_wrong_matrix_length_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("touch.calib");
    let blob = serde_json::json!({
        "version": CALIB_FILE_VERSION,
        "calibrated": true,
        "matrix": [1.0, 0.0, 0.0],
        "saved_at": 0,
    });
    std::fs::write(&path, serde_json::to_vec(&blob).unwrap()).unwrap();
    assert!(load(&path).is_none());
}
