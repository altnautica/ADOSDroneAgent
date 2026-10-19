//! The native in-process page UI loop of the `ados-display` daemon.

use super::*;

/// Drive the native in-process page UI: build the navigator + state source,
/// then on each tick rebuild the page context, render the active page, pack the
/// frame, and present it through the already-running off-thread writer.
///
/// Two cadences run on one timer. The agent state is re-polled every 5 s (the
/// status pages tolerate a few seconds of staleness and a faster poll burns a
/// core on an SBC that is also serving the video chain); the panel re-renders
/// at the active page's
/// `refresh_hz`, floored to 0.5 Hz when idle so the dashboard's clock-second
/// paint does not waste a core. `lcd-fb-stats.json` is mirrored at 1 Hz, and a
/// remote `POST /api/v1/display/page` request is drained each render tick.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_page_ui(
    mut writer: crate::fb_writer::FbWriter,
    bpp: u32,
    rotation: i32,
    conf_blob: &std::collections::BTreeMap<String, String>,
    xres: u32,
    yres: u32,
) -> Result<()> {
    use std::time::{Duration, Instant};

    use tokio::signal::unix::{signal, SignalKind};

    use crate::calibration::{
        take_recalibrate_flag, CalibrationController, CalibrationOutcome, RECALIBRATE_FLAG_PATH,
        TOUCH_CALIB_SKIPPED_PATH,
    };
    use crate::graphics::palette::{self, Palette};
    use crate::graphics::primitives::Canvas;
    use crate::navigator::{Dispatch, PageNavigator};
    use crate::pages::calibration::render_calibration;
    use crate::pages::plugin::PLUGIN_PAGE_ID;
    use crate::pages::{PageContext, PanelAction};
    use crate::panel_action::{AgentWriter, Outcome};
    use crate::render_loop::{FramePresenter, TICK_GRANULARITY};
    use crate::sidecar::{write_plugin_tap, LcdFbStats, LCD_FB_STATS_PATH, LCD_SNAPSHOT_PATH};
    use crate::state_poller::StatePoller;
    use crate::state_source::StateSource;
    use crate::touch_input::{TouchTransformHandle, TOUCH_CALIB_PATH};
    use crate::widgets::draw_ack_line;
    use ados_hid::touch::GestureKind;

    // State poll cadence — matches the Python service's POLL_PERIOD_SECONDS.
    const STATE_POLL_PERIOD: Duration = Duration::from_secs(5);
    // Idle render floor — matches IDLE_LCD_FLOOR_HZ (0.5 Hz → 2 s).
    const IDLE_RENDER_PERIOD: Duration = Duration::from_millis(2000);
    // After any touch the panel repaints at this rate for BOOST_WINDOW so a drag
    // or tab switch tracks the finger, then settles back to the page cadence so
    // a quiet panel does not pin a core.
    const BOOST_HZ: f32 = 20.0;
    const BOOST_WINDOW: Duration = Duration::from_millis(500);
    // The calibration screen is static between taps (the tap path repaints it
    // immediately), so a modest base cadence keeps it lively without spinning.
    const CALIB_REFRESH_HZ: f32 = 5.0;
    // How long a panel action's outcome line stays on screen.
    const ACK_LINGER: Duration = Duration::from_secs(4);

    // Build the frame for the active surface: the calibration wizard when one is
    // running (it owns the whole panel, no navigator chrome), else the
    // navigator's current page (with a just-tapped tab painted pressed) and the
    // latest action outcome over its foot. `touch_clock` is the clock the touch
    // path stamps taps with.
    fn build_canvas(
        calibration: &Option<CalibrationController>,
        navigator: &PageNavigator,
        ctx: &PageContext,
        palette: &Palette,
        ack: &Option<(Outcome, Instant)>,
        touch_clock: Instant,
    ) -> Canvas {
        match calibration {
            Some(ctrl) => render_calibration(ctrl, palette),
            None => {
                let now_ms = touch_clock.elapsed().as_millis() as i64;
                let mut canvas = navigator.render_active(ctx, palette, now_ms);
                if let Some((outcome, at)) = ack {
                    if at.elapsed() < ACK_LINGER {
                        draw_ack_line(&mut canvas, palette, &outcome.message, outcome.ok);
                    }
                }
                canvas
            }
        }
    }

    // Act on a page-defined key. On the plugin page it is a zone tap for the
    // plugin that owns the page. Elsewhere the page resolves it: an overlay
    // toggle repaints (returns true), an agent write runs off the loop and posts
    // its outcome to `acks`. Taps on system pages never reach plugins.
    fn handle_custom(
        key: &str,
        navigator: &PageNavigator,
        ctx: &PageContext,
        writer: &std::sync::Arc<AgentWriter>,
        acks: &tokio::sync::mpsc::Sender<Outcome>,
        ts_ms: i64,
    ) -> bool {
        if navigator.current_page_id() == PLUGIN_PAGE_ID {
            let _ = write_plugin_tap(key, ts_ms);
            return false;
        }
        match navigator.custom_action(key, ctx) {
            Some(PanelAction::Repaint) => true,
            Some(PanelAction::Agent(req)) => {
                tracing::info!(method = req.method, path = req.path, "panel action");
                let writer = writer.clone();
                let acks = acks.clone();
                tokio::task::spawn_blocking(move || {
                    let outcome = writer.send(&req);
                    let _ = acks.blocking_send(outcome);
                });
                false
            }
            None => false,
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

    // The tab bar follows the node profile and the persisted screen list; it
    // is rebuilt in place on SIGHUP (the screens write route signals this unit).
    let panel = crate::panel_conf::PanelConf::load();
    tracing::info!(profile = %panel.profile, tabs = ?panel.tab_ids(), "panel page set");
    // The live button mapping: the power prompt names its keys, and the button
    // reader below resolves board-pin presses through it.
    let button_layout = crate::button_source::ButtonLayout::load(Path::new(
        crate::host_identity::BOARD_SIDECAR_PATH,
    ));
    let mut navigator = PageNavigator::new(all_pages(&button_layout), &panel.tab_ids());
    // The agent state is read on its own thread; the loop only ever borrows the
    // latest context, so a wedged agent never stalls touch, render or watchdog.
    let poller = StatePoller::spawn(StateSource::new(), STATE_POLL_PERIOD);
    let mut ctx_rx = poller.subscribe();
    let mut ctx: PageContext = ctx_rx.borrow_and_update().clone();
    // Cleared if the poller thread ever ends so its select arm cannot spin.
    let mut ctx_open = true;
    let mut presenter = FramePresenter::new(bpp, xres, yres);

    // Panel actions run off the loop; their outcomes come back here and are
    // painted for ACK_LINGER, then the state is re-polled so the page shows
    // what the agent now reports.
    let agent_writer = std::sync::Arc::new(AgentWriter::local());
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::channel::<Outcome>(8);
    let mut ack: Option<(Outcome, Instant)> = None;

    // The touch reader runs as its own task and posts each classified gesture
    // here; the select! loop turns it into a navigator transition. A small
    // bounded buffer is plenty — strokes arrive at human speed and each is
    // applied within one tick. The reader maps raw ADC samples to LCD pixels
    // for the configured rotation, so the navigator receives the same
    // coordinate frame the pages lay out in.
    // The touch transform is shared with the reader so a fresh calibration
    // reloads in place; the render loop also reads it to gate the auto-prompt.
    let touch_transform = TouchTransformHandle::new(rotation);
    let (touch_tx, mut touch_rx) = tokio::sync::mpsc::channel::<crate::touch_input::TouchEvent>(16);
    {
        // The reader rediscovers a missing or lost panel for the life of the
        // process and returns only once this loop drops its receiver.
        let handle = touch_transform.clone();
        tokio::spawn(crate::touch_input::run_touch_reader(handle, touch_tx));
    }
    // Cleared if the touch channel ever closes so its select arm cannot spin.
    let mut touch_open = true;

    // Front-panel buttons. `ados-hid` already classifies and publishes them; this
    // is the transport that lets a press reach the navigator, which has always
    // known what to do with one. A node whose `ados-pic` is absent or down simply
    // never yields an event — the reader retries quietly and the UI is unaffected.
    let mut button_rx = crate::button_source::spawn_button_reader(
        std::path::PathBuf::from(crate::button_source::BUTTONS_SOCK),
        button_layout,
    );

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    // SIGHUP is the in-place config-reload signal. The agent's REST handlers
    // SIGHUP this unit after persisting a UI config change; without a handler
    // the default disposition would terminate the daemon. The theme and the
    // panel page set reload on it.
    let mut sighup = signal(SignalKind::hangup())?;

    let mut tick = tokio::time::interval(TICK_GRANULARITY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // The snapshot PNG is encoded at most this often. The GCS preview polls at
    // 1 Hz, so a faster snapshot cadence would burn a core re-encoding frames no
    // remote ever fetches; the panel itself still re-renders at the page cadence.
    const SNAPSHOT_PERIOD: Duration = Duration::from_secs(1);

    let now = Instant::now();
    let mut last_render: Option<Instant> = None;
    let mut last_latency_write = now;
    let mut last_snapshot: Option<Instant> = None;
    // The active calibration wizard, if any. While `Some`, the loop paints the
    // calibration screen and routes every tap to it instead of the navigator.
    let mut calibration: Option<CalibrationController> = None;
    // Set when an auto-launched wizard closed itself on the idle timeout, so it
    // does not re-open for the rest of this run. Only a recalibration request
    // launches it again before the next start.
    let mut calibration_idle_closed = false;
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
                // is visibly off, so prompt for a fit. An operator who skipped
                // it (persistent marker) is not prompted again, and a wizard
                // that timed out stays closed for this run.
                if calibration.is_none()
                    && !calibration_idle_closed
                    && touch_transform.touch_present()
                    && !touch_transform.is_calibrated()
                    && !Path::new(TOUCH_CALIB_SKIPPED_PATH).exists()
                {
                    tracing::info!("uncalibrated touch panel; launching calibration wizard");
                    calibration = Some(CalibrationController::new(rotation));
                    last_render = None;
                }

                // Close a wizard nobody is tapping: the digitizer may be absent
                // or unreadable, and the status screen must come back.
                if calibration.as_ref().is_some_and(|c| c.idle_expired(now)) {
                    tracing::info!("touch calibration idle; closing the wizard");
                    calibration = None;
                    calibration_idle_closed = true;
                    poller.refresh_now();
                    last_render = None;
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
                    let canvas = build_canvas(&calibration, &navigator, &ctx, &palette, &ack, touch_clock);

                    // Mirror the frame to the snapshot PNG so the REST snapshot
                    // endpoint serves the live panel. Throttled to ~1 Hz, skipped
                    // for an unchanged canvas, encoded on the writer thread.
                    let snapshot_due = last_snapshot
                        .map(|t| now.duration_since(t) >= SNAPSHOT_PERIOD)
                        .unwrap_or(true);
                    let snapshot_to = if snapshot_due {
                        last_snapshot = Some(now);
                        Some(std::path::PathBuf::from(LCD_SNAPSHOT_PATH))
                    } else {
                        None
                    };
                    presenter.present(&writer, &canvas, snapshot_to);
                    last_render = Some(now);
                }

                // Mirror writer stats + consume a recalibration request at ~1 Hz.
                if now.duration_since(last_latency_write) >= Duration::from_secs(1) {
                    let fb_stats: LcdFbStats = writer.stats().into();
                    if let Err(e) = fb_stats.write_to(Path::new(LCD_FB_STATS_PATH)) {
                        tracing::debug!(error = %e, "lcd-fb-stats write failed");
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
            maybe_event = touch_rx.recv(), if touch_open => {
                // A completed stroke arrived from the reader task. `None` means
                // the reader is gone: disable this arm so the loop does not spin
                // on a closed channel, and keep the rest of the UI running.
                if maybe_event.is_none() {
                    touch_open = false;
                    tracing::warn!("touch reader ended; touch input disabled");
                }
                if let Some(event) = maybe_event {
                    let now = Instant::now();
                    last_interaction = Some(now);

                    if calibration.is_some() {
                        // Calibration mode: a tap on Skip arms or confirms it;
                        // any other tap places a sample on the current target.
                        // On the final accepted tap the fit is saved, the live
                        // transform reloads, and the normal UI resumes; a
                        // confirmed Skip resumes it on the fallback transform.
                        if event.gesture.kind == GestureKind::Tap {
                            let outcome = calibration
                                .as_mut()
                                .expect("calibration active")
                                .on_tap(
                                    (event.gesture.end_x, event.gesture.end_y),
                                    event.raw,
                                    Path::new(TOUCH_CALIB_PATH),
                                    Path::new(TOUCH_CALIB_SKIPPED_PATH),
                                );
                            match outcome {
                                CalibrationOutcome::Saved => {
                                    touch_transform.reload();
                                    tracing::info!("touch calibration saved; resuming UI");
                                }
                                CalibrationOutcome::Skipped => {
                                    tracing::info!("touch calibration skipped; resuming UI");
                                }
                                CalibrationOutcome::Continue => {}
                            }
                            if outcome != CalibrationOutcome::Continue {
                                calibration = None;
                                // Re-poll so the resumed UI shows fresh state.
                                poller.refresh_now();
                            }
                        }
                        // Repaint immediately: the next target, or the resumed
                        // UI when the fit just landed.
                        let canvas = build_canvas(&calibration, &navigator, &ctx, &palette, &ack, touch_clock);
                        presenter.present(&writer, &canvas, None);
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
                            // surface: ask for fresh state for the new page (it
                            // repaints again when that lands), and repaint now
                            // instead of waiting out the previous page's period.
                            poller.refresh_now();
                            let canvas = build_canvas(&calibration, &navigator, &ctx, &palette, &ack, touch_clock);
                            presenter.present(&writer, &canvas, None);
                            last_render = Some(now);
                        }
                        // A page-defined custom key: a plugin zone tap, an overlay
                        // toggle (repaint now), or an agent write whose outcome
                        // arrives on the ack channel.
                        if let Dispatch::Custom(key) = dispatch {
                            if handle_custom(&key, &navigator, &ctx, &agent_writer, &ack_tx, now_ms) {
                                let canvas = build_canvas(&calibration, &navigator, &ctx, &palette, &ack, touch_clock);
                                presenter.present(&writer, &canvas, None);
                                last_render = Some(now);
                            }
                        }
                    }
                }
            }
            maybe_button = button_rx.recv() => {
                // A decoded front-panel press. `None` means the reader task is
                // gone; keep looping so the rest of the UI survives, exactly as
                // the touch arm does.
                if let Some(event) = maybe_button {
                    let now = Instant::now();
                    last_interaction = Some(now);

                    // While the calibration wizard is engaged the panel has no
                    // usable coordinate frame; its on-screen Skip is the way
                    // out, so a button must not navigate away from it.
                    if calibration.is_none() {
                        let dispatch = navigator.on_button(&event);
                        if matches!(
                            dispatch,
                            Dispatch::RouteChanged(_) | Dispatch::ModalChanged(_)
                        ) {
                            // Same handling as a touch: the active surface moved,
                            // so ask for fresh state for the new page and repaint
                            // now rather than waiting out the old page's period.
                            poller.refresh_now();
                            let canvas = build_canvas(&calibration, &navigator, &ctx, &palette, &ack, touch_clock);
                            presenter.present(&writer, &canvas, None);
                            last_render = Some(now);
                        }
                        // A custom key (including an unmapped-button fallback to an
                        // on-page action) is handled exactly as a touch-zone key.
                        if let Dispatch::Custom(key) = dispatch {
                            let ts_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0);
                            if handle_custom(&key, &navigator, &ctx, &agent_writer, &ack_tx, ts_ms) {
                                let canvas = build_canvas(&calibration, &navigator, &ctx, &palette, &ack, touch_clock);
                                presenter.present(&writer, &canvas, None);
                                last_render = Some(now);
                            }
                        }
                    }
                }
            }
            maybe_ack = ack_rx.recv() => {
                // A panel action finished: show its outcome now and re-poll so
                // the page reflects what the agent reports after the write.
                if let Some(outcome) = maybe_ack {
                    if outcome.ok {
                        tracing::info!(message = %outcome.message, "panel action done");
                    } else {
                        tracing::warn!(message = %outcome.message, "panel action failed");
                    }
                    let now = Instant::now();
                    ack = Some((outcome, now));
                    if calibration.is_none() {
                        poller.refresh_now();
                    }
                    last_render = None;
                }
            }
            changed = ctx_rx.changed(), if ctx_open => {
                // A fresh agent-state context landed from the poller thread:
                // adopt it and repaint on the next tick.
                if changed.is_ok() {
                    ctx = ctx_rx.borrow_and_update().clone();
                    last_render = None;
                } else {
                    ctx_open = false;
                    tracing::warn!("state poller ended; panel keeps the last context");
                }
            }
            _ = sighup.recv() => {
                // Re-read the display config and rebuild the palette in place,
                // then the panel page set (the tab list) from the agent config.
                // Force a render next tick so both paint immediately.
                let fresh = conf::parse(Path::new(conf::DISPLAY_CONF_PATH));
                palette = palette_from_conf(&fresh);
                let panel = crate::panel_conf::PanelConf::load();
                navigator.set_tabs(&panel.tab_ids());
                last_render = None;
                tracing::info!(tabs = ?navigator.tabs(), "received SIGHUP; reloaded display config");
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
