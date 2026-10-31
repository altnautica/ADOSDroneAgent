use super::*;

// The full-range portrait corners for clarity in the rotation tests.
const X: AxisRange = AxisRange { min: 0, max: 4095 };
const Y: AxisRange = AxisRange { min: 0, max: 4095 };

// The rotation-aware fallback transform applied to a raw corner. Full-range
// ranges make the renormalization a no-op, so this exercises the bare
// `identity_for` matrix for each rotation.
fn corner(x_raw: i32, y_raw: i32, rotation: i32) -> (i32, i32) {
    TouchTransform::fallback(rotation, X, Y).map_to_lcd(x_raw, y_raw)
}

#[test]
fn norm_clamps_and_handles_degenerate_range() {
    let r = AxisRange { min: 100, max: 200 };
    assert_eq!(r.norm(100), 0.0);
    assert_eq!(r.norm(200), 1.0);
    assert_eq!(r.norm(150), 0.5);
    // Below/above the range clamp to the ends.
    assert_eq!(r.norm(50), 0.0);
    assert_eq!(r.norm(500), 1.0);
    // Degenerate (min == max) never divides by zero.
    let z = AxisRange { min: 7, max: 7 };
    assert_eq!(z.norm(7), 0.0);
    assert_eq!(z.norm(999), 0.0);
}

#[test]
fn rotation_0_is_a_straight_scale() {
    // identity_for(0): x = sx*x_raw, y = sy*y_raw. The far corners scale to
    // exactly LCD_W / LCD_H, which the clamp pins to the last valid pixel.
    assert_eq!(corner(0, 0, 0), (0, 0));
    assert_eq!(corner(4095, 0, 0), (LCD_W - 1, 0));
    assert_eq!(corner(0, 4095, 0), (0, LCD_H - 1));
    assert_eq!(corner(4095, 4095, 0), (LCD_W - 1, LCD_H - 1));
    // centre: sx*2047=239.88 -> 240; sy*2047=159.92 -> 160.
    assert_eq!(corner(2047, 2047, 0), (240, 160));
}

#[test]
fn rotation_90_swaps_axes_and_flips_y() {
    // identity_for(90): x = (LCD_W/span)*y_raw, y = LCD_H - (LCD_H/span)*x_raw.
    assert_eq!(corner(0, 0, 90), (0, LCD_H - 1));
    assert_eq!(corner(4095, 0, 90), (0, 0));
    assert_eq!(corner(0, 4095, 90), (LCD_W - 1, LCD_H - 1));
    assert_eq!(corner(4095, 4095, 90), (LCD_W - 1, 0));
    // centre: x=(480/span)*2047=239.88 -> 240; y=320-159.92=160.08 -> 160.
    assert_eq!(corner(2047, 2047, 90), (240, 160));
}

#[test]
fn rotation_180_inverts_both_axes() {
    // identity_for(180): x = LCD_W - sx*x_raw, y = LCD_H - sy*y_raw.
    assert_eq!(corner(0, 0, 180), (LCD_W - 1, LCD_H - 1));
    assert_eq!(corner(4095, 0, 180), (0, LCD_H - 1));
    assert_eq!(corner(0, 4095, 180), (LCD_W - 1, 0));
    assert_eq!(corner(4095, 4095, 180), (0, 0));
    // centre: both axes flipped -> 480-239.88=240.12 -> 240, 320-159.92=160.08 -> 160.
    assert_eq!(corner(2047, 2047, 180), (240, 160));
}

#[test]
fn rotation_270_swaps_axes_and_flips_x() {
    // identity_for(270): x = LCD_W - (LCD_W/span)*y_raw, y = (LCD_H/span)*x_raw.
    assert_eq!(corner(0, 0, 270), (LCD_W - 1, 0));
    assert_eq!(corner(4095, 0, 270), (LCD_W - 1, LCD_H - 1));
    assert_eq!(corner(0, 4095, 270), (0, 0));
    assert_eq!(corner(4095, 4095, 270), (0, LCD_H - 1));
    // centre: x=480-239.88=240.12 -> 240; y=159.92 -> 160.
    assert_eq!(corner(2047, 2047, 270), (240, 160));
}

#[test]
fn out_of_range_rotation_falls_back_to_zero() {
    // identity_for() collapses any rotation that is not a cardinal multiple
    // of 90 (after rem_euclid(360)) to rotation 0. (360 -> 0; 1000 -> 280;
    // -315 -> 45; 137 stays 137 — all non-cardinal -> rotation 0.)
    for bad in [45, 137, 280, 360, 1000, -315] {
        assert_eq!(corner(0, 0, bad), corner(0, 0, 0));
        assert_eq!(corner(4095, 4095, bad), corner(4095, 4095, 0));
        assert_eq!(corner(1234, 2345, bad), corner(1234, 2345, 0));
    }
}

#[test]
fn negative_270_equivalent_to_90() {
    // rem_euclid normalizes -270 to 90.
    assert_eq!(corner(1000, 2000, -270), corner(1000, 2000, 90));
}

#[test]
fn output_is_always_in_bounds() {
    // Sweep a grid across the raw range for every rotation; nothing escapes
    // the landscape surface, including the max corner (which the clamp pins
    // to W-1 / H-1 rather than W / H).
    for rot in [0, 90, 180, 270] {
        let t = TouchTransform::fallback(rot, X, Y);
        for &xr in &[0, 1, 2047, 4094, 4095, 5000] {
            for &yr in &[0, 1, 2047, 4094, 4095, 5000] {
                let (x, y) = t.map_to_lcd(xr, yr);
                assert!((0..LCD_W).contains(&x), "x={x} rot={rot}");
                assert!((0..LCD_H).contains(&y), "y={y} rot={rot}");
            }
        }
    }
}

#[test]
fn non_full_ranges_normalize_before_placing() {
    // A driver that reports 200..3900 still maps its own min/max to the LCD
    // corners: the fallback renormalizes the driver span into the canonical
    // range before the identity matrix places it.
    let xr = AxisRange {
        min: 200,
        max: 3900,
    };
    let yr = AxisRange {
        min: 150,
        max: 3950,
    };
    let t = TouchTransform::fallback(0, xr, yr);
    // Rotation-0 straight scale: (min,min) -> origin, (max,max) -> far corner.
    assert_eq!(t.map_to_lcd(200, 150), (0, 0));
    assert_eq!(t.map_to_lcd(3900, 3950), (LCD_W - 1, LCD_H - 1));
}

#[test]
fn calibrated_transform_applies_matrix_to_raw_directly() {
    // A loaded calibration is applied to raw counts without renormalizing.
    // A simple scale-only affine (a=sx, e=sy) maps the raw corners to the
    // LCD bounds, which the clamp pins to the last valid pixel.
    let m = affine::identity_for(0, (LCD_W, LCD_H));
    let t = TouchTransform::calibrated(m);
    assert_eq!(t.map_to_lcd(0, 0), (0, 0));
    assert_eq!(t.map_to_lcd(4095, 4095), (LCD_W - 1, LCD_H - 1));
    // The driver ranges are ignored on this path: the same raw point lands
    // in the same place regardless of what the panel published.
    assert_eq!(t.map_to_lcd(2047, 2047), (240, 160));
}

#[test]
fn load_transform_falls_back_when_no_calib_file() {
    // No file at the path -> the rotation-aware identity fallback.
    let t = load_transform(std::path::Path::new("/nonexistent/touch.calib"), 0, X, Y);
    assert_eq!(t, TouchTransform::fallback(0, X, Y));
}
