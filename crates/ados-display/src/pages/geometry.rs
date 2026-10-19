//! Panel geometry: the landscape panel size, the chrome bands, the bottom-bar
//! tab sizing and the dashboard tile grid. Re-exported from [`crate::pages`].

/// Full panel width in landscape orientation.
pub const PANEL_W: u32 = 480;
/// Full panel height in landscape orientation.
pub const PANEL_H: u32 = 320;

/// Height of the persistent top status bar.
pub const TOP_BAR_H: u32 = 32;
/// Height of the persistent bottom tab bar.
pub const BOTTOM_BAR_H: u32 = 44;

/// Width of the page content region (full panel width).
pub const CONTENT_W: u32 = PANEL_W;
/// Height of the page content region: panel minus top bar and bottom bar.
pub const CONTENT_H: u32 = PANEL_H - TOP_BAR_H - BOTTOM_BAR_H;
/// Top edge (in panel-global y) of the page content region.
pub const CONTENT_Y: u32 = TOP_BAR_H;

/// Most tabs the bottom bar holds (`480 / 5` = 96 px each).
pub const MAX_TABS: usize = 5;
/// The ground-station bottom-bar tabs, left to right, and the tab list of every
/// node whose panel config does not name its own. Each id is both the page the
/// tab routes to and the icon it paints. The navigator owns the live tab list;
/// the bar painter and the tap router both read it.
pub const GS_TAB_IDS: [&str; MAX_TABS] = [
    "dashboard",
    "video",
    "settings",
    "link_stats",
    "channel_hops",
];

/// Width of one bottom-bar tab when the bar holds `count` tabs. Index `i` owns
/// the horizontal band `[i*w, (i+1)*w)`; `count` is clamped to `1..=MAX_TABS`,
/// so every supported count divides the 480 px panel exactly.
pub fn tab_width(count: usize) -> i32 {
    PANEL_W as i32 / count.clamp(1, MAX_TABS) as i32
}

/// Outer margin and inter-tile gap for the dashboard's inset 2x2 grid.
pub const TILE_OUTER_MARGIN: i32 = 8;
/// Gap between adjacent dashboard tiles.
pub const TILE_GAP: i32 = 8;

/// A single tile rectangle in page-local coordinates (origin at the top-left of
/// the content region, not the panel-global origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The four dashboard tile rectangles in page-local content coordinates.
///
/// Order is top-left, top-right, bottom-left, bottom-right — matching the
/// `(top_left, top_right, bottom_left, bottom_right)` router order the
/// dashboard paints in. Tile size derives from the same outer-margin + gap math
/// the inset dashboard uses: `(CONTENT_W - 2*margin - gap) / 2` wide by
/// `(CONTENT_H - 2*margin - gap) / 2` tall.
pub fn tile_rects() -> [TileRect; 4] {
    let tile_w = (CONTENT_W as i32 - TILE_OUTER_MARGIN * 2 - TILE_GAP) / 2;
    let tile_h = (CONTENT_H as i32 - TILE_OUTER_MARGIN * 2 - TILE_GAP) / 2;
    let col_a = TILE_OUTER_MARGIN;
    let col_b = TILE_OUTER_MARGIN + tile_w + TILE_GAP;
    let row_a = TILE_OUTER_MARGIN;
    let row_b = TILE_OUTER_MARGIN + tile_h + TILE_GAP;
    [
        TileRect {
            x: col_a,
            y: row_a,
            w: tile_w,
            h: tile_h,
        },
        TileRect {
            x: col_b,
            y: row_a,
            w: tile_w,
            h: tile_h,
        },
        TileRect {
            x: col_a,
            y: row_b,
            w: tile_w,
            h: tile_h,
        },
        TileRect {
            x: col_b,
            y: row_b,
            w: tile_w,
            h: tile_h,
        },
    ]
}
