"""Framebuffer probe + paint helpers for the OLED service.

Hosts the two probe entry points (luma OLED on I2C, SPI LCD via
:class:`FrameBufferRenderer`), the active-screen blit helper, the
SPI-LCD dashboard / carousel paint helper, and the main async render
loop. All methods operate on attributes initialised in
:class:`OledService.__init__`.
"""

from __future__ import annotations

from PIL import Image, ImageDraw

from ados.services.ui.renderers.framebuffer import FrameBufferRenderer

from .constants import (
    CONTRAST_ACTIVE,
    CONTRAST_DIM,
    HEIGHT,
    IDLE_DIM_SECONDS,
    IDLE_LCD_FLOOR_HZ,
    IDLE_LCD_FLOOR_SECONDS,
    INVERT_PERIOD_SECONDS,
    WIDTH,
)
from .menu_tree import _now


class _FramebufferMixin:
    """Mixin: device probe, framebuffer probe, screen paint, render loop."""

    def _probe_device(self) -> bool:
        """Try SSD1306 then SH1106 at 0x3C and 0x3D. Return True on bind."""
        from .service import log

        try:
            from luma.core.interface.serial import i2c
            from luma.oled.device import sh1106, ssd1306
        except Exception as exc:
            log.warning("luma_import_failed", error=str(exc))
            return False

        for addr in (0x3C, 0x3D):
            for name, cls in (("ssd1306", ssd1306), ("sh1106", sh1106)):
                try:
                    serial = i2c(port=1, address=addr)
                    dev = cls(serial, width=WIDTH, height=HEIGHT)
                    dev.contrast(CONTRAST_ACTIVE)
                    self._device = dev
                    self._driver_name = f"{name}@0x{addr:02X}"
                    log.info("oled_bound", driver=name, address=f"0x{addr:02X}")
                    return True
                except Exception as exc:
                    log.debug(
                        "oled_probe_failed",
                        driver=name,
                        address=f"0x{addr:02X}",
                        error=str(exc),
                    )
        return False

    def _probe_framebuffer(self) -> bool:
        """Bind the SPI LCD framebuffer renderer when it is present.

        Returns True if a usable framebuffer was found. The OLED can
        still be absent in this case; the service runs as long as at
        least one render target is bound.
        """
        from .service import log

        try:
            renderer = FrameBufferRenderer.probe()
        except Exception as exc:  # noqa: BLE001
            log.warning("framebuffer_probe_failed", error=str(exc))
            return False
        if renderer is None:
            return False
        self._fb_renderer = renderer
        log.info(
            "framebuffer_bound",
            width=renderer.actual_width,
            height=renderer.actual_height,
            bpp=renderer.bpp,
        )
        return True

    def _render_to_framebuffer(self) -> None:
        """Paint to the SPI LCD framebuffer.

        Two paths:

        * Dashboard (preferred when available): pass the live state
          dict to the native-resolution dashboard renderer and blit
          the resulting 480x320 RGB image straight to the panel. No
          carousel — the dashboard composes ALL critical info on one
          screen.
        * Legacy upscale: paint the OLED carousel screen at 128x64
          and let the framebuffer renderer NEAREST-upscale it. This
          path runs when the dashboard module is unavailable (older
          agents, broken import) so the panel always shows
          something.
        """
        from .service import log

        if self._fb_renderer is None:
            return
        # Native dashboard path.
        if self._dashboard_render is not None:
            try:
                img = self._dashboard_render(self._state)
                self._fb_renderer.present(img)
                return
            except Exception as exc:  # noqa: BLE001
                log.warning(
                    "dashboard_render_failed",
                    error=str(exc),
                )
                # Fall through to the carousel as a safety net.
        try:
            img = Image.new("1", (WIDTH, HEIGHT), 0)
            draw = ImageDraw.Draw(img)
            self._paint_active_screen(draw, WIDTH, HEIGHT)
            self._fb_renderer.present(img)
        except Exception as exc:  # noqa: BLE001
            log.warning("framebuffer_render_failed", error=str(exc))

    async def _render_forever(self) -> None:
        """Main draw loop. Advances status screens every AUTO_CYCLE_SECONDS."""
        import asyncio

        from .service import log

        if self._device is None and self._fb_renderer is None:
            return
        from luma.core.render import canvas

        last_advance = _now()
        while not self._stop.is_set():
            now = _now()

            # Pick up SIGHUP-driven config reloads.
            if self._reload_requested:
                self._reload_requested = False
                self._reload_ui_config()
                last_advance = now

            # Pick up REST-arm of the calibration wizard. The shared
            # session bumps its generation counter every time a remote
            # POST /calibrate/start fires; if we are in lcd_page mode
            # and the counter advanced, engage the wizard so the
            # operator on the bench sees the targets.
            if self._page_navigator is not None and self._fb_renderer is not None:
                self._maybe_engage_remote_calibration()

            # Pick up REST-driven page-set requests. Same pattern: the
            # REST handler atomically writes the request file, this
            # watcher consumes + unlinks it on the next render tick.
            if self._page_navigator is not None:
                await self._maybe_apply_page_request()

            # First-boot unset override: when the node is mesh-capable
            # but role is still unset, take over the status cycle until
            # the operator sets a role. Any button press enters the
            # Mesh submenu directly.
            if self._first_boot_unset() and self._mode not in ("menu", "overlay"):
                self._mode = "unset"

            # Idle auto-dim. Honors the auto_dim_enabled flag from config.
            idle = now - self._last_button_ts
            if (
                self._auto_dim_enabled
                and not self._dimmed
                and idle >= IDLE_DIM_SECONDS
            ):
                self._set_contrast(CONTRAST_DIM)
                self._dimmed = True

            # Periodic pixel invert for burn-in mitigation. The cycle
            # clock is reset on every button press so the user never
            # sees an inverted screen immediately after interacting.
            if now - self._last_invert_ts >= INVERT_PERIOD_SECONDS:
                self._set_invert(not self._inverted)
                self._last_invert_ts = now

            # Auto-advance status screens using the live cycle period.
            n_screens = len(self._active_screens)
            if (
                self._mode == "status"
                and n_screens > 0
                and (now - last_advance) >= self._cycle_seconds
            ):
                self._screen_idx = (self._screen_idx + 1) % n_screens
                last_advance = now

            if self._device is not None and self._mode not in (
                "lcd_page",
                "calibrate",
            ):
                try:
                    with canvas(self._device) as draw:
                        self._paint_active_screen(draw, WIDTH, HEIGHT)
                except Exception as exc:
                    log.warning("render_failed", error=str(exc))

            # Mirror the same screen onto the SPI LCD when one is bound.
            # Both surfaces can run at once on a bench rig that has both
            # an OLED HAT and an LCD HAT plugged in; the framebuffer call
            # is a no-op when fb_renderer is None.
            if self._mode == "lcd_page":
                await self._render_lcd_page()
                # Tear down the video tap when the operator has been
                # off the Video tab for the inactivity grace. Keeps
                # the rest of the LCD UI snappy when video isn't in
                # use without forcing a cold start every tab switch.
                await self._maybe_tear_down_video_tap()
            elif self._mode == "calibrate":
                self._render_calibration()
            else:
                self._render_to_framebuffer()

            # Refresh cadence: pages declare a preferred Hz; status
            # carousel and overlays render at 1 Hz so a typical status
            # screen (clock, RSSI, role badge) doesn't repaint at 5 fps
            # when nothing has actually changed. The framebuffer renderer
            # also fast-skips identical frames so a higher rate would
            # mostly cost hash + tobytes overhead, but 1 Hz also drops
            # the wakeups themselves.
            tick_period = 1.0
            if self._mode == "lcd_page" and self._page_navigator is not None:
                page = self._page_navigator.current_page()
                hz = float(getattr(page, "refresh_hz", 5.0) or 5.0)
                if hz > 0:
                    tick_period = max(0.02, 1.0 / hz)
                # Idle floor: stretch the tick when the operator has
                # not pressed a button or touched the screen for a while.
                # Building a 480x320 PIL image in Python at the page's
                # declared 5 Hz costs nearly a full core on a Pi 4B; on
                # a benchtop unit nobody is looking at the LCD and this
                # CPU is wasted scheduler time that would otherwise go
                # to mediamtx serving the remote WebRTC viewer. The
                # floor only applies in lcd_page mode (status mode is
                # already low-frequency, overlays handle their own
                # cadence). Any button or touch event resets the
                # underlying timestamp via _consume_buttons /
                # _consume_touch_gestures, so the loop returns to the
                # page's full rate on the next tick.
                if idle >= IDLE_LCD_FLOOR_SECONDS and IDLE_LCD_FLOOR_HZ > 0:
                    tick_period = max(tick_period, 1.0 / IDLE_LCD_FLOOR_HZ)
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=tick_period)
            except TimeoutError:
                continue
