//! Native-resolution LCD draw layer.
//!
//! This module group is the Rust replacement for the on-device page UI: the
//! color palette, the TrueType font rasterizer, the small draw primitives
//! (rect/line/text), and the composite widgets (sparkline, bar meter, status
//! dot, QR). Everything paints onto an in-memory RGB888 canvas that the
//! existing [`crate::pack`] packers convert to the panel's bit depth and the
//! [`crate::fb_writer`] thread blits to `/dev/fbN`.
//!
//! The submodules:
//!
//! * [`palette`] — the two named color sets (dark / light) and the
//!   threshold-color helper that maps a numeric value to success/warning/error.
//! * [`fonts`] — the embedded DejaVu faces (sans + mono, regular + bold) and a
//!   glyph rasterizer that reports the same text metrics the page layout was
//!   tuned against.
//! * [`primitives`] — filled rectangles, hairlines, and text onto the canvas.
//! * [`sparkline`] — a polyline trend graph with gap-on-missing-sample.
//! * [`bar_meter`] — a chipped horizontal fill meter.
//! * [`status_dot`] — a filled status circle.
//! * [`qr`] — a QR matrix rasterizer for pair URLs / short codes.

pub mod bar_meter;
pub mod fonts;
pub mod palette;
pub mod primitives;
pub mod qr;
pub mod sparkline;
pub mod status_dot;
