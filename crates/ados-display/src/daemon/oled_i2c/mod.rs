//! ados-oled-i2c: drive a small I2C OLED (SSD1306 / SH1106, 128x64 or 128x32)
//! with a compact ground-station status page.
//!
//! This is the wired consumer of the crate's `oled` driver. It is INDEPENDENT
//! of the framebuffer / SPI-LCD display arbitration (the legacy `ados-oled`
//! unit, which despite its name runs the framebuffer renderer): a tiny I2C
//! status OLED coexists with the HDMI cockpit, so it runs whenever an OLED
//! answers on the I2C bus, regardless of what the main display surface is.
//!
//! Auto-skip: if I2C is not enabled (`/dev/i2c-<bus>` absent) or no
//! device acknowledges at the configured address, the service logs the reason
//! and exits cleanly (rc 0) — a board with no OLED never churns.
//!
//! Reuse: status comes from [`crate::state_source::StateSource::status_only`]
//! (one GET of the ground-station status every 2 s); the frame is painted by
//! [`crate::oled_frame`] (grayscale glyph coverage thresholded to 1bpp) and
//! packed + uploaded by [`crate::oled`], which re-initialises the panel after
//! repeated upload failures (a brownout). The configured
//! `ground_station.ui.oled.brightness` is applied as the controller contrast
//! whenever it changes. The frame builder is pure and unit-tested; only the
//! I2C open/write is Linux-gated.

use crate::oled::{self, Controller, OledGeometry};
use crate::oled_frame::{build_frame, status_fonts};
#[cfg(not(target_os = "linux"))]
use crate::pages::PageContext;

/// Read a `u8` env var with a fallback (for ops overrides; the defaults match a
/// standard Raspberry Pi header OLED).
fn env_u8(key: &str, default: u8) -> u8 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Parse a `u16` that may be hex (`0x3c`) or decimal (`60`). Pure.
fn parse_u16(s: &str) -> Option<u16> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()
    } else {
        t.parse().ok()
    }
}

/// Resolve the panel geometry from `ADOS_OLED_HEIGHT` (32 -> 128x32, else
/// 128x64, the common part).
fn geometry_from_env() -> OledGeometry {
    if env_u8("ADOS_OLED_HEIGHT", 64) == 32 {
        OledGeometry::W128_H32
    } else {
        OledGeometry::W128_H64
    }
}

/// Resolve the controller from `ADOS_OLED_CONTROLLER` (`sh1106` -> SH1106, else
/// SSD1306).
fn controller_from_env() -> Controller {
    match std::env::var("ADOS_OLED_CONTROLLER").as_deref() {
        Ok("sh1106") | Ok("SH1106") => Controller::Sh1106,
        _ => Controller::Ssd1306,
    }
}

/// The I2C addresses to probe, in order. When the operator pins `ADOS_OLED_ADDR`
/// we honour exactly that; otherwise we auto-probe the two addresses these OLED
/// modules ship at — 0x3C (default) then 0x3D (the jumpered variant) — so the
/// panel is found with zero manual config regardless of its address strap
/// (plug-and-play).
fn addresses_to_probe() -> Vec<u16> {
    resolve_probe_addresses(std::env::var("ADOS_OLED_ADDR").ok().as_deref())
}

/// Pure resolution of the probe address list from an optional pinned value.
fn resolve_probe_addresses(pinned: Option<&str>) -> Vec<u16> {
    match pinned {
        Some(s) => vec![parse_u16(s).unwrap_or(oled::DEFAULT_I2C_ADDR)],
        None => vec![0x3C, 0x3D],
    }
}

/// The primary LAN IPv4 the box is reachable at, or "no network". Uses the
/// connect-a-UDP-socket trick (no packet is sent) to read the source address the
/// kernel would use for an off-box route — i.e. the interface with the default
/// route.
fn primary_ip() -> String {
    use std::net::UdpSocket;
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "no network".to_string())
}

/// How long a resolved LAN IP is reused before the UDP-socket lookup reruns.
const IP_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// [`primary_ip`] cached for [`IP_CACHE_TTL`].
struct CachedIp {
    value: String,
    at: Option<std::time::Instant>,
}

impl CachedIp {
    fn new() -> Self {
        Self {
            value: String::new(),
            at: None,
        }
    }

    fn get(&mut self) -> &str {
        if self.at.is_none_or(|t| t.elapsed() >= IP_CACHE_TTL) {
            self.value = primary_ip();
            self.at = Some(std::time::Instant::now());
        }
        &self.value
    }
}

/// The persisted OLED contrast (`ground_station.ui.oled.brightness`, 0-255) from
/// the agent config text, or the API default (204) when unset or unparseable.
fn configured_brightness(config_yaml: Option<&str>) -> i64 {
    const DEFAULT_BRIGHTNESS: i64 = 204;
    config_yaml
        .and_then(|text| serde_norway::from_str::<serde_norway::Value>(text).ok())
        .and_then(|root| {
            root.get("ground_station")?
                .get("ui")?
                .get("oled")?
                .get("brightness")?
                .as_i64()
        })
        .unwrap_or(DEFAULT_BRIGHTNESS)
}

/// Re-reads the agent config only when its modification time changes.
struct BrightnessWatch {
    path: std::path::PathBuf,
    mtime: Option<std::time::SystemTime>,
    value: i64,
    loaded: bool,
}

impl BrightnessWatch {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            mtime: None,
            value: configured_brightness(None),
            loaded: false,
        }
    }

    fn current(&mut self) -> i64 {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if !self.loaded || mtime != self.mtime {
            let text = std::fs::read_to_string(&self.path).ok();
            self.value = configured_brightness(text.as_deref());
            self.mtime = mtime;
            self.loaded = true;
        }
        self.value
    }
}

#[cfg(target_os = "linux")]
fn drive() {
    use crate::state_source::StateSource;
    use std::time::Duration;

    let bus = env_u8("ADOS_OLED_BUS", 1);
    let geom = geometry_from_env();
    let controller = controller_from_env();

    // Open the bus once and probe each candidate address. A missing
    // /dev/i2c-<bus> (I2C not enabled) OR no device acknowledging at any
    // candidate address is a clean skip (rc 0) — a board with no OLED never
    // churns. The init write is the presence check: an absent device NAKs
    // (ENXIO), so a failing init means no OLED at that address.
    let mut found: Option<(oled::I2cDev, u16)> = None;
    for addr in addresses_to_probe() {
        let mut dev = match oled::I2cDev::open(bus, addr) {
            Ok(d) => d,
            Err(e) => {
                tracing::info!(bus, addr, error = %e, "no I2C OLED bus; service exits cleanly");
                return;
            }
        };
        match oled::init(&mut dev, geom) {
            Ok(()) => {
                found = Some((dev, addr));
                break;
            }
            Err(e) => {
                tracing::debug!(bus, addr, error = %e, "no OLED acknowledged at address");
            }
        }
    }
    let (mut dev, addr) = match found {
        Some(f) => f,
        None => {
            tracing::info!(
                bus,
                "no OLED acknowledged on the I2C bus; service exits cleanly"
            );
            return;
        }
    };
    tracing::info!(
        bus,
        addr,
        height = geom.height,
        "OLED status service started"
    );

    let (title_font, body_font) = status_fonts();
    let mut src = StateSource::new();
    let mut ip = CachedIp::new();
    let mut link = crate::oled_link::OledLink::new();
    let mut brightness = BrightnessWatch::new(std::path::PathBuf::from(
        std::env::var("ADOS_CONFIG").unwrap_or_else(|_| "/etc/ados/config.yaml".to_string()),
    ));

    loop {
        let contrast = crate::oled_link::brightness_to_contrast(brightness.current());
        if let Err(e) = link.apply_contrast(&mut dev, contrast) {
            tracing::debug!(error = %e, "OLED contrast write failed; retrying next tick");
        }
        let ctx = src.status_only();
        let packed = build_frame(&ctx, ip.get(), geom, &title_font, &body_font);
        if let Err(e) = link.upload(&mut dev, &packed, geom, controller) {
            // A transient bus error should not kill the service; the link
            // re-initialises the panel after repeated failures.
            tracing::warn!(error = %e, "OLED frame upload failed; retrying");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(not(target_os = "linux"))]
fn drive() {
    // This service drives a Linux I2C device. On other hosts (dev machines) do
    // a single dry render so the pure frame builder + env helpers stay compiled
    // and exercised (no dead-code warnings off-target), then exit.
    let geom = geometry_from_env();
    let _ = controller_from_env();
    let _ = addresses_to_probe();
    let mut brightness = BrightnessWatch::new(std::path::PathBuf::from("/etc/ados/config.yaml"));
    let _ = crate::oled_link::brightness_to_contrast(brightness.current());
    let mut ip = CachedIp::new();
    let (title_font, body_font) = status_fonts();
    let frame = build_frame(
        &PageContext::default(),
        ip.get(),
        geom,
        &title_font,
        &body_font,
    );
    eprintln!(
        "ados-oled-i2c drives a Linux I2C device; not supported on this host ({} bytes/frame)",
        frame.len()
    );
}

/// Best-effort tracing init: journald on Linux (the unit journals to it),
/// stderr fmt otherwise, both fed to the logging daemon. Mirrors ados-display.
fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-oled-i2c"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-oled-i2c"))
        .try_init();
}

/// The `ados-oled-i2c` binary's entry point.
pub fn run() -> std::process::ExitCode {
    init_logging();
    drive();
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests;
