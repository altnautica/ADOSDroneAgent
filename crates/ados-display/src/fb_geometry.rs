//! Framebuffer geometry + driver-name discovery from `/sys/class/graphics`.
//!
//! Ports `_read_fb_geometry`, `_read_fb_name`, and the SPI-LCD driver-name set
//! (`hardware_check.py::_SPI_LCD_DRIVER_NAMES`) plus the probe's candidate-match
//! rule from `renderers/framebuffer.py`. All of it is sysfs reads, so it is
//! tested against a temp `/sys/class/graphics` tree.

use std::path::{Path, PathBuf};

/// sysfs root for framebuffer devices.
pub const SYS_GRAPHICS_DIR: &str = "/sys/class/graphics";

/// Kernel driver names exported under `/sys/class/graphics/fbN/name` when a
/// supported SPI LCD is bound. The acceptance set used when display.conf
/// carries no explicit expected name, so the probe never binds the primary
/// HDMI/DRM framebuffer by accident. Exact 9-name set from
/// `hardware_check.py::_SPI_LCD_DRIVER_NAMES`.
pub const SPI_LCD_DRIVER_NAMES: [&str; 9] = [
    "fb_ili9486",
    "fb_ili9341",
    "fb_ili9340",
    "fb_st7789v",
    "fb_st7735r",
    "fb_hx8347d",
    "fb_hx8353d",
    "fb_pcd8544",
    "fb_ssd1351",
];

/// Whether `name` is one of the known SPI-LCD fbtft driver names (exact match).
pub fn is_spi_lcd_driver(name: &str) -> bool {
    SPI_LCD_DRIVER_NAMES.contains(&name)
}

/// Resolved framebuffer geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FbGeometry {
    pub xres: u32,
    pub yres: u32,
    pub bits_per_pixel: u32,
}

/// Read `(xres, yres, bits_per_pixel)` for a framebuffer under `sys_root`.
///
/// Prefers the modern `virtual_size` (`W,H` or `W H`) + `bits_per_pixel`
/// files; falls back to the legacy `var` blob (xres yres ... bpp at index 6).
/// `sys_root` is the `/sys/class/graphics` directory (a temp tree in tests);
/// `fb_name` is the `fbN` entry.
pub fn read_fb_geometry(sys_root: &Path, fb_name: &str) -> std::io::Result<FbGeometry> {
    let fb_dir = sys_root.join(fb_name);
    let vsize_path = fb_dir.join("virtual_size");
    let bpp_path = fb_dir.join("bits_per_pixel");
    if vsize_path.exists() && bpp_path.exists() {
        let vsize = std::fs::read_to_string(&vsize_path)?;
        let vsize = vsize.trim();
        let (w_str, h_str) = if let Some((w, h)) = vsize.split_once(',') {
            (w.trim().to_string(), h.trim().to_string())
        } else {
            let mut parts = vsize.split_whitespace();
            let w = parts.next();
            let h = parts.next();
            match (w, h) {
                (Some(w), Some(h)) => (w.to_string(), h.to_string()),
                _ => {
                    return Err(bad_format(&format!(
                        "unexpected virtual_size format: {vsize:?}"
                    )))
                }
            }
        };
        let bpp_text = std::fs::read_to_string(&bpp_path)?;
        let xres = parse_u32(&w_str)?;
        let yres = parse_u32(&h_str)?;
        let bits_per_pixel = parse_u32(bpp_text.trim())?;
        return Ok(FbGeometry {
            xres,
            yres,
            bits_per_pixel,
        });
    }
    // Legacy fallback: parse the show_var() blob (xres yres ... bpp@6).
    let var_text = std::fs::read_to_string(fb_dir.join("var"))?;
    let parts: Vec<&str> = var_text.split_whitespace().collect();
    if parts.len() < 7 {
        return Err(bad_format(&format!(
            "unexpected /sys/class/graphics/{fb_name}/var format"
        )));
    }
    Ok(FbGeometry {
        xres: parse_u32(parts[0])?,
        yres: parse_u32(parts[1])?,
        bits_per_pixel: parse_u32(parts[6])?,
    })
}

/// Read the driver-reported name from `<sys_root>/<fb_name>/name`. Returns an
/// empty string when the file is missing or unreadable.
pub fn read_fb_name(sys_root: &Path, fb_name: &str) -> String {
    std::fs::read_to_string(sys_root.join(fb_name).join("name"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Whether a candidate framebuffer's driver name is acceptable for the SPI-LCD
/// UI. Mirrors the probe rule: when an `expected` name is configured, accept
/// only a framebuffer whose driver name contains it; otherwise accept only a
/// known SPI-LCD fbtft driver so the primary HDMI/DRM fb is never grabbed.
pub fn driver_name_acceptable(driver_name: &str, expected: &str) -> bool {
    let expected = expected.trim();
    if !expected.is_empty() {
        !driver_name.is_empty() && driver_name.contains(expected)
    } else {
        is_spi_lcd_driver(driver_name)
    }
}

/// Whether the bit depth is one this renderer can pack (16/24/32).
pub fn bpp_supported(bpp: u32) -> bool {
    matches!(bpp, 16 | 24 | 32)
}

/// List the `fbN` entries under `sys_root`, sorted, for `N` all-digits.
pub fn list_framebuffers(sys_root: &Path) -> Vec<String> {
    let mut names: Vec<String> = match std::fs::read_dir(sys_root) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| is_fb_entry(n))
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Whether `name` is an `fbN` entry with an all-digit suffix.
fn is_fb_entry(name: &str) -> bool {
    name.strip_prefix("fb")
        .map(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
        .unwrap_or(false)
}

/// A framebuffer that matched the probe rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FbMatch {
    pub fb_name: String,
    pub dev_path: PathBuf,
    pub driver_name: String,
    pub geometry: FbGeometry,
}

/// Walk `sys_root` and return the first framebuffer whose driver name passes
/// [`driver_name_acceptable`] and whose bpp is supported. `expected` is the
/// configured `framebuffer_name_expected` (empty for the SPI-LCD-set fallback).
/// This is the pure core of the renderer probe (the dev-node open lives in the
/// daemon).
pub fn match_framebuffer(sys_root: &Path, expected: &str) -> Option<FbMatch> {
    for fb_name in list_framebuffers(sys_root) {
        let Ok(geometry) = read_fb_geometry(sys_root, &fb_name) else {
            continue;
        };
        let driver_name = read_fb_name(sys_root, &fb_name);
        if !driver_name_acceptable(&driver_name, expected) {
            continue;
        }
        if !bpp_supported(geometry.bits_per_pixel) {
            continue;
        }
        return Some(FbMatch {
            dev_path: PathBuf::from(format!("/dev/{fb_name}")),
            fb_name,
            driver_name,
            geometry,
        });
    }
    None
}

fn parse_u32(s: &str) -> std::io::Result<u32> {
    s.trim()
        .parse::<u32>()
        .map_err(|_| bad_format(&format!("not a u32: {s:?}")))
}

fn bad_format(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a temp `/sys/class/graphics` tree with one fb entry.
    fn make_fb(
        root: &Path,
        fb: &str,
        name: Option<&str>,
        virtual_size: Option<&str>,
        bpp: Option<&str>,
        var: Option<&str>,
    ) {
        let dir = root.join(fb);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(n) = name {
            std::fs::write(dir.join("name"), format!("{n}\n")).unwrap();
        }
        if let Some(vs) = virtual_size {
            std::fs::write(dir.join("virtual_size"), format!("{vs}\n")).unwrap();
        }
        if let Some(b) = bpp {
            std::fs::write(dir.join("bits_per_pixel"), format!("{b}\n")).unwrap();
        }
        if let Some(v) = var {
            std::fs::write(dir.join("var"), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn spi_lcd_driver_set_is_the_nine_names() {
        assert_eq!(SPI_LCD_DRIVER_NAMES.len(), 9);
        assert!(is_spi_lcd_driver("fb_ili9486"));
        assert!(is_spi_lcd_driver("fb_ssd1351"));
        assert!(!is_spi_lcd_driver("rockchip-drm"));
        assert!(!is_spi_lcd_driver("BCM2708 FB"));
    }

    #[test]
    fn geometry_from_virtual_size_comma() {
        let dir = tempfile::tempdir().unwrap();
        make_fb(
            dir.path(),
            "fb1",
            Some("fb_ili9486"),
            Some("480,320"),
            Some("16"),
            None,
        );
        let g = read_fb_geometry(dir.path(), "fb1").unwrap();
        assert_eq!(
            g,
            FbGeometry {
                xres: 480,
                yres: 320,
                bits_per_pixel: 16
            }
        );
    }

    #[test]
    fn geometry_from_virtual_size_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        make_fb(
            dir.path(),
            "fb0",
            Some("fb_st7789v"),
            Some("320 240"),
            Some("16"),
            None,
        );
        let g = read_fb_geometry(dir.path(), "fb0").unwrap();
        assert_eq!(
            g,
            FbGeometry {
                xres: 320,
                yres: 240,
                bits_per_pixel: 16
            }
        );
    }

    #[test]
    fn geometry_legacy_var_fallback() {
        let dir = tempfile::tempdir().unwrap();
        // var blob: xres yres a b c d bpp(@6) ...
        make_fb(
            dir.path(),
            "fb1",
            Some("fb_ili9486"),
            None,
            None,
            Some("480 320 0 0 0 0 24 0"),
        );
        let g = read_fb_geometry(dir.path(), "fb1").unwrap();
        assert_eq!(
            g,
            FbGeometry {
                xres: 480,
                yres: 320,
                bits_per_pixel: 24
            }
        );
    }

    #[test]
    fn geometry_var_too_short_errors() {
        let dir = tempfile::tempdir().unwrap();
        make_fb(dir.path(), "fb1", None, None, None, Some("480 320 0"));
        assert!(read_fb_geometry(dir.path(), "fb1").is_err());
    }

    #[test]
    fn driver_name_acceptable_rules() {
        // Expected configured: substring match required.
        assert!(driver_name_acceptable("fb_ili9486", "fb_ili9486"));
        assert!(driver_name_acceptable("fb_ili9486 rev2", "fb_ili9486"));
        assert!(!driver_name_acceptable("rockchip-drm", "fb_ili9486"));
        assert!(!driver_name_acceptable("", "fb_ili9486"));
        // No expected: only the known SPI-LCD set is accepted.
        assert!(driver_name_acceptable("fb_st7735r", ""));
        assert!(!driver_name_acceptable("BCM2708 FB", ""));
        assert!(!driver_name_acceptable("", ""));
    }

    #[test]
    fn bpp_support_gate() {
        assert!(bpp_supported(16));
        assert!(bpp_supported(24));
        assert!(bpp_supported(32));
        assert!(!bpp_supported(8));
        assert!(!bpp_supported(15));
    }

    #[test]
    fn list_framebuffers_sorted_digits_only() {
        let dir = tempfile::tempdir().unwrap();
        for fb in ["fb1", "fb0", "fbcon", "fb10"] {
            std::fs::create_dir_all(dir.path().join(fb)).unwrap();
        }
        // "fbcon" excluded (non-digit suffix); rest sorted lexically.
        assert_eq!(list_framebuffers(dir.path()), vec!["fb0", "fb1", "fb10"]);
    }

    #[test]
    fn match_prefers_expected_and_skips_primary_drm() {
        let dir = tempfile::tempdir().unwrap();
        // fb0 is the primary DRM surface; fb1 is the SPI LCD.
        make_fb(
            dir.path(),
            "fb0",
            Some("rockchip-drm"),
            Some("1920,1080"),
            Some("32"),
            None,
        );
        make_fb(
            dir.path(),
            "fb1",
            Some("fb_ili9486"),
            Some("480,320"),
            Some("16"),
            None,
        );
        let m = match_framebuffer(dir.path(), "fb_ili9486").unwrap();
        assert_eq!(m.fb_name, "fb1");
        assert_eq!(m.dev_path, PathBuf::from("/dev/fb1"));
        assert_eq!(m.driver_name, "fb_ili9486");
        assert_eq!(m.geometry.xres, 480);
    }

    #[test]
    fn match_fallback_to_spi_lcd_set_when_no_expected() {
        let dir = tempfile::tempdir().unwrap();
        make_fb(
            dir.path(),
            "fb0",
            Some("rockchip-drm"),
            Some("1920,1080"),
            Some("32"),
            None,
        );
        make_fb(
            dir.path(),
            "fb1",
            Some("fb_st7789v"),
            Some("320,240"),
            Some("16"),
            None,
        );
        let m = match_framebuffer(dir.path(), "").unwrap();
        assert_eq!(m.fb_name, "fb1");
        assert_eq!(m.driver_name, "fb_st7789v");
    }

    #[test]
    fn match_skips_unsupported_bpp() {
        let dir = tempfile::tempdir().unwrap();
        // A SPI LCD reporting an unsupported 8 bpp is rejected.
        make_fb(
            dir.path(),
            "fb0",
            Some("fb_pcd8544"),
            Some("84,48"),
            Some("8"),
            None,
        );
        assert!(match_framebuffer(dir.path(), "").is_none());
    }

    #[test]
    fn match_none_when_no_fb_present() {
        let dir = tempfile::tempdir().unwrap();
        assert!(match_framebuffer(dir.path(), "fb_ili9486").is_none());
    }
}
