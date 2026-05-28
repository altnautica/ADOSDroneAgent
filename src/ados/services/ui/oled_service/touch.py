"""Touch + calibration helpers for the OLED service.

Hosts:

* the evdev touch-device presence probe,
* the on-disk calibration rotation check,
* the calibration wizard renderer + gesture dispatch,
* the touch gesture consumer that fans out into page-touch / tab-tap
  / calibration handlers,
* the helper that exits calibrate mode cleanly, and the watcher that
  picks up REST-armed calibration runs.
"""

from __future__ import annotations

import json
from pathlib import Path

from ados.core.paths import TOUCH_CALIB_PATH
from ados.services.ui.theme import current_palette
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.touch.recent import record_touch
from ados.services.ui.touch.session import get_session_registry

from .menu_tree import _now


class _TouchMixin:
    """Mixin: touch presence, calibration, gesture dispatch."""

    def _touch_present(self) -> bool:
        """Best-effort check whether an evdev touch device is bound."""
        try:
            from evdev import InputDevice, ecodes, list_devices
        except ImportError:
            return False
        for path in list_devices():
            try:
                dev = InputDevice(path)
            except OSError:
                continue
            try:
                caps = dev.capabilities().get(ecodes.EV_KEY, [])
                if ecodes.BTN_TOUCH in caps:
                    return True
            finally:
                try:
                    dev.close()
                except Exception:  # noqa: BLE001
                    pass
        return False

    def _verify_calibration_rotation(self, current_rotation: int) -> None:
        """Warn + arm a recalibrate flag when the on-disk calib is stale.

        The wizard saves ``rotation_applied_at_save`` alongside the
        affine matrix. When the operator later changes display
        rotation from the settings page, the saved matrix no longer
        maps raw ADC reads to the new orientation correctly. Detect
        the mismatch on startup, write
        ``/run/ados/recalibrate.flag`` (the same one-shot flag the
        settings page uses) so the next run of the bootstrap launches
        the wizard, and log a warning.

        The flag is written but NOT consumed here — bootstrap unlinks
        it on read. That ordering is intentional: this method runs
        before bootstrap, so the flag we write is picked up on the
        same boot.
        """
        from .service import log

        if not TOUCH_CALIB_PATH.exists():
            return
        try:
            blob = json.loads(TOUCH_CALIB_PATH.read_text())
        except (OSError, ValueError):
            return
        if not isinstance(blob, dict):
            return
        if not blob.get("calibrated"):
            return
        saved = blob.get("rotation_applied_at_save")
        try:
            saved_int = int(saved) % 360
        except (TypeError, ValueError):
            return
        if saved_int == int(current_rotation) % 360:
            return
        log.warning(
            "touch_calibration_stale",
            saved_rotation=saved_int,
            current_rotation=current_rotation,
            msg=(
                "touch.calib was saved at a different rotation than "
                "the panel is configured for now; arming recalibrate "
                "flag so the wizard runs on the next bootstrap"
            ),
        )
        flag_path = Path("/run/ados/recalibrate.flag")
        try:
            flag_path.parent.mkdir(parents=True, exist_ok=True)
            flag_path.write_text("rotation_changed\n")
        except OSError as exc:
            log.warning(
                "recalibrate_flag_write_failed",
                path=str(flag_path),
                error=str(exc),
            )

    def _render_calibration(self) -> None:
        """Paint the active calibration wizard (or failure card)."""
        from .service import log

        if self._fb_renderer is None or self._calibration_wizard is None:
            return
        palette = current_palette()
        now_ms = int(_now() * 1000)
        if (
            self._calibration_failure_until_ms
            and now_ms < self._calibration_failure_until_ms
        ):
            img = self._calibration_wizard.render_failure(
                palette,
                rms_px=getattr(self, "_calibration_failure_rms", 0.0),
            )
        else:
            img = self._calibration_wizard.render(palette)
            self._calibration_failure_until_ms = 0
        try:
            self._fb_renderer.present(img)
        except Exception as exc:  # noqa: BLE001
            log.warning("framebuffer_present_failed", error=str(exc))

    async def _consume_touch_gestures(self) -> None:
        """Dispatch TouchGesture events to the active page or wizard."""
        from .service import log

        if self._touch_bridge is None:
            return
        async for gesture in self._touch_bridge.gesture_bus.subscribe():
            if self._stop.is_set():
                return
            try:
                await self._dispatch_gesture(gesture)
            except Exception as exc:  # noqa: BLE001
                log.warning(
                    "touch_dispatch_failed",
                    error=str(exc),
                    kind=gesture.kind,
                )

    async def _dispatch_gesture(self, gesture: TouchGesture) -> None:
        from .service import log

        # Record every dispatched gesture in the recent-touches ring
        # so the GCS Display sub-view can show a tail of activity.
        try:
            active_page = (
                self._page_navigator.active_page_id
                if self._page_navigator is not None
                else self._mode
            )
            record_touch(
                kind=gesture.kind,
                x=int(gesture.start_x),
                y=int(gesture.start_y),
                page=active_page,
                timestamp_ms=int(gesture.start_t_ms),
            )
        except Exception as exc:  # noqa: BLE001
            log.debug("touch_recent_record_failed", error=str(exc))
        if self._mode == "calibrate" and self._calibration_wizard is not None:
            await self._handle_calibration_gesture(gesture)
            return
        if self._mode != "lcd_page" or self._page_navigator is None:
            return
        navigator = self._page_navigator
        ctx = self._page_context
        # Tab bar zones live at y=276..320 on the LCD canvas.
        if gesture.start_y >= 320 - 44 and gesture.kind == "tap":
            await self._handle_tab_tap(gesture)
            return
        # Otherwise dispatch to active page or topmost modal.
        if ctx is None:
            return
        page = navigator.current_page()
        # Translate to page-local coords (subtract chrome offset).
        local_x = gesture.start_x
        local_y = gesture.start_y - 32
        for zone in page.hit_zones(ctx):
            if zone.contains(local_x, local_y):
                navigator.record_tap(zone.id, int(_now() * 1000))
                try:
                    await page.on_touch(ctx, zone, gesture)
                except Exception as exc:  # noqa: BLE001
                    log.warning(
                        "page_on_touch_failed",
                        page_id=page.id,
                        zone=zone.id,
                        error=str(exc),
                    )
                return

    async def _handle_tab_tap(self, gesture: TouchGesture) -> None:
        from .service import log

        if self._page_navigator is None:
            return
        # Re-derive zones using the same layout the renderer uses.
        # We recompute rather than caching them so a different chrome
        # height in the future does not get a stale dispatch.
        from ados.services.ui.chrome.bottom_tab_bar import (
            _TABS,
            TAB_COUNT,
            TAB_WIDTH,
            page_id_for_zone,
        )
        from ados.services.ui.chrome.bottom_tab_bar import (
            HEIGHT as TAB_HEIGHT,
        )
        if gesture.start_y < 320 - TAB_HEIGHT:
            return
        index = max(0, min(TAB_COUNT - 1, gesture.start_x // TAB_WIDTH))
        # Read zone ids straight from the _TABS source-of-truth so the
        # dispatch stays correct when tabs are added or removed.
        # Previously this was a hardcoded 4-tuple which silently
        # IndexError'd on the 5th tap when TAB_COUNT grew to 5 (the
        # ChannelHopsPage entry).
        zone_id = _TABS[index][0]
        page_id = page_id_for_zone(zone_id)
        if page_id is None:
            return
        if not self._page_navigator.has(page_id):
            log.info(
                "tab_tapped_unregistered_page",
                page_id=page_id,
                zone=zone_id,
            )
            self._page_navigator.record_tap(zone_id, int(_now() * 1000))
            return
        self._page_navigator.record_tap(zone_id, int(_now() * 1000))
        # A tab-bar tap from inside any drilldown should pop the
        # entire modal stack first so the operator returns to the
        # tab's root page instead of landing on a stale modal that
        # belonged to a different tab. The pop is best-effort: if
        # any on_leave callback throws we still continue to go().
        while self._page_navigator.modal_stack:
            try:
                await self._page_navigator.pop_modal(ctx=self._page_context)
            except Exception as exc:  # noqa: BLE001
                log.debug("tab_tap_modal_pop_failed", error=str(exc))
                break
        await self._page_navigator.go(page_id, ctx=self._page_context)

    async def _handle_calibration_gesture(self, gesture: TouchGesture) -> None:
        from .service import log

        wizard = self._calibration_wizard
        if wizard is None:
            return
        # If we're showing a failure card, any tap restarts the
        # wizard. Otherwise process the tap as a sample.
        now_ms = int(_now() * 1000)
        if self._calibration_failure_until_ms and now_ms < self._calibration_failure_until_ms:
            if gesture.kind == "tap":
                wizard.reset_for_retry()
                self._calibration_failure_until_ms = 0
            return
        if gesture.kind == "long_press":
            wizard.skip()
            self._exit_calibration()
            return
        if gesture.kind != "tap":
            return
        # Map the LCD-pixel tap back to raw ADC coordinates by
        # recovering the last raw sample from the bridge. The bridge
        # stored it in the gesture's samples sequence — but those are
        # already LCD-space. For the wizard's submit_sample contract
        # we need raw ADC. The bridge keeps the raw last-x/last-y in
        # its private state; expose it via a small helper rather
        # than reaching in.
        raw = self._touch_bridge_raw_for(gesture)
        wizard.submit_sample(wizard.step, raw[0], raw[1])
        # Mirror the on-LCD progression into the shared session so a
        # remote /calibrate/status poll sees the live step counter
        # without racing the wizard's private state.
        try:
            registry = get_session_registry()
            registry.mirror_step(
                step=wizard.step,
                samples=list(getattr(wizard, "_samples", [])),
            )
        except Exception as exc:  # noqa: BLE001
            log.debug("calibration_mirror_step_failed", error=str(exc))
        if wizard.is_done:
            result = wizard.complete()
            if result.success:
                # Reload the bridge so live gestures use the new map.
                if self._touch_bridge is not None:
                    self._touch_bridge.reload_calibration()
                log.info(
                    "lcd_calibration_complete",
                    rms_px=result.rms_px,
                )
                try:
                    get_session_registry().mirror_complete(
                        rms_residual_px=result.rms_px,
                        success=True,
                    )
                except Exception as exc:  # noqa: BLE001
                    log.debug("calibration_mirror_complete_failed", error=str(exc))
                self._exit_calibration()
            else:
                log.warning(
                    "lcd_calibration_rejected",
                    rms_px=result.rms_px,
                    error=result.error,
                )
                try:
                    get_session_registry().mirror_complete(
                        rms_residual_px=result.rms_px,
                        success=False,
                    )
                except Exception as exc:  # noqa: BLE001
                    log.debug("calibration_mirror_failed_failed", error=str(exc))
                self._calibration_failure_rms = result.rms_px
                # Show the failure card for 4 seconds, then auto-retry.
                self._calibration_failure_until_ms = now_ms + 4000

    def _touch_bridge_raw_for(self, gesture: TouchGesture) -> tuple[int, int]:
        """Return the last raw ADC sample the bridge captured.

        The wizard fits raw -> LCD; gesture coordinates are already
        LCD-space (post-transform). For the first tap before any
        calibration exists, the bridge is using the identity transform
        so feeding the LCD tap-position back as if it were raw would
        produce a near-identity matrix. To get a meaningful fit, we
        read the bridge's most recent raw values.
        """
        if self._touch_bridge is None:
            return (gesture.start_x, gesture.start_y)
        return (
            getattr(self._touch_bridge, "_last_x_raw", gesture.start_x),
            getattr(self._touch_bridge, "_last_y_raw", gesture.start_y),
        )

    def _exit_calibration(self) -> None:
        from .service import log

        self._calibration_wizard = None
        self._calibration_failure_until_ms = 0
        # Transition out of calibrate mode. Without this the render
        # loop keeps hitting the calibrate branch, _render_calibration
        # sees wizard=None and returns early, and the LCD freezes on
        # whatever frame was last painted (verified bench-side post
        # v0.18.13: 5/5 wizard frame stuck after a successful fit).
        # Flip to the page system when the framebuffer + navigator
        # are alive; fall back to the OLED carousel otherwise.
        if self._fb_renderer is not None and self._page_navigator is not None:
            self._mode = "lcd_page"
        else:
            self._mode = "status"
        # Mirror the terminal state into the shared session so a
        # remote /status poll sees in_progress=False after a tap-
        # driven completion path. The wizard.complete() success
        # already wrote the calibration file on disk; we only need
        # to flip the in_progress flag here.
        try:
            registry = get_session_registry()
            snap = registry.snapshot()
            if snap.in_progress:
                # Use mirror_complete with the previously-recorded
                # rms (or 0.0 fallback) and success=True so the
                # session settles cleanly. The on-disk calibration
                # file is the source of truth for "calibrated".
                registry.mirror_complete(
                    rms_residual_px=snap.rms_residual_px or 0.0,
                    success=True,
                )
        except Exception as exc:  # noqa: BLE001
            log.debug("calibration_session_mirror_failed", error=str(exc))

    def _maybe_engage_remote_calibration(self) -> None:
        """Engage calibrate mode if the REST surface armed a new run.

        The shared session's ``generation`` counter increments on every
        ``POST /calibrate/start``. When the OLED service sees a higher
        generation than the last one it acted on, it constructs a
        fresh wizard, switches mode, and bumps the watermark so the
        same arm doesn't fire twice.
        """
        from ados.services.ui.touch.calibrate import CalibrationWizard

        from .service import log

        try:
            registry = get_session_registry()
            snap = registry.snapshot()
        except Exception as exc:  # noqa: BLE001
            log.debug("calibration_session_read_failed", error=str(exc))
            return
        if snap.generation <= self._last_calibration_generation:
            return
        self._last_calibration_generation = snap.generation
        if not snap.in_progress:
            return
        # Build a fresh wizard at step 0. Even if a wizard was already
        # running we replace it so the GCS-armed run takes precedence.
        self._calibration_wizard = CalibrationWizard()
        self._calibration_wizard.start()
        self._mode = "calibrate"
        if self._touch_bridge is not None:
            self._touch_bridge.mode = "lcd_page"
        log.info("lcd_calibration_armed_remotely", generation=snap.generation)
