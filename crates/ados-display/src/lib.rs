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
//! The PIL page UI, mDNS, and the FastAPI display routes stay in Python; this
//! crate is the byte-level write path the Python render sidecar feeds.

pub mod conf;
pub mod fb_geometry;
pub mod fb_writer;
pub mod oled;
pub mod pack;
pub mod probe;
pub mod sidecar;
