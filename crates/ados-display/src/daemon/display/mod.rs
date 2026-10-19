//! `ados-display` daemon.
//!
//! Owns the LCD framebuffer write path. It probes the bound SPI-LCD
//! framebuffer, opens the mmap sink, and runs the off-thread writer.
//!
//! The full-resolution page UI in [`crate::pages`] renders in-process: a
//! [`StateSource`] reads the live agent state, the [`PageNavigator`] resolves
//! the active page, the page composer paints a 480x320 canvas, the frame is
//! packed and presented through the writer. It also writes a PNG of each
//! rendered frame to `lcd-snapshot.png` so the REST snapshot endpoint can serve
//! the live panel without re-reading the framebuffer. The writer's stats are
//! mirrored to `lcd-fb-stats.json` at 1 Hz, and a remote page switch arrives via
//! `lcd-page-request.json`.
//!
//! On a board with no bound SPI-LCD framebuffer the daemon logs and exits 0 — a
//! ground station with no LCD is a supported configuration, not a failure.
//! Modelled on the supervisor main loop.

use std::path::Path;

use anyhow::Result;

use crate::conf;

fn init_logging() {
    use ados_protocol::logd::layer::LogdLayer;
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());

    // The logd layer ships records to the logging daemon's ingest socket
    // alongside the primary sink; it is best-effort and never blocks the service.
    #[cfg(target_os = "linux")]
    {
        if let Ok(journald) = tracing_journald::layer() {
            let _ = tracing_subscriber::registry()
                .with(EnvFilter::new(&filter))
                .with(journald)
                .with(LogdLayer::new("ados-display"))
                .try_init();
            return;
        }
    }

    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(&filter))
        .with(tracing_subscriber::fmt::layer())
        .with(LogdLayer::new("ados-display"))
        .try_init();
}

#[cfg(target_os = "linux")]
fn sd_ready() {
    if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
        tracing::debug!(error = %e, "sd_notify READY failed");
    }
}

#[cfg(target_os = "linux")]
fn sd_watchdog() {
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
}

/// The `ados-display` binary's entry point.
pub fn run() -> std::process::ExitCode {
    super::exit_code(serve())
}

#[tokio::main]
async fn serve() -> Result<()> {
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
        run_linux(expected, rotation, &conf_blob).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (expected, rotation, &conf_blob);
        tracing::info!("ados-display: framebuffer write path is Linux-only; idle on this host");
        Ok(())
    }
}

/// The Linux service body: probe, open the sink, run the writer, then drive the
/// native page UI until a stop signal.
#[cfg(target_os = "linux")]
async fn run_linux(
    expected: &str,
    rotation: i32,
    conf_blob: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    use crate::fb_geometry::{self, FbMatch};
    use crate::fb_writer::{FbWriter, MmapSink};

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

    // The virtual sink (a file, for a VM/CI/dev box with no framebuffer) takes
    // precedence when ADOS_DISPLAY_VIRTUAL_FB is set; otherwise the real
    // /dev/fbN mmap. FbWriter::spawn is generic over the sink, so both yield the
    // same writer.
    let writer = match crate::fb_writer::VirtualSink::from_env() {
        Some(vs) => {
            tracing::info!("ados-display: virtual framebuffer sink (ADOS_DISPLAY_VIRTUAL_FB)");
            FbWriter::spawn(vs)
        }
        None => FbWriter::spawn(MmapSink::open(&dev_path.to_string_lossy(), frame_bytes)?),
    };

    sd_ready();

    // The native in-process page UI drives ANY panel: the 480x320 page canvas is
    // scaled-to-fit (letterboxed) onto the probed geometry, so a smaller or
    // larger panel renders the full UI rather than nothing.
    tracing::info!(
        width = geometry.xres,
        height = geometry.yres,
        bpp = geometry.bits_per_pixel,
        "native page UI mode engaged (scale-to-fit)"
    );
    run_page_ui(
        writer,
        geometry.bits_per_pixel,
        rotation,
        conf_blob,
        geometry.xres,
        geometry.yres,
    )
    .await
}

#[cfg(target_os = "linux")]
mod page_ui;
#[cfg(target_os = "linux")]
use page_ui::run_page_ui;

/// The full set of pages the native page UI registers: the ground-station tab
/// roots, the visor tab pages, the detail pages the dashboard / more overflow
/// drill into, and the power-off prompt. Every profile registers every page;
/// the panel config only decides which are tabs, and a remote page request may
/// still reach any of them, so the persisted `route_ids` stay truthful.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn all_pages() -> Vec<Box<dyn crate::pages::Page>> {
    use crate::pages::{
        about::AboutDetailPage, access_point::AccessPointDetailPage, cameras::CamerasPage,
        channel_hops::ChannelHopsPage, dashboard::DashboardPage,
        diagnostics::DiagnosticsDetailPage, drone::DroneDetailPage, gnss::GnssPage,
        link_stats::LinkStatsPage, lora::LoraPage, mesh::MeshDetailPage, more::MorePage,
        pair_drone::PairDroneDetailPage, plugin::PluginPage, power::PowerPage,
        radio_link::RadioLinkDetailPage, radios::RadiosPage, sensors::SensorsPage,
        settings::SettingsPage, storage::StoragePage, uplink::UplinkDetailPage, video::VideoPage,
        Page,
    };
    let pages: Vec<Box<dyn Page>> = vec![
        Box::new(DashboardPage),
        Box::new(VideoPage),
        Box::new(SettingsPage),
        Box::new(LinkStatsPage),
        Box::new(ChannelHopsPage),
        Box::new(MorePage::default()),
        Box::new(RadioLinkDetailPage::new()),
        Box::new(UplinkDetailPage),
        Box::new(DroneDetailPage),
        Box::new(MeshDetailPage::new()),
        Box::new(AboutDetailPage),
        // How to join this ground station's own WiFi. A panel is the only
        // surface that helps when the box is the thing you cannot reach.
        Box::new(AccessPointDetailPage),
        Box::new(PairDroneDetailPage::default()),
        Box::new(DiagnosticsDetailPage),
        // Ground-station detail pages for the GNSS base, the receive radios
        // and the LoRa link.
        Box::new(GnssPage),
        Box::new(RadiosPage),
        Box::new(LoraPage),
        // The reserved data-driven page a plugin contributes content to. A
        // visor tab; elsewhere reachable via a page-switch request for the
        // `plugin` id.
        Box::new(PluginPage::new()),
        // The visor status tabs.
        Box::new(CamerasPage),
        Box::new(SensorsPage),
        Box::new(StoragePage),
        // The power-off prompt a `shutdown_prompt` press opens.
        Box::new(PowerPage),
    ];
    pages
}
