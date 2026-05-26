"""OledService composition + lifecycle (__init__, run, helpers).

Composes the four method mixins (framebuffer, touch, buttons,
display) into the concrete :class:`OledService`. Hosts the
constructor (all instance state), the orchestrator ``run()`` (probe
→ bootstrap → spawn tasks), the page-system bootstrap helper, the
video-tap construction + lifecycle coroutines, the first-boot mode
predicate, the REST page-request watcher, the video-tap teardown
helper, and the state-poll loop.

This module deliberately does NOT host ``main()`` / ``_amain()`` —
those live in :mod:`.service` so the existing test that monkeypatches
``service.load_config`` / ``service.ButtonEventBus`` /
``service.OledService`` continues to work without test churn.
"""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
from typing import Any

from ados.core.paths import LCD_PAGE_REQUEST_PATH
from ados.services.ui.events import ButtonEventBus
from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.dashboard import DashboardPage
from ados.services.ui.renderers import Renderer
from ados.services.ui.touch.calibrate import CalibrationWizard
from ados.services.ui.touch_input import TouchInputBridge

from .buttons import _ButtonsMixin
from .constants import AUTO_CYCLE_SECONDS, CONTRAST_ACTIVE
from .display import _DisplayMixin
from .framebuffer import _FramebufferMixin
from .menu_tree import MENU_TREE, _normalize_radio_fields, _now
from .screen_registry import (
    DEFAULT_SCREEN_ORDER,
    SCREEN_RENDERERS,
)
from .touch import _TouchMixin
from .video_tap import _VideoTapMixin

# Native-resolution dashboard renderer. Imported defensively so the
# OLED service still starts (and falls back to the legacy 4x upscale
# carousel) when the dashboards module fails to import for any
# reason.
try:
    from ados.services.ui.dashboards.groundnode_landscape import (
        render as render_groundnode_dashboard,
    )
except Exception:  # noqa: BLE001
    render_groundnode_dashboard = None  # type: ignore[assignment]


class OledService(
    _FramebufferMixin,
    _TouchMixin,
    _ButtonsMixin,
    _DisplayMixin,
    _VideoTapMixin,
):
    """Owns the OLED device, the render loop, and menu state."""

    def __init__(
        self,
        bus: ButtonEventBus,
        api_host: str = "127.0.0.1",
        api_port: int = 8080,
    ) -> None:
        self._bus = bus
        self._api_host = api_host
        self._api_port = api_port
        self._api_base = f"http://{api_host}:{api_port}/api/v1/ground-station"
        self._api_url = f"{self._api_base}/status"
        self._device = None
        self._driver_name = ""
        self._mode: str = "status"  # "status" | "menu" | "overlay" | "unset"
        self._screen_idx: int = 0
        self._menu_stack: list[tuple[list[dict[str, Any]], int]] = []
        self._menu_items: list[dict[str, Any]] = MENU_TREE
        self._menu_sel: int = 0
        self._last_button_ts: float = _now()
        self._last_invert_ts: float = _now()
        self._inverted: bool = False
        self._dimmed: bool = False
        self._state: dict[str, Any] = {}
        self._stop = asyncio.Event()
        # Overlay mode: mesh screens hijack the display while the
        # operator drives a pairing or role transition. _overlay_module
        # is the live renderer; _overlay_state is a small scratch dict
        # the screen uses for cursor position, timers, etc. The button
        # bus dispatches to _overlay_module.BUTTON_ACTIONS first when
        # the service is in overlay mode.
        self._overlay_id: str | None = None
        self._overlay_module: Any | None = None
        self._overlay_state: dict[str, Any] = {}
        # Shared HTTP client reused by overlay button handlers and the
        # state poller. Created in `run()` so the event loop is set.
        self._http: Any | None = None
        # Optional second poll that runs only while the pairing
        # accept-window overlay is active.
        self._pairing_poll_task: asyncio.Task | None = None
        # Dynamic screen list and OLED prefs are
        # rebuilt from `ground_station.ui` on SIGHUP. Initial values
        # are populated from `load_config()` in `_reload_ui_config()`.
        self._active_screens: list[tuple[str, Any]] = [
            (sid, SCREEN_RENDERERS[sid]) for sid in DEFAULT_SCREEN_ORDER
        ]
        self._cycle_seconds: float = AUTO_CYCLE_SECONDS
        self._auto_dim_enabled: bool = True
        self._brightness_active: int = CONTRAST_ACTIVE
        self._reload_requested: bool = False
        # Optional secondary render target. Bound when a framebuffer
        # carries a supported SPI LCD (e.g. ILI9486 via fbtft on Cubie A7Z +
        # Rock 5C + Pi). The renderer matches by driver NAME across all fb
        # indices (fb0 on a headless rig, fb1 when a DRM/HDMI driver claims
        # fb0), never by a fixed index. Stays None on benches that only have
        # the I2C OLED. When bound, the render loop paints the same screens
        # onto this surface in addition to (or instead of) the OLED.
        self._fb_renderer: Renderer | None = None
        # Dashboard renderer for the SPI LCD path. When bound (any
        # truthy callable) the framebuffer paints the dashboard at
        # native 480x320 instead of upscaling the OLED carousel.
        self._dashboard_render = render_groundnode_dashboard
        # Optional touch-input bridge. Translates ADS7846 events into
        # gestures and synthetic legacy button events. The mode is
        # flipped by the run loop based on whether the LCD is large
        # enough to host the page system.
        self._touch_bridge: TouchInputBridge | None = None
        # Page-system state. Bound only when the framebuffer
        # geometry can host the 480x320 page UI. The navigator owns
        # the active route and modal stack; the wizard owns
        # calibration. Both are None when the carousel is in charge.
        self._page_navigator: PageNavigator | None = None
        self._page_context: PageContext | None = None
        self._calibration_wizard: CalibrationWizard | None = None
        self._calibration_failure_until_ms: int = 0
        self._touch_consumer_task: asyncio.Task | None = None
        # Last-seen generation counter on the shared calibration
        # session. The render loop watches this so a remote
        # ``POST /api/v1/display/calibrate/start`` engages calibrate
        # mode on the next tick without waiting for a panel tap.
        self._last_calibration_generation: int = 0
        self._reload_ui_config()

    # ── lcd_page mode helpers ──────────────────────────────────

    def _bootstrap_page_system(self) -> None:
        """Construct the navigator, register pages, and pick initial mode.

        Called once during ``run()`` after the framebuffer probe
        succeeds and the framebuffer reports a geometry the page UI
        can host. Triggers the calibration wizard when the touch
        chip is present and no calibration file exists yet.
        """
        from ados.core.paths import TOUCH_CALIB_PATH
        from ados.services.ui.theme import current_palette
        from ados.services.ui.touch.transform import load as load_calib

        from .service import log

        if self._fb_renderer is None:
            return
        geom_ok = (
            getattr(self._fb_renderer, "actual_width", 0) >= 480
            and getattr(self._fb_renderer, "actual_height", 0) >= 320
        )
        if not geom_ok:
            return
        navigator = PageNavigator()
        navigator.register(DashboardPage())
        # Settings page lives at the same registry; the tab-bar route
        # for "settings" resolves to it. Imported locally to avoid
        # cycles with the page __init__ during early startup.
        from ados.services.ui.pages.channel_hops import ChannelHopsPage
        from ados.services.ui.pages.link_stats import LinkStatsPage
        from ados.services.ui.pages.settings import SettingsPage
        from ados.services.ui.pages.video import VideoPage

        navigator.register(SettingsPage())
        navigator.register(VideoPage())
        # Live link + decoder + system metrics page; replaces the old
        # More overflow menu (whose four rows now live under
        # Settings -> Maintenance). Tab bar's fourth tab routes here
        # via id="link_stats".
        navigator.register(LinkStatsPage())
        # Channel-hopping history (drone profile only renders real data;
        # GS profile sees the empty-state placeholder since the
        # supervisor lives only on the drone side). Tab bar's fifth
        # tab routes here via id="channel_hops".
        navigator.register(ChannelHopsPage())
        self._page_navigator = navigator
        # Surface the bridge's touch buses on the context so any page
        # that needs live drag tracking (settings list, slider modal,
        # enum picker scroll) can subscribe without reaching into
        # private bridge attributes.
        move_bus = (
            getattr(self._touch_bridge, "move_bus", None)
            if self._touch_bridge is not None
            else None
        )
        gesture_bus = (
            getattr(self._touch_bridge, "gesture_bus", None)
            if self._touch_bridge is not None
            else None
        )
        self._page_context = PageContext(
            state=self._state,
            palette=current_palette(),
            hostname=self._read_hostname(),
            http=None,  # bound later in run() once httpx client is alive
            framebuffer=self._fb_renderer,
            navigator=navigator,
            logger=log,
            touch_move_bus=move_bus,
            touch_event_bus=gesture_bus,
        )
        # The settings page can arm a one-shot recalibrate flag at
        # ``/run/ados/recalibrate.flag``. When present, force the
        # wizard regardless of whether a calibration file is already
        # on disk; unlink the flag immediately so a single press only
        # produces one wizard run.
        recalibrate_flag = Path("/run/ados/recalibrate.flag")
        force_recalibrate = False
        if recalibrate_flag.exists():
            force_recalibrate = True
            try:
                recalibrate_flag.unlink()
            except OSError:
                pass
        # If a touch chip exists and (a) no calibration file is present,
        # or (b) the operator just armed a recalibrate, launch the
        # wizard before landing on the dashboard. The wizard takes over
        # the full panel until it completes or the operator skips.
        if self._touch_present() and (
            force_recalibrate or load_calib(TOUCH_CALIB_PATH) is None
        ):
            self._calibration_wizard = CalibrationWizard()
            self._calibration_wizard.start()
            self._mode = "calibrate"
            log.info(
                "lcd_calibration_wizard_started",
                forced=force_recalibrate,
            )
        else:
            self._mode = "lcd_page"
            log.info(
                "lcd_page_mode_engaged",
                page_id=navigator.active_page_id,
            )

    async def _maybe_apply_page_request(self) -> None:
        """Honor a remote ``POST /api/v1/display/page`` request.

        Reads ``/run/ados/lcd-page-request.json``, routes the
        navigator to the requested page, and unlinks the file so the
        same request can't reapply on the next tick. Best-effort:
        any malformed payload is silently dropped and the file is
        unlinked so the watcher doesn't loop on it.
        """
        from .service import log

        if not LCD_PAGE_REQUEST_PATH.exists():
            return
        if self._page_navigator is None:
            try:
                LCD_PAGE_REQUEST_PATH.unlink()
            except OSError:
                pass
            return
        try:
            blob = json.loads(LCD_PAGE_REQUEST_PATH.read_text())
        except (OSError, json.JSONDecodeError) as exc:
            log.debug("lcd_page_request_unreadable", error=str(exc))
            try:
                LCD_PAGE_REQUEST_PATH.unlink()
            except OSError:
                pass
            return
        page_id = ""
        if isinstance(blob, dict):
            raw = blob.get("page")
            if isinstance(raw, str):
                page_id = raw.strip()
        if not page_id or not self._page_navigator.has(page_id):
            log.warning(
                "lcd_page_request_invalid",
                requested=page_id or "<missing>",
            )
            try:
                LCD_PAGE_REQUEST_PATH.unlink()
            except OSError:
                pass
            return
        # Pop any open modal so a remote tab switch lands at the
        # requested page's root, mirroring the on-LCD tab-tap UX.
        while self._page_navigator.modal_stack:
            try:
                await self._page_navigator.pop_modal(ctx=self._page_context)
            except Exception:  # noqa: BLE001
                break
        try:
            await self._page_navigator.go(page_id, ctx=self._page_context)
        except Exception as exc:  # noqa: BLE001
            log.warning(
                "lcd_page_request_apply_failed",
                page_id=page_id,
                error=str(exc),
            )
        try:
            LCD_PAGE_REQUEST_PATH.unlink()
        except OSError:
            pass
        # If we were in calibrate mode, a remote page set returns the
        # operator to lcd_page so the requested page renders.
        if self._mode == "calibrate":
            self._exit_calibration()
        self._mode = "lcd_page"
        log.info("lcd_page_set_remotely", page_id=page_id)
        if self._page_navigator is not None:
            self._mode = "lcd_page"
            log.info("lcd_calibration_exit_to_page")
        else:
            self._mode = "status"
            log.info("lcd_calibration_exit_to_status")

    async def _poll_state_forever(self) -> None:
        """Refresh self._state at 1 Hz from the agent REST endpoint.

        Uses the shared HTTP client so overlay button handlers can
        reuse it without creating a second TCP connection pool.
        """
        from .constants import POLL_PERIOD_SECONDS

        if self._http is None:
            return
        while not self._stop.is_set():
            try:
                r = await self._http.get(self._api_url, timeout=0.9)
                if r.status_code == 200:
                    data = r.json()
                    if isinstance(data, dict):
                        # Preserve the pairing sub-tree written by the
                        # pair-window secondary poll; the status
                        # endpoint does not carry it and a naive
                        # overwrite would wipe live overlay state.
                        existing_pair = self._state.get("pairing")
                        self._state = _normalize_radio_fields(data)
                        if existing_pair is not None:
                            self._state["pairing"] = existing_pair
            except Exception:
                # Agent may be briefly unreachable during a restart.
                # Keep the last known state.
                pass
            try:
                await asyncio.wait_for(
                    self._stop.wait(), timeout=POLL_PERIOD_SECONDS
                )
            except TimeoutError:
                continue

    def _first_boot_unset(self) -> bool:
        role_block = self._state.get("role") or {}
        if not role_block.get("mesh_capable", False):
            return False
        current = role_block.get("current")
        return current in (None, "", "unset")

    async def run(self) -> int:
        import httpx
        import psutil

        from ados.services.ui.display_conf import read_rotation

        from .service import log

        oled_present = self._probe_device()
        fb_present = self._probe_framebuffer()
        if not oled_present and not fb_present:
            log.warning(
                "no_display_detected",
                msg=(
                    "no SSD1306 / SH1106 OLED on i2c-1 and no bound SPI LCD "
                    "framebuffer (matched by driver name across /dev/fb*), "
                    "exiting cleanly"
                ),
            )
            return 0
        log.info(
            "oled_service_running",
            oled=self._driver_name or None,
            framebuffer=fb_present,
        )
        # When the SPI LCD is bound, the touch chip shows up as an evdev
        # node we can listen on. Translating taps to gestures (in lcd_page
        # mode) or to synthetic button events (in oled_compat mode) gives
        # the operator a way to drive the UI on a board that has no
        # physical buttons.
        if fb_present:
            # Read the configured rotation once so the touch bridge's
            # default identity transform matches what is actually being
            # blitted to the panel. Without this the bridge falls back
            # to rotation=0 even when the panel is mounted at 90 / 180
            # / 270, which lands every tap 12-16 px off-axis on a
            # Waveshare 3.5" RPi LCD A in portrait.
            rotation = read_rotation()
            self._verify_calibration_rotation(rotation)
            self._touch_bridge = TouchInputBridge(
                self._bus, rotation=rotation,
            )
            # Decide whether the framebuffer can host the page UI.
            # The bootstrap may flip _mode to "calibrate" or
            # "lcd_page". Touch bridge mode follows.
            self._bootstrap_page_system()
            if self._mode in ("lcd_page", "calibrate"):
                self._touch_bridge.mode = "lcd_page"
            else:
                self._touch_bridge.mode = "oled_compat"
        # Set base_url so pages can use relative paths like
        # ``"/api/wfb"`` without each one having to know the agent's
        # host/port. Without this, httpx raises UnsupportedProtocol on
        # relative-URL requests and pages render blank metrics. The
        # absolute-URL callers in this service (``_api_base/...`` and
        # the mediamtx 9997 calls) are unaffected — httpx ignores
        # ``base_url`` when the request URL is already absolute.
        self._http = httpx.AsyncClient(
            base_url=f"http://{self._api_host}:{self._api_port}",
            timeout=0.9,
        )
        # Hand the http client to the page context so pages can make
        # REST calls without each one constructing its own pool.
        if self._page_context is not None:
            self._page_context.http = self._http
        # Always-on LCD video tap. Construct the LocalVideoTap once at
        # service init and keep it running so the Video page (and any
        # future consumer) can read frames without paying the gst
        # cold-start cost on every navigation. _ensure_video_tap_forever
        # retries start() with the existing 15s cooldown until mediamtx
        # publishes /main; once running, the tap auto-recovers from
        # transient errors via its own bus-error restart loop.
        #
        # The LCD video tap runs on every ground station regardless of
        # RAM. On low-RAM boards (≤ 1500 MB: Pi 4B 1 GB, Pi Zero 2 W,
        # Cubie A7Z) the always-on GStreamer pipeline competes with
        # mediamtx + WebRTC for scheduler time and adds ~130 % CPU; the
        # operator gets a one-time warning so the slowdown is not a
        # mystery. The previous behavior of skipping the tap entirely on
        # low-RAM boards was reverted at user direction — the operator
        # paying for the LCD wants to see the video on it.
        try:
            total_ram_mb = (
                psutil.virtual_memory().total // (1024 * 1024)
            )
        except Exception:  # noqa: BLE001
            total_ram_mb = 0
        video_tap_enabled = True
        if total_ram_mb and total_ram_mb < 1500:
            log.warning(
                "oled_video_tap_low_ram_cpu_warning",
                ram_mb=total_ram_mb,
                msg=(
                    "LCD video tap is running on a memory-constrained "
                    "board. Expect ~130 % CPU and possible status-page "
                    "render hitches. Run on a 2 GB+ SBC for smoother "
                    "operation."
                ),
            )
        # Propagate the gate to the PageContext so the Video page can
        # honor it. Without this, video.py's per-page _ensure_tap
        # fallback path constructs its own LocalVideoTap on every
        # navigation, defeating the run()-level gate.
        self._page_context.video_tap_enabled = video_tap_enabled
        self._page_context.video_tap = self._build_video_tap()
        tasks = [
            asyncio.create_task(self._render_forever(), name="oled_render"),
            asyncio.create_task(self._consume_buttons(), name="oled_buttons"),
            asyncio.create_task(self._poll_state_forever(), name="oled_poll"),
        ]
        if video_tap_enabled:
            tasks.append(
                asyncio.create_task(
                    self._ensure_video_tap_forever(),
                    name="oled_video_tap",
                ),
            )
            tasks.append(
                asyncio.create_task(
                    self._persist_tap_stats_forever(),
                    name="oled_tap_stats_persist",
                ),
            )
        if self._touch_bridge is not None:
            tasks.append(
                asyncio.create_task(self._touch_bridge.run(), name="oled_touch")
            )
            # Gesture consumer dispatches taps/swipes/long-presses to
            # the active page or calibration wizard. Only useful when
            # the bridge is in lcd_page mode; in oled_compat mode the
            # bridge republishes legacy ButtonEvents and this consumer
            # simply sits idle (the bus has no producers).
            self._touch_consumer_task = asyncio.create_task(
                self._consume_touch_gestures(), name="oled_touch_dispatch",
            )
            tasks.append(self._touch_consumer_task)
        try:
            await self._stop.wait()
        finally:
            for t in tasks:
                t.cancel()
            self._stop_pairing_poll()
            if self._touch_bridge is not None:
                self._touch_bridge.request_stop()
            await asyncio.gather(*tasks, return_exceptions=True)
            try:
                if self._http is not None:
                    await self._http.aclose()
            except Exception:
                pass
            try:
                tap = (
                    self._page_context.video_tap
                    if self._page_context is not None
                    else None
                )
                if tap is not None:
                    await tap.stop()
            except Exception:
                pass
            try:
                if self._device is not None:
                    self._device.cleanup()
            except Exception:
                pass
            try:
                if self._fb_renderer is not None:
                    self._fb_renderer.cleanup()
            except Exception:
                pass
        return 0

    def request_stop(self) -> None:
        self._stop.set()
