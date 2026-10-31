use super::bars::format_ram;
use super::*;
use crate::graphics::palette::DARK;
use crate::pages::blank_panel;
use crate::pages::{tab_width, BOTTOM_BAR_H, PANEL_W, TOP_BAR_H};

#[test]
fn format_ram_renders_gigabytes() {
    assert_eq!(format_ram(Some(1234.0), Some(16384.0)), "1.2/16G");
    assert_eq!(format_ram(None, Some(16384.0)), "-");
    assert_eq!(format_ram(Some(1234.0), Some(0.0)), "-");
}

#[test]
fn top_bar_inks_the_band() {
    let mut c = blank_panel(&DARK);
    draw_top_bar(
        &mut c,
        &DARK,
        "gs-example",
        "receiver",
        Some(22.0),
        Some(1234.0),
        Some(16384.0),
        Some(47.0),
        "13:47:23",
    );
    // The divider row is the border color at the bottom edge of the bar.
    assert_eq!(c.pixel(0, TOP_BAR_H as i32 - 1), DARK.border_default);
}

/// The accent line sits over the active cell only, for the five ground
/// station tabs and for a shorter list whose cells are wider.
#[test]
fn bottom_bar_accents_the_active_cell() {
    let y = crate::pages::PANEL_H as i32 - BOTTOM_BAR_H as i32;
    for (tabs, active) in [
        (&crate::pages::GS_TAB_IDS[..], 4usize),
        (&["plugin", "cameras", "storage"][..], 1),
    ] {
        let mut c = blank_panel(&DARK);
        draw_bottom_bar(&mut c, &DARK, tabs, tabs[active]);
        let w = tab_width(tabs.len());
        assert_eq!(w * tabs.len() as i32, PANEL_W as i32);
        for i in 0..tabs.len() as i32 {
            let accent = c.pixel(i * w + w / 2, y + 1) == DARK.accent_primary;
            assert_eq!(accent, i == active as i32, "{tabs:?} cell {i}");
        }
    }
}

#[test]
fn tile_body_sits_below_the_title_bar() {
    let mut c = blank_panel(&DARK);
    let (bx, by, bw, bh) = draw_tile(&mut c, &DARK, 8, 8, 228, 110, "Radio link", "ch 161");
    assert_eq!(bx, 16);
    assert_eq!(by, 8 + TILE_TITLE_BAR_H + 2);
    assert_eq!(bw, 228 - 16);
    assert_eq!(bh, 110 - TILE_TITLE_BAR_H - 10);
}

#[test]
fn big_number_reports_a_positive_width() {
    let mut c = blank_panel(&DARK);
    let w = draw_big_number(
        &mut c,
        16,
        40,
        "87",
        DARK.status_success,
        28,
        "%",
        DARK.text_secondary,
    );
    assert!(w > 0);
}
