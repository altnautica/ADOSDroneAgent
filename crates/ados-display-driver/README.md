# ados-display-driver

[![crates.io](https://img.shields.io/crates/v/ados-display-driver.svg)](https://crates.io/crates/ados-display-driver)
[![docs.rs](https://img.shields.io/docsrs/ados-display-driver)](https://docs.rs/ados-display-driver)
[![license](https://img.shields.io/crates/l/ados-display-driver.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-blue.svg)](https://github.com/altnautica/ADOSDroneAgent/blob/main/crates/Cargo.toml)

**Linux output for small displays: SSD1306/SH1106 OLED over I2C, RGB565/RGB888/xRGB pixel packers and an off-thread latest-wins framebuffer writer.**

Small SPI LCDs behind the Linux fbtft driver and I2C OLEDs are slow devices: a full-frame write can take tens of milliseconds, and doing it on a render loop or an async runtime worker stalls everything else. This crate gives you the output half of such a display: find the right framebuffer in sysfs, pack an RGB888 canvas to the panel's bit depth, and hand the frame to a dedicated writer thread that keeps only the newest frame, skips duplicates and pushes only the bytes that changed. For OLEDs it drives SSD1306 and SH1106 controllers over `/dev/i2c-N` with no embedded-hal stack.

```rust
use ados_display_driver::fb_writer::{FbWriter, Frame, VirtualSink};
use ados_display_driver::pack::pack_for_bpp;

// A 4x2 RGB888 canvas: one red pixel, the rest black.
let mut rgb = vec![0u8; 4 * 2 * 3];
rgb[..3].copy_from_slice(&[0xFF, 0x00, 0x00]);

// Pack it for a 16 bpp panel: RGB565, little-endian.
let packed = pack_for_bpp(&rgb, 16).expect("16 bpp is supported");
assert_eq!(&packed[..2], &[0x00, 0xF8]);

// Hand it to the writer thread. VirtualSink writes a file; on Linux,
// MmapSink writes the real /dev/fbN mapping instead.
let path = std::env::temp_dir().join("ados-display-driver-readme.fb");
let mut writer = FbWriter::spawn(VirtualSink::open(&path));
writer.present(Frame::new(packed, &rgb));

// cleanup() drains the pending frame, then joins the thread.
writer.cleanup();
assert_eq!(writer.stats().writes, 1);
assert_eq!(std::fs::read(&path).unwrap().len(), 4 * 2 * 2);
std::fs::remove_file(&path).unwrap();
```

## Highlights

- One runtime dependency (`tracing`); `memmap2` and `libc` are pulled in on Linux only.
- The packers, the OLED page layout, the upload plan, the writer thread model and the sysfs discovery are portable and build on any host; only `MmapSink` and `I2cDev` are Linux-only.
- Single-slot latest-wins writer on a dedicated OS thread: [`FbWriter::present`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_writer/struct.FbWriter.html#method.present) never blocks on the device.
- `MmapSink` copies and flushes only the changed byte span of each frame, so fbtft pushes only the touched scanlines over SPI.
- `VirtualSink` replaces its file atomically (temp file plus rename), so a concurrent reader never sees a torn frame.
- 47 unit tests: the writer thread model against a fake sink, the OLED framing against a fake bus, the packers, sysfs discovery against temp trees, and a concurrent torn-read test for `VirtualSink`.

## How it works

The crate has four modules.

### [`fb_geometry`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_geometry/index.html): which framebuffer, at what size

[`match_framebuffer`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_geometry/fn.match_framebuffer.html) walks the `fbN` entries under a sysfs root (normally `/sys/class/graphics`) and returns the first one whose driver name is acceptable and whose bit depth is 16, 24 or 32. With an expected name configured, the driver name must contain it; otherwise it must be one of the known fbtft SPI-LCD drivers in [`SPI_LCD_DRIVER_NAMES`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_geometry/constant.SPI_LCD_DRIVER_NAMES.html), so the primary HDMI/DRM framebuffer is never picked by accident. Geometry comes from `virtual_size` plus `bits_per_pixel`, falling back to the legacy `var` blob. The sysfs root is a parameter, so the whole module is testable against a temp directory.

### [`pack`](https://docs.rs/ados-display-driver/latest/ados_display_driver/pack/index.html): RGB888 to the panel's bit depth

Input is a tightly-packed RGB888 slice. [`pack_for_bpp`](https://docs.rs/ados-display-driver/latest/ados_display_driver/pack/fn.pack_for_bpp.html) dispatches on bit depth:

|bpp|Output|Layout per pixel|
|---|---|---|
|16|RGB565 little-endian|`((r & 0xF8) << 8) \| ((g & 0xFC) << 3) \| (b >> 3)`, low byte first|
|24|RGB888 passthrough|`R, G, B`|
|32|xRGB32|`B, G, R, 0`|

Any other depth returns `None`, so the caller can drop the frame.

### [`fb_writer`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_writer/index.html): the off-thread writer

[`FbWriter::spawn`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_writer/struct.FbWriter.html#method.spawn) moves a [`FrameSink`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_writer/trait.FrameSink.html) onto a thread named `fb-writer` that owns it for its whole life.

- **Latest wins.** `present` stashes the frame in a single slot and returns. A frame still pending when the next one arrives is overwritten and counted as a drop.
- **Duplicate skip.** Each [`Frame`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_writer/struct.Frame.html) carries a hash of its raw, pre-pack input. A frame whose hash matches the last successful write is skipped. Callers can ask `is_last(hash)` before packing at all.
- **Failures do not freeze the panel.** A failed write is logged with `tracing` and the last-written hash is left unchanged, so the next frame is written even if its input is the same.
- **Side jobs.** `request_snapshot` queues a boxed closure (for example a PNG encode of the canvas) that the writer runs after its pending frame, again latest wins.
- **Teardown.** `cleanup` (also run on drop) sets the stop flag, wakes the thread, drains a frame stashed just before stop, and joins for up to [`WRITER_JOIN_TIMEOUT`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_writer/constant.WRITER_JOIN_TIMEOUT.html) (1 s) before the sink is released.
- **Stats.** `stats()` returns writes, drops, skipped duplicates and the last write time in milliseconds.

Two sinks ship with the crate. `MmapSink` (Linux) maps `/dev/fbN` and, after the first full frame, copies and flushes only the span reported by [`changed_span`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_writer/fn.changed_span.html). [`VirtualSink`](https://docs.rs/ados-display-driver/latest/ados_display_driver/fb_writer/struct.VirtualSink.html) writes each frame to a file, which makes a render path runnable in CI or a VM with no framebuffer. Anything else implements `FrameSink`.

### [`oled`](https://docs.rs/ados-display-driver/latest/ados_display_driver/oled/index.html): SSD1306 / SH1106 over I2C

[`pack_page_buffer`](https://docs.rs/ados-display-driver/latest/ados_display_driver/oled/fn.pack_page_buffer.html) turns a one-byte-per-pixel canvas (non-zero is on) into the controllers' vertical-byte page layout: for each 8-pixel page and each column, bit `b` is the pixel at row `page * 8 + b`. [`init_commands`](https://docs.rs/ados-display-driver/latest/ados_display_driver/oled/fn.init_commands.html) builds the power-on sequence for a 64- or 32-row panel, and [`page_upload_plan`](https://docs.rs/ados-display-driver/latest/ados_display_driver/oled/fn.page_upload_plan.html) the per-page address commands and data. The two controllers differ only in the start column: SH1106 is a 132-column die windowed to 128, so it starts at column 2. Commands go out behind control byte `0x00` and data behind `0x40`.

The transport is the [`I2cBus`](https://docs.rs/ados-display-driver/latest/ados_display_driver/oled/trait.I2cBus.html) trait. On Linux, `I2cDev` opens `/dev/i2c-N`, binds the slave address with the `I2C_SLAVE` ioctl and writes with plain `write(2)`:

```rust,no_run
# #[cfg(target_os = "linux")]
# fn main() -> std::io::Result<()> {
use ados_display_driver::oled::{self, Controller, I2cDev, OledGeometry};

let geom = OledGeometry::W128_H64;
let mut dev = I2cDev::open(1, oled::DEFAULT_I2C_ADDR)?; // /dev/i2c-1, address 0x3C
oled::init(&mut dev, geom)?;

// A canvas with a horizontal line across row 10.
let mut canvas = vec![0u8; geom.width as usize * geom.height as usize];
canvas[10 * geom.width as usize..11 * geom.width as usize].fill(1);
let packed = oled::pack_page_buffer(&canvas, geom).expect("canvas matches the geometry");
oled::render_frame(&mut dev, &packed, geom, Controller::Ssd1306)?;
# Ok(())
# }
# #[cfg(not(target_os = "linux"))]
# fn main() {}
```

## Part of ADOS

Used in production by the [ADOS Drone Agent](https://github.com/altnautica/ADOSDroneAgent), the onboard software of the ADOS software-defined drone platform.

[All ADOS crates](https://github.com/altnautica/ADOSDroneAgent/tree/main/crates#published-crates) · [Docs](https://docs.altnautica.com) · [Discord](https://discord.gg/uxbvuD4d5q)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this crate by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
