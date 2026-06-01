//! Display layer for the ground-station SPI LCD and OLED.
//!
//! * [`conf`] — `/etc/ados/display.conf` parsing + `read_rotation`.
//! * [`fb_geometry`] — `/sys/class/graphics` geometry + driver-name discovery
//!   and the SPI-LCD framebuffer match (skips the primary HDMI/DRM surface).
//! * [`pack`] — RGB565 / RGB888 / xRGB32 pixel packers.
//! * [`probe`] — the boot-time apply-verify-auto-revert presence probe.
//! * [`fb_writer`] — the off-thread framebuffer writer: single-slot latest-wins,
//!   dedicated OS thread, duplicate-skip on raw-input hash, stats, and a
//!   join-before-release teardown. The load-bearing fidelity surface.
//! * [`oled`] — SSD1306 / SH1106 monochrome OLED over raw `/dev/i2c-N` ioctls
//!   (no external driver-crate stack); the page packing is pure.
//! * [`sidecar`] — `/run/ados` LCD sidecars (lcd-state read, page-request
//!   read+unlink, latency snapshot write).
//!
//! The native-resolution page UI is being moved into this crate so the LCD
//! render path is dependency-light pure Rust end to end:
//!
//! * [`graphics`] — the color palette, the embedded TrueType faces + glyph
//!   rasterizer, the draw primitives, and the composite widgets (sparkline,
//!   bar meter, status dot, QR) the pages paint with.
//! * [`widgets`] — the shared page chrome (status bars, tiles, big numbers).
//! * [`pages`] — the full-panel page composers.
//! * [`navigator`] — the page-navigation state machine driven by buttons/touch.
//! * [`render_loop`] — the tick loop that paints the active page and feeds the
//!   off-thread framebuffer writer.
//!
//! mDNS and the FastAPI display routes stay in Python; the byte-level write path
//! and the page render both live here.

pub mod conf;
pub mod fb_geometry;
pub mod fb_writer;
pub mod graphics;
pub mod navigator;
pub mod oled;
pub mod pack;
pub mod pages;
pub mod probe;
pub mod render_loop;
pub mod sidecar;
pub mod state_source;
pub mod widgets;
