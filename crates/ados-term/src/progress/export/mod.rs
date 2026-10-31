//! Terminal-UI asset export: turn a real composed grid (from
//! [`crate::frame::compose`] or [`crate::progress::fullscreen::sample_grid`])
//! into standalone SVG and HTML assets for documentation. Because it consumes
//! the same styled grid the live terminal renders, the asset IS the real UI.

pub mod ansi_cells;
pub mod html;
pub mod svg;

pub use html::{to_document, to_player, to_pre};
pub use svg::to_svg;
