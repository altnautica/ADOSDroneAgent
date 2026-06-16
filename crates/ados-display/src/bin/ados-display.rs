//! `ados-display` daemon.
//!
//! Owns the LCD framebuffer write path. It probes the bound SPI-LCD
//! framebuffer, opens the mmap sink, and runs the off-thread writer.
//!
//! The full-resolution page UI in [`ados_display::pages`] renders in-process: a
//! [`StateSource`] reads the live agent state, the [`PageNavigator`] resolves
//! the active page, the page composer paints a 480x320 canvas, the frame is
//! packed and presented through the writer. It also writes a PNG of each
//! rendered frame to `lcd-snapshot.png` so the REST snapshot endpoint can serve
//! the live panel without re-reading the framebuffer. The writer's stats are
//! mirrored to `lcd-latency.json` at 1 Hz, and a remote page switch arrives via
//! `lcd-page-request.json`.
//!
//! On a board with no bound SPI-LCD framebuffer the daemon logs and exits 0 — a
//! ground station with no LCD is a supported configuration, not a failure.
//! Modelled on the supervisor main loop.

use std::path::Path;

use anyhow::Result;

use ados_display::conf;

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

    // The native in-process page UI is the only render mode. When the panel can
    // host the 480x320 page system, drive the page render loop.
    let page_geom_ok = geometry.xres >= ados_display::pages::PANEL_W
        && geometry.yres >= ados_display::pages::PANEL_H;
    if page_geom_ok {
        tracing::info!(
            width = geometry.xres,
            height = geometry.yres,
            bpp = geometry.bits_per_pixel,
            "native page UI mode engaged"
        );
        return run_page_ui(writer, geometry.bits_per_pixel, rotation, conf_blob).await;
    }

    // A bound SPI-LCD too small for the 480x320 page system is an unsupported
    // panel: there is nothing to render, but the framebuffer is still owned, so
    // keep the writer alive and run the latency mirror + page-request drain so
    // the diagnostics surface stays honest rather than reporting a dead service.
    tracing::warn!(
        width = geometry.xres,
        height = geometry.yres,
        "bound panel cannot host the 480x320 page system; idling the write path"
    );
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    // Absorb SIGHUP so a UI-config reload signal does not terminate the idle
    // write path (there is no page system to reload, but the unit must survive).
    let mut sighup = signal(SignalKind::hangup())?;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                // Mirror writer stats to lcd-latency.json for the diag surface.
                let lat: LcdLatency = writer.stats().into();
                if let Err(e) = lat.write_to(Path::new(LCD_LATENCY_PATH)) {
                    tracing::debug!(error = %e, "lcd-latency write failed");
                }
                // Drain (and discard) a remote page switch so a stale request
                // can never accumulate while no page system is running.
                if let Some(page) = ados_display::sidecar::take_page_request(
                    Path::new(LCD_PAGE_REQUEST_PATH),
                ) {
                    tracing::info!(page = %page, "lcd page request dropped (no page system)");
                }
                sd_watchdog();
            }
            _ = sighup.recv() => {
                tracing::debug!("received SIGHUP (idle write path; nothing to reload)");
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

/// Drive the native in-process page UI: build the navigator + state source,
/// then on each tick rebuild the page context, render the active page, pack the
/// frame, and present it through the already-running off-thread writer.
///
/// Two cadences run on one timer. The agent state is re-polled every 5 s (the
/// status pages tolerate a few seconds of staleness and a faster poll burns a
/// core on an SBC that is also serving the video chain); the panel re-renders
/// at the active page's
/// `refresh_hz`, floored to 0.5 Hz when idle so the dashboard's clock-second
/// paint does not waste a core. `lcd-latency.json` is mirrored at 1 Hz, and a
/// remote `POST /api/v1/display/page` request is drained each render tick.
#[cfg(target_os = "linux")]
async fn run_page_ui(
    mut writer: ados_display::fb_writer::FbWriter,
    bpp: u32,
    rotation: i32,
    conf_blob: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    use std::time::{Duration, Instant};

    use tokio::signal::unix::{signal, SignalKind};

    use ados_display::calibration::{
        take_recalibrate_flag, CalibrationController, CalibrationOutcome, RECALIBRATE_FLAG_PATH,
    };
    use ados_display::fb_writer::{FbWriter, Frame};
    use ados_display::graphics::palette::{self, Palette};
    use ados_display::graphics::primitives::Canvas;
    use ados_display::navigator::{Dispatch, PageNavigator};
    use ados_display::pages::calibration::render_calibration;
    use ados_display::pages::PageContext;
    use ados_display::render_loop::pack_frame;
    use ados_display::sidecar::{
        write_snapshot_png, LcdLatency, LCD_LATENCY_PATH, LCD_SNAPSHOT_PATH,
    };
    use ados_display::state_source::StateSource;
    use ados_display::touch_input::{TouchTransformHandle, TOUCH_CALIB_PATH};
    use ados_hid::touch::GestureKind;

    // State poll cadence — matches the Python service's POLL_PERIOD_SECONDS.
    const STATE_POLL_PERIOD: Duration = Duration::from_secs(5);
    // Idle render floor — matches IDLE_LCD_FLOOR_HZ (0.5 Hz → 2 s).
    const IDLE_RENDER_PERIOD: Duration = Duration::from_millis(2000);
    // Render-loop tick granularity. The render period derives from the active
    // page's refresh_hz; this is the polling resolution the loop wakes at to
    // re-evaluate whether a render or a state poll is due.
    const TICK_GRANULARITY: Duration = Duration::from_millis(100);
    // After any touch the panel repaints at this rate for BOOST_WINDOW so a drag
    // or tab switch tracks the finger, then settles back to the page cadence so
    // a quiet panel does not pin a core.
    const BOOST_HZ: f32 = 20.0;
    const BOOST_WINDOW: Duration = Duration::from_millis(500);
    // The calibration screen is static between taps (the tap path repaints it
    // immediately), so a modest base cadence keeps it lively without spinning.
    const CALIB_REFRESH_HZ: f32 = 5.0;

    // Build the frame for the active surface: the calibration wizard when one is
    // running (it owns the whole panel, no navigator chrome), else the
    // navigator's current page.
    fn build_canvas(
        calibration: &Option<CalibrationController>,
        navigator: &PageNavigator,
        ctx: &PageContext,
        palette: &Palette,
    ) -> Canvas {
        match calibration {
            Some(ctrl) => render_calibration(ctrl, palette),
            None => navigator.current_page().render(ctx, palette),
        }
    }

    // Pack + present a finished canvas through the off-thread writer. The blit
    // is off-thread, so this returns immediately.
    fn present_frame(writer: &FbWriter, bpp: u32, canvas: &Canvas) {
        let raw = canvas.as_rgb888();
        if let Some(packed) = pack_frame(canvas, bpp) {
            writer.present(Frame::new(packed, raw));
        } else {
            tracing::warn!(bpp, "unsupported panel bit depth; frame dropped");
        }
    }

    // The theme drives the palette. It is re-read on SIGHUP so a GCS or captive
    // portal config edit (`PUT /ui/oled`) takes effect without a unit restart.
    fn palette_from_conf(conf: &std::collections::BTreeMap<String, String>) -> Palette {
        palette::get_palette(
            conf.get("theme")
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("dark"),
        )
    }
    let mut palette: Palette = palette_from_conf(conf_blob);

    let mut navigator = PageNavigator::new(all_pages());
    let mut source = StateSource::new();
    let mut ctx = source.build_context();

    // The touch reader runs as its own task and posts each classified gesture
    // here; the select! loop turns it into a navigator transition. A small
    // bounded buffer is plenty — strokes arrive at human speed and each is
    // applied within one tick. The reader maps raw ADC samples to LCD pixels
    // for the configured rotation, so the navigator receives the same
    // coordinate frame the pages lay out in.
    // The touch transform is shared with the reader so a fresh calibration
    // reloads in place; the render loop also reads it to gate the auto-prompt.
    let touch_transform = TouchTransformHandle::new(rotation);
    let (touch_tx, mut touch_rx) =
        tokio::sync::mpsc::channel::<ados_display::touch_input::TouchEvent>(16);
    {
        let handle = touch_transform.clone();
        tokio::spawn(async move {
            if let Err(e) = ados_display::touch_input::run_touch_reader(handle, touch_tx).await {
                tracing::warn!(error = %e, "touch reader exited");
            }
        });
    }

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    // SIGHUP is the in-place config-reload signal. The agent's REST handlers
    // SIGHUP this unit after persisting a UI config change; without a handler
    // the default disposition would terminate the daemon, so the handler must
    // exist even though the only live-reloadable surface today is the theme.
    let mut sighup = signal(SignalKind::hangup())?;

    let mut tick = tokio::time::interval(TICK_GRANULARITY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // The snapshot PNG is encoded at most this often. The GCS preview polls at
    // 1 Hz, so a faster snapshot cadence would burn a core re-encoding frames no
    // remote ever fetches; the panel itself still re-renders at the page cadence.
    const SNAPSHOT_PERIOD: Duration = Duration::from_secs(1);

    let now = Instant::now();
    let mut last_state_poll = now;
    let mut last_render: Option<Instant> = None;
    let mut last_latency_write = now;
    let mut last_snapshot: Option<Instant> = None;
    // The active calibration wizard, if any. While `Some`, the loop paints the
    // calibration screen and routes every tap to it instead of the navigator.
    let mut calibration: Option<CalibrationController> = None;
    // When the operator last touched the panel, for the post-interaction render
    // boost.
    let mut last_interaction: Option<Instant> = None;
    // Monotonic millisecond clock for touch dispatch. The navigator stamps
    // tap-feedback against this same base the chrome compares its linger window
    // to, so the tapped tab flashes when a touch routes to it.
    let touch_clock = now;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                let now = Instant::now();

                // Engage the calibration wizard when a touch panel is present
                // but has no saved calibration: the rotation-identity fallback
                // is visibly off, so force a fit before the UI is usable. Stays
                // engaged until a fit is saved (no skip).
                if calibration.is_none()
                    && touch_transform.touch_present()
                    && !touch_transform.is_calibrated()
                {
                    tracing::info!("uncalibrated touch panel; launching calibration wizard");
                    calibration = Some(CalibrationController::new(rotation));
                    last_render = None;
                }

                // Re-poll the agent state on the slow cadence (the wizard reads
                // no agent state, so skip it while calibrating).
                if calibration.is_none()
                    && now.duration_since(last_state_poll) >= STATE_POLL_PERIOD
                {
                    ctx = source.build_context();
                    last_state_poll = now;
                }

                // Apply a remote page switch (mirrors the sidecar watcher), but
                // never let one interrupt an in-progress calibration.
                let route_changed =
                    calibration.is_none() && navigator.drain_page_request().is_some();

                // The render period follows the active surface's cadence, lifted
                // to the boost rate for a short window after any touch so drags
                // and tab switches track the finger. A page that declares no
                // cadence (hz <= 0) falls to the idle floor.
                let base_hz = if calibration.is_some() {
                    CALIB_REFRESH_HZ
                } else {
                    navigator.active_refresh_hz()
                };
                let boosting = last_interaction
                    .map(|t| now.duration_since(t) < BOOST_WINDOW)
                    .unwrap_or(false);
                let effective_hz = if boosting { base_hz.max(BOOST_HZ) } else { base_hz };
                let render_period = if effective_hz > 0.0 {
                    Duration::from_secs_f32(1.0 / effective_hz)
                } else {
                    IDLE_RENDER_PERIOD
                };
                let render_due = route_changed
                    || last_render
                        .map(|t| now.duration_since(t) >= render_period)
                        .unwrap_or(true);

                if render_due {
                    let canvas = build_canvas(&calibration, &navigator, &ctx, &palette);

                    // Mirror the freshly rendered frame to the snapshot PNG so the
                    // REST snapshot endpoint serves the live panel without PIL.
                    // Throttled to ~1 Hz independent of the render cadence.
                    let snapshot_due = last_snapshot
                        .map(|t| now.duration_since(t) >= SNAPSHOT_PERIOD)
                        .unwrap_or(true);
                    if snapshot_due {
                        if let Err(e) = write_snapshot_png(
                            Path::new(LCD_SNAPSHOT_PATH),
                            canvas.as_rgb888(),
                            canvas.width(),
                            canvas.height(),
                        ) {
                            tracing::debug!(error = %e, "lcd-snapshot write failed");
                        }
                        last_snapshot = Some(now);
                    }

                    present_frame(&writer, bpp, &canvas);
                    last_render = Some(now);
                }

                // Mirror writer stats + consume a recalibration request at ~1 Hz.
                if now.duration_since(last_latency_write) >= Duration::from_secs(1) {
                    let lat: LcdLatency = writer.stats().into();
                    if let Err(e) = lat.write_to(Path::new(LCD_LATENCY_PATH)) {
                        tracing::debug!(error = %e, "lcd-latency write failed");
                    }
                    // A GCS / bench "Recalibrate" drops the flag; relaunch the
                    // wizard in place (no reboot) and consume the request.
                    if calibration.is_none()
                        && take_recalibrate_flag(Path::new(RECALIBRATE_FLAG_PATH))
                    {
                        tracing::info!("recalibration requested; launching calibration wizard");
                        calibration = Some(CalibrationController::new(rotation));
                        last_render = None;
                    }
                    last_latency_write = now;
                    sd_watchdog();
                }
            }
            maybe_event = touch_rx.recv() => {
                // A completed stroke arrived from the reader task. A `None`
                // closes the channel only when the reader is gone; keep looping
                // so the rest of the UI (signals, render, latency) survives.
                if let Some(event) = maybe_event {
                    let now = Instant::now();
                    last_interaction = Some(now);

                    if calibration.is_some() {
                        // Calibration mode: a tap places a sample on the current
                        // target. On the final accepted tap the fit is saved,
                        // the live transform reloads, and the normal UI resumes.
                        if event.gesture.kind == GestureKind::Tap {
                            let outcome = calibration
                                .as_mut()
                                .expect("calibration active")
                                .on_tap_raw(event.raw, Path::new(TOUCH_CALIB_PATH));
                            if outcome == CalibrationOutcome::Saved {
                                touch_transform.reload();
                                calibration = None;
                                // Re-poll so the resumed UI shows fresh state.
                                ctx = source.build_context();
                                last_state_poll = now;
                                tracing::info!("touch calibration saved; resuming UI");
                            }
                        }
                        // Repaint immediately: the next target, or the resumed
                        // UI when the fit just landed.
                        let canvas = build_canvas(&calibration, &navigator, &ctx, &palette);
                        present_frame(&writer, bpp, &canvas);
                        last_render = Some(now);
                    } else {
                        // Normal UI: route the gesture through the navigator
                        // against the live context (some pages lay their hit
                        // zones out from state), then act on the dispatch.
                        let now_ms = touch_clock.elapsed().as_millis() as i64;
                        let dispatch = navigator.on_touch(&ctx, &event.gesture, now_ms);
                        if matches!(dispatch, Dispatch::RouteChanged(_) | Dispatch::ModalChanged(_))
                        {
                            // A tab switch or modal push/pop moves the active
                            // surface: pull fresh state for the new page, then
                            // repaint immediately instead of waiting out the
                            // previous page's refresh period.
                            ctx = source.build_context();
                            last_state_poll = now;
                            let canvas = build_canvas(&calibration, &navigator, &ctx, &palette);
                            present_frame(&writer, bpp, &canvas);
                            last_render = Some(now);
                        }
                        // A page-defined custom key (slider drag, list row) or an
                        // inert tap has no navigator-owned surface change; the
                        // interaction boost already quickened the next render.
                    }
                }
            }
            _ = sighup.recv() => {
                // Re-read the display config and rebuild the palette in place.
                // Force a render next tick so the new theme paints immediately.
                let fresh = conf::parse(Path::new(conf::DISPLAY_CONF_PATH));
                palette = palette_from_conf(&fresh);
                last_render = None;
                tracing::info!("received SIGHUP; reloaded display config");
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

    writer.cleanup();
    tracing::info!("ados-display stopped (native page UI)");
    Ok(())
}

/// The full set of pages the native page UI registers — the five tab roots plus
/// the detail pages the dashboard / more overflow drill into. Mirrors the
/// Python bootstrap registration order.
#[cfg(target_os = "linux")]
fn all_pages() -> Vec<Box<dyn ados_display::pages::Page>> {
    use ados_display::pages::{
        about::AboutDetailPage, channel_hops::ChannelHopsPage, dashboard::DashboardPage,
        diagnostics::DiagnosticsDetailPage, drone::DroneDetailPage, link_stats::LinkStatsPage,
        mesh::MeshDetailPage, more::MorePage, pair_drone::PairDroneDetailPage, plugin::PluginPage,
        radio_link::RadioLinkDetailPage, settings::SettingsPage, uplink::UplinkDetailPage,
        video::VideoPage, Page,
    };
    let pages: Vec<Box<dyn Page>> = vec![
        Box::new(DashboardPage),
        Box::new(VideoPage),
        Box::new(SettingsPage),
        Box::new(LinkStatsPage),
        Box::new(ChannelHopsPage),
        Box::new(MorePage),
        Box::new(RadioLinkDetailPage::new()),
        Box::new(UplinkDetailPage),
        Box::new(DroneDetailPage),
        Box::new(MeshDetailPage::new()),
        Box::new(AboutDetailPage),
        Box::new(PairDroneDetailPage),
        Box::new(DiagnosticsDetailPage),
        // The reserved data-driven page a plugin contributes content to. Not a
        // tab; reachable via a page-switch request for the `plugin` id.
        Box::new(PluginPage::new()),
    ];
    pages
}
