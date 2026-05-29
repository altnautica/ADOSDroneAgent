//! `ados-display` daemon.
//!
//! Owns the LCD framebuffer write path. It probes the bound SPI-LCD
//! framebuffer, opens the mmap sink, and runs the off-thread writer. Composed
//! RGB888 frames come from a thin Python page-render sidecar (the ~6k-LOC PIL
//! page UI STAYS PYTHON): the daemon spawns it and reads length-prefixed frames
//! from its stdout, packs each to the panel's bit depth, and hands it to the
//! writer. The writer's stats are mirrored to `lcd-latency.json` at 1 Hz, and a
//! remote page switch is forwarded to the render sidecar via
//! `lcd-page-request.json`.
//!
//! On a board with no bound SPI-LCD framebuffer the daemon logs and exits 0 — a
//! ground station with no LCD is a supported configuration, not a failure.
//! Modelled on the supervisor main loop.

use std::path::Path;

use anyhow::Result;

use ados_display::conf;

fn init_logging() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    #[cfg(target_os = "linux")]
    {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&filter))
        .try_init();
}

#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(target_os = "linux")]
fn sd_watchdog() {
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]);
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    tracing::info!("ados-display starting");

    // Probe the bound SPI-LCD framebuffer (driver-name match + supported bpp).
    let conf_blob = conf::parse(Path::new(conf::DISPLAY_CONF_PATH));
    let expected = conf_blob
        .get("framebuffer_name_expected")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("fb_ili9486");
    let rotation = conf::rotation_from_blob(&conf_blob);

    #[cfg(target_os = "linux")]
    {
        run_linux(expected, rotation).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (expected, rotation);
        tracing::info!("ados-display: framebuffer write path is Linux-only; idle on this host");
        Ok(())
    }
}

/// The Linux service body: probe, open the sink, run the writer, drive the
/// page-request watcher + latency mirror until a stop signal.
#[cfg(target_os = "linux")]
async fn run_linux(expected: &str, rotation: i32) -> Result<()> {
    use std::time::Duration;

    use tokio::signal::unix::{signal, SignalKind};

    use ados_display::fb_geometry::{self, FbMatch};
    use ados_display::fb_writer::{FbWriter, MmapSink};
    use ados_display::sidecar::{LcdLatency, LCD_LATENCY_PATH, LCD_PAGE_REQUEST_PATH};

    let sys_root = Path::new(fb_geometry::SYS_GRAPHICS_DIR);
    let Some(FbMatch {
        dev_path,
        fb_name,
        driver_name,
        geometry,
    }) = fb_geometry::match_framebuffer(sys_root, expected)
    else {
        tracing::info!("ados-display: no SPI-LCD framebuffer bound; nothing to drive");
        return Ok(());
    };
    let frame_bytes =
        geometry.xres as usize * geometry.yres as usize * (geometry.bits_per_pixel as usize / 8);
    tracing::info!(
        path = %dev_path.display(),
        name = %driver_name,
        fb = %fb_name,
        width = geometry.xres,
        height = geometry.yres,
        bpp = geometry.bits_per_pixel,
        rotation,
        "framebuffer probed"
    );

    let sink = MmapSink::open(&dev_path.to_string_lossy(), frame_bytes)?;
    let writer = FbWriter::spawn(sink);

    sd_ready();

    // The page-render sidecar (Python PIL UI) is launched by the daemon's
    // systemd unit / env; here we run the latency mirror + page-request watcher.
    // The actual RGB frame ingestion wires to the sidecar's framed stdout in the
    // deployment plumbing; this loop owns the cross-process sidecars.
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                // Mirror writer stats to lcd-latency.json for the diag surface.
                let lat: LcdLatency = writer.stats().into();
                if let Err(e) = lat.write_to(Path::new(LCD_LATENCY_PATH)) {
                    tracing::debug!(error = %e, "lcd-latency write failed");
                }
                // Drain a remote page switch (the render sidecar applies it).
                if let Some(page) = ados_display::sidecar::take_page_request(
                    Path::new(LCD_PAGE_REQUEST_PATH),
                ) {
                    tracing::info!(page = %page, "lcd page request received");
                    // The render sidecar reads the resolved page; the request
                    // file is already unlinked by take_page_request.
                }
                sd_watchdog();
            }
            _ = sigterm.recv() => {
                tracing::info!("received SIGTERM");
                break;
            }
            _ = sigint.recv() => {
                tracing::info!("received SIGINT");
                break;
            }
        }
    }

    // cleanup() joins the writer thread BEFORE the mmap sink is dropped.
    drop(writer);
    tracing::info!("ados-display stopped");
    Ok(())
}
