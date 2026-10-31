//! Display layer for the ground-station SPI LCD and OLED.
//!
//! * [`conf`] — `/etc/ados/display.conf` parsing + `read_rotation`.
//! * [`probe`] — the boot-time apply-verify-auto-revert presence probe.
//! * The framebuffer geometry discovery, pixel packers, off-thread framebuffer
//!   writer and SSD1306 / SH1106 OLED transport live in [`ados_display_driver`].
//! * [`oled_frame`] — the status OLED's pure frame builder (hostname, IP,
//!   link, role lines), shared by the daemon and the `render_oled` example.
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
//! * [`panel_action`] — performs the agent writes the panel's controls ask for.
//! * [`panel_conf`] — the panel's page set from the node profile and the
//!   persisted `ground_station.ui.screens` order / enabled lists.
//! * [`render_loop`] — the tick loop that paints the active page and feeds the
//!   off-thread framebuffer writer.
//! * [`touch_input`] — the touchscreen evdev reader: raw ADS7846 ABS samples ->
//!   rotated LCD pixels -> classified gestures posted to the render loop. The
//!   coordinate transform is pure; only the node drain is Linux-gated.
//! * [`touch_supervisor`] — the never-give-up discover/drain loop around it.
//! * [`state_poller`] — the off-loop agent-state poller the render loop reads.
//! * [`kiosk`] — the HDMI kiosk: a full-screen browser on the cockpit, under
//!   cage or inside a running desktop session (`ados-kiosk`).
//!
//! mDNS and the FastAPI display routes stay in Python; the byte-level write path
//! and the page render both live here.

pub mod button_source;
pub mod calibration;
pub mod conf;
pub mod daemon;
pub mod graphics;
pub mod host_identity;
pub mod kiosk;
pub mod log_tail;
pub mod navigator;
pub mod oled_frame;
pub mod oled_link;
pub mod pages;
pub mod panel_action;
pub mod panel_conf;
pub mod probe;
pub mod render_loop;
pub mod sidecar;
pub mod state_poller;
pub mod state_source;
pub mod touch_input;
pub mod touch_supervisor;
pub mod widgets;
