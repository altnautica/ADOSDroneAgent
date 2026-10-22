//! Render the I2C status OLED's 128x64 frame to a PNG for documentation and
//! visual review.
//!
//! Paints a synthetic ground station (hostname, LAN IP, link line, role line)
//! through the same [`ados_display::oled_frame`] builder the `ados-oled-i2c`
//! daemon uses, then scales each panel pixel to a `SCALE`x`SCALE` block
//! (nearest neighbour, so the frame content stays exactly 128x64) and writes
//! `target/ui-parity/oled_status.png`.
//!
//! ```text
//! cargo run -p ados-display --example render_oled
//! ```

use std::path::PathBuf;

use ados_display::oled_frame::{render_canvas, status_fonts};
use ados_display::pages::PageContext;
use ados_display_driver::oled::OledGeometry;

/// Integer upscale per panel pixel.
const SCALE: u32 = 4;
/// Lit pixel colour: the white of a common monochrome SSD1306 module.
const ON: [u8; 3] = [0xF2, 0xF6, 0xFF];
/// Unlit pixel colour.
const OFF: [u8; 3] = [0x00, 0x00, 0x00];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate dir has a parent (the crates workspace dir)")
        .join("target")
        .join("ui-parity");
    std::fs::create_dir_all(&out_dir)?;

    let mut ctx = PageContext {
        hostname: "ados-9f2c1a".to_string(),
        ..PageContext::default()
    };
    ctx.link.state = Some("connected".to_string());
    ctx.link.rssi_dbm = Some(-67.0);
    ctx.role.current = Some("direct".to_string());

    let geom = OledGeometry::W128_H64;
    let (title_font, body_font) = status_fonts();
    let canvas = render_canvas(&ctx, "192.168.7.1", geom, &title_font, &body_font);

    let (w, h) = (geom.width as u32, geom.height as u32);
    let image = image::RgbImage::from_fn(w * SCALE, h * SCALE, |x, y| {
        let lit = canvas[((y / SCALE) * w + x / SCALE) as usize] != 0;
        image::Rgb(if lit { ON } else { OFF })
    });
    let path = out_dir.join("oled_status.png");
    image.save(&path)?;
    println!(
        "rendered the {w}x{h} OLED frame at x{SCALE} to {}",
        path.display()
    );
    Ok(())
}
