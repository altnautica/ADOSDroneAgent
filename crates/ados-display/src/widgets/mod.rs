//! Composite chrome widgets shared across pages.
//!
//! Where [`crate::graphics`] holds the narrow draw primitives (rect, line,
//! text, sparkline, bar, dot, QR), this module holds the larger reusable chrome
//! the pages compose from: the top status bar (hostname, role badge, system
//! metrics, clock), the bottom tab bar (one icon cell per tab + active accent),
//! the bordered content tile, and the headline big number with a unit suffix.
//!
//! The geometry, colors, fonts, and text match the on-device layout one for one
//! so a render is pixel-faithful regardless of which side produces it.

// The chrome helpers carry the panel layout's full parameter sets (a tile's
// rect plus its two captions, a big number's value plus color/size/unit). The
// extra arguments are the layout contract, not accidental sprawl, so the
// argument-count lint is opted out per helper at its definition.

mod bars;
mod detail;
mod tab_icons;
mod tiles;

pub use bars::{draw_bottom_bar, draw_tab_pulse, draw_top_bar};
pub use detail::{
    detail_back_zone, draw_ack_line, draw_detail_header, ACK_LINE_H, DETAIL_HEADER_H,
};
pub use tiles::{
    draw_big_number, draw_label, draw_tile, TileBody, TILE_TITLE_BAR_H, TILE_TITLE_PAD_X,
};

#[cfg(test)]
mod tests;
