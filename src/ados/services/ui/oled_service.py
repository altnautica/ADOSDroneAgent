"""OLED status display service.

Drives the 128x64 SSD1306 or SH1106 I2C OLED on the ground-station
front panel. Auto-cycles five status screens (Link, Drone, GCS, Net,
System) at 5-second intervals plus a role badge for mesh-capable
nodes. Reacts to front-panel button events from `ButtonEventBus` for
manual screen advance, menu entry, and mesh overlay screens.

Design choices:
- luma.oled probes the I2C bus at 0x3C then 0x3D. Both SSD1306 and
  SH1106 are tried on each address. If none bind, the service logs
  a clear warning and exits 0 so the systemd unit can stay inactive
  on hardware without an OLED.
- Status state is refreshed at 1 Hz by polling
  `/api/v1/ground-station/status`. While the pairing accept overlay
  is live a secondary 2 Hz poll hits `/pair/pending` so the pending
  list feels responsive.
- Button mapping on status cycle:
    B1 short: previous screen
    B2 short: next screen
    B3 short: enter menu
    B4 short: back to auto-cycle (no-op on status)
- Button mapping on menu:
    B1 short: selection up
    B2 short: selection down
    B3 short: enter, confirm, or open overlay
    B4 short: back out one level (top level returns to status)
- Button mapping on overlay: per-screen BUTTON_ACTIONS dispatch.
  Unmapped button falls through to B4-exits default.
- Burn-in protection: contrast auto-dim to 40 after 60 seconds idle,
  pixel-invert toggled every 10 minutes. The invert clock resets on
  every button press so the operator always sees the natural
  orientation right after they interact.

Manual bench soak for the 10-minute pixel-invert cycle:
  1. Boot the ground-station SBC with an OLED attached.
  2. `journalctl -u ados-oled.service -f` to watch state changes.
  3. Do not press any front-panel button.
  4. After ~10 minutes the display should swap fg/bg colors.
  5. After another ~10 minutes it should swap back.
  6. Press any button. The display should return to natural
     orientation immediately and the 10-min clock should reset.
  7. To shorten the soak for a quick test, temporarily lower
     `INVERT_PERIOD_SECONDS` to e.g. 60 in this file.
"""

from __future__ import annotations

import asyncio
import json
import signal
import sys
import time
from pathlib import Path
from typing import Any

import httpx
from PIL import Image, ImageDraw

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.core.paths import LCD_PAGE_REQUEST_PATH, TOUCH_CALIB_PATH
from ados.services.ui.chrome import bottom_tab_bar, top_status_bar
from ados.services.ui.display_conf import read_rotation
from ados.services.ui.events import ButtonEventBus
from ados.services.ui.pages import PageContext, PageNavigator
from ados.services.ui.pages.dashboard import DashboardPage
from ados.services.ui.renderers import Renderer
from ados.services.ui.renderers.framebuffer import FrameBufferRenderer
from ados.services.ui.theme import current_palette
from ados.services.ui.touch.calibrate import CalibrationWizard
from ados.services.ui.touch.events import TouchGesture
from ados.services.ui.touch.recent import record_touch
from ados.services.ui.touch.session import get_session_registry
from ados.services.ui.touch.transform import load as load_calib
from ados.services.ui.touch_input import TouchInputBridge

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
from ados.services.ui.screens import (
    drone as screen_drone,
)
from ados.services.ui.screens import (
    gcs as screen_gcs,
)
from ados.services.ui.screens import (
    link as screen_link,
)
from ados.services.ui.screens import (
    menu as screen_menu,
)
from ados.services.ui.screens import (
    net as screen_net,
)
from ados.services.ui.screens import (
    system as screen_system,
)
from ados.services.ui.screens.mesh import (
    accept_window as screen_mesh_accept_window,
)
from ados.services.ui.screens.mesh import (
    error_states as screen_mesh_error_states,
)
from ados.services.ui.screens.mesh import (
    hub_unreachable as screen_mesh_hub_unreachable,
)
from ados.services.ui.screens.mesh import (
    join_request_inflight as screen_mesh_join_request_inflight,
)
from ados.services.ui.screens.mesh import (
    join_scan as screen_mesh_join_scan,
)
from ados.services.ui.screens.mesh import (
    joined_status as screen_mesh_joined_status,
)
from ados.services.ui.screens.mesh import (
    leave_confirm as screen_mesh_leave_confirm,
)
from ados.services.ui.screens.mesh import (
    mesh_unavailable as screen_mesh_unavailable,
)
from ados.services.ui.screens.mesh import (
    neighbors as screen_mesh_neighbors,
)
from ados.services.ui.screens.mesh import (
    role_picker as screen_mesh_role_picker,
)
from ados.services.ui.screens.mesh import (
    unset_boot as screen_mesh_unset_boot,
)

# The `unset_boot` module is additionally reached via OVERLAY_SCREENS
# below; keeping the direct import for the first-boot render path lets
# the render loop skip a dict lookup on every tick.

log = get_logger("ui.oled_service")

# Button BCM pins, matching `button_service.py`.
B1, B2, B3, B4 = 5, 6, 13, 19

# Auto-cycle period and idle behavior (seconds).
AUTO_CYCLE_SECONDS = 5.0
IDLE_DIM_SECONDS = 60.0
INVERT_PERIOD_SECONDS = 600.0

# Brightness as luma.oled contrast values.
CONTRAST_ACTIVE = 80
CONTRAST_DIM = 40

# Display geometry.
WIDTH = 128
HEIGHT = 64

# Polling cadence for agent state.
POLL_PERIOD_SECONDS = 1.0

# Screen registry keyed by screen id so the active list can be rebuilt
# from `ground_station.ui.screens` config. REST schema uses the keys
# `home`, `link`, `drone`, `network`, `system`, `qr`. We map older ids
# onto the current REST schema (`net` -> `network`, no separate `home`
# or `qr` renderer yet) so an unknown screen id is silently skipped
# instead of crashing.
SCREEN_RENDERERS: dict[str, Any] = {
    "link": screen_link,
    "drone": screen_drone,
    "gcs": screen_gcs,
    "net": screen_net,
    "network": screen_net,
    "system": screen_system,
}

DEFAULT_SCREEN_ORDER: list[str] = ["link", "drone", "gcs", "net", "system"]
DEFAULT_SCREEN_ENABLED: list[str] = ["link", "drone", "gcs", "net", "system"]

# Overlay screen registry. Each module exports render() plus optional
# BUTTON_ACTIONS, initial_state(service), on_enter(service), and
# on_exit(service). The service dispatches button presses to
# BUTTON_ACTIONS while the overlay is active. B4 in an overlay always
# exits unless the module maps B4 to a different action.
OVERLAY_SCREENS: dict[str, Any] = {
    "unset_boot": screen_mesh_unset_boot,
    "role_picker": screen_mesh_role_picker,
    "accept_window": screen_mesh_accept_window,
    "join_scan": screen_mesh_join_scan,
    "join_request_inflight": screen_mesh_join_request_inflight,
    "joined_status": screen_mesh_joined_status,
    "hub_unreachable": screen_mesh_hub_unreachable,
    "mesh_unavailable": screen_mesh_unavailable,
    "neighbors": screen_mesh_neighbors,
    "leave_confirm": screen_mesh_leave_confirm,
    "error_states": screen_mesh_error_states,
}

# Secondary poll cadence for pending-relay list while the Accept-window
# overlay is live. Faster than the main status poll so the operator sees
# incoming requests with minimal latency.
PAIRING_POLL_SECONDS = 0.5

# Menu tree from the physical UI spec.
# Each node is {label, children, optional: visibility, screen}. Leaves
# with a `screen` key open the named overlay. Leaves without one are
# logged as `menu_action_stub`. `visibility` is an optional callable
# `state -> bool`; when present, the node is hidden if the callable
# returns False. The filter runs against the live agent state snapshot.
MENU_TREE: list[dict[str, Any]] = [
    {"label": "Pair with drone", "children": []},
    {
        "label": "Network",
        "children": [
            {"label": "WiFi AP on/off", "children": []},
            {"label": "WiFi client scan", "children": []},
            {"label": "4G modem status", "children": []},
            {"label": "Uplink priority", "children": []},
        ],
    },
    {
        "label": "Mesh",
        # Mesh menu is always visible so operators on drone-profile or
        # direct-role nodes can see that the feature exists. When the
        # node is not mesh_capable, the submenu collapses to a single
        # hint item explaining how to enable it. This avoids a silent
        # "nothing happens when I open Mesh" failure mode.
        "children": [
            {
                "label": "Mesh unavailable",
                "children": [],
                "screen": "mesh_unavailable",
                "visibility": lambda st: not bool(
                    (st.get("role") or {}).get("mesh_capable")
                ),
            },
            {
                "label": "Set role",
                "children": [],
                "screen": "role_picker",
                "visibility": lambda st: bool(
                    (st.get("role") or {}).get("mesh_capable")
                ),
            },
            {
                "label": "Accept relay",
                "children": [],
                "screen": "accept_window",
                "visibility": lambda st: (
                    bool((st.get("role") or {}).get("mesh_capable"))
                    and (st.get("role") or {}).get("current") == "receiver"
                ),
            },
            {
                "label": "Join mesh",
                "children": [],
                "screen": "join_scan",
                "visibility": lambda st: (
                    bool((st.get("role") or {}).get("mesh_capable"))
                    and (st.get("role") or {}).get("current") == "relay"
                    and not (st.get("mesh") or {}).get("up")
                ),
            },
            {
                "label": "Neighbors",
                "children": [],
                "screen": "neighbors",
                "visibility": lambda st: (
                    bool((st.get("role") or {}).get("mesh_capable"))
                    and (st.get("role") or {}).get("current") in ("relay", "receiver")
                ),
            },
            {
                "label": "Leave mesh",
                "children": [],
                "screen": "leave_confirm",
                "visibility": lambda st: (
                    bool((st.get("role") or {}).get("mesh_capable"))
                    and (st.get("mesh") or {}).get("up", False)
                ),
            },
        ],
    },
    {
        "label": "Radio",
        "children": [
            {"label": "Channel", "children": []},
            {"label": "TX power (n/a)", "children": []},
            {"label": "Bitrate profile", "children": []},
        ],
    },
    {
        "label": "Display",
        "children": [
            {"label": "HDMI resolution", "children": []},
            {"label": "OLED brightness", "children": []},
        ],
    },
    {
        "label": "System",
        "children": [
            {"label": "Version", "children": []},
            {"label": "Reboot", "children": []},
            {"label": "Factory reset", "children": []},
        ],
    },
    {"label": "Back to status", "children": []},
]


def _filter_visible(items: list[dict[str, Any]], state: dict) -> list[dict[str, Any]]:
    """Drop items whose `visibility` callable returns False.

    Nodes without a visibility callable are always visible. Errors in
    the callable are treated as "hide" so a broken predicate does not
    crash the menu.
    """
    out: list[dict[str, Any]] = []
    for node in items:
        vis = node.get("visibility")
        if vis is None:
            out.append(node)
            continue
        try:
            if vis(state):
                out.append(node)
        except Exception:
            pass
    return out


def _now() -> float:
    return time.monotonic()


def _normalize_radio_fields(data: dict[str, Any]) -> dict[str, Any]:
    """Backfill ``link.tx_power_dbm`` and ``radio.topology`` defaults.

    The dashboard tile + OLED link screen both reach into
    ``state["link"]["tx_power_dbm"]`` and
    ``state["radio"]["topology"]``. Older agent builds (or any build
    that hasn't taken the WFB-status REST exposure yet) won't carry
    these keys; left missing, the renderers paint ``--`` placeholders.
    Filling defensively here means downstream code can rely on a
    stable shape regardless of which side ships first.

    Mutates and returns the same dict for caller convenience. The
    transformation is shallow and additive — we never overwrite a
    value the agent already supplied.
    """
    link = data.get("link")
    if isinstance(link, dict):
        link.setdefault("tx_power_dbm", None)
    radio = data.get("radio")
    if not isinstance(radio, dict):
        radio = {}
        data["radio"] = radio
    radio.setdefault("topology", "host_vbus")
    return data


class OledService:
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
        # Optional secondary render target. Bound when /dev/fb1 carries a
        # supported SPI LCD (e.g. ILI9486 via fbtft on Cubie A7Z + Rock
        # 5C). Stays None on stock Pi 4B benches that only have the I2C
        # OLED. When bound, the render loop paints the same screens onto
        # this surface in addition to (or instead of) the OLED.
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

    def _probe_device(self) -> bool:
        """Try SSD1306 then SH1106 at 0x3C and 0x3D. Return True on bind."""
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

    def _paint_active_screen(self, draw: Any, width: int, height: int) -> None:
        """Dispatch the active mode/screen onto a PIL ImageDraw canvas.

        Centralizes the screen-selection logic so the OLED render loop
        and the framebuffer render loop call the same code.
        """
        if self._mode == "overlay" and self._overlay_module is not None:
            overlay_state = {
                **self._state,
                "_overlay_state": self._overlay_state,
            }
            self._overlay_module.render(draw, width, height, overlay_state)
        elif self._mode == "unset":
            screen_mesh_unset_boot.render(draw, width, height, self._state)
        elif self._mode == "status" and self._active_screens:
            _, module = self._active_screens[self._screen_idx]
            module.render(draw, width, height, self._state)
            self._render_role_badge(draw)
        elif self._mode == "menu":
            screen_menu.render(
                draw,
                width,
                height,
                {
                    "items": [n.get("label", "") for n in self._menu_items],
                    "selected": self._menu_sel,
                    "depth": len(self._menu_stack),
                },
            )

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

    # ── lcd_page mode helpers ──────────────────────────────────

    def _bootstrap_page_system(self) -> None:
        """Construct the navigator, register pages, and pick initial mode.

        Called once during ``run()`` after the framebuffer probe
        succeeds and the framebuffer reports a geometry the page UI
        can host. Triggers the calibration wizard when the touch
        chip is present and no calibration file exists yet.
        """
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

    def _read_hostname(self) -> str:
        try:
            return Path("/etc/hostname").read_text().strip() or "groundnode"
        except OSError:
            return "groundnode"

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

    async def _render_lcd_page(self) -> None:
        """Paint chrome + active page onto the framebuffer.

        Async because the page protocol's :meth:`render` is async.
        Called from the async ``_render_forever`` loop directly.
        """
        if self._fb_renderer is None or self._page_navigator is None:
            return
        palette = current_palette()
        # Refresh context state every tick — palette flip and state
        # poll updates need to reach the page without restart.
        if self._page_context is not None:
            self._page_context.palette = palette
            self._page_context.state = self._state
        canvas = Image.new("RGB", (480, 320), palette.bg_primary)
        # Top status bar.
        top_status_bar.draw(
            canvas,
            0,
            0,
            480,
            palette=palette,
            hostname=self._page_context.hostname if self._page_context else "groundnode",
            state=self._state,
        )
        # Active page paints into the 480x244 region just below the
        # 32 px chrome. Modal stack is rendered on top by the
        # current_page() resolution.
        page = self._page_navigator.current_page()
        page_img: Image.Image | None = None
        try:
            page_img = await page.render(self._page_context)  # type: ignore[arg-type]
        except Exception as exc:  # noqa: BLE001
            log.warning("page_render_failed", page_id=page.id, error=str(exc))
        if page_img is not None:
            canvas.paste(page_img, (0, 32))
        # Bottom tab bar with any active feedback flashes.
        bottom_tab_bar.draw(
            canvas,
            0,
            320 - 44,
            480,
            palette=palette,
            active=self._page_navigator.active_page_id,
            tapped_at_ms=self._page_navigator.tap_feedback(),
        )
        try:
            self._fb_renderer.present(canvas)
        except Exception as exc:  # noqa: BLE001
            log.warning("framebuffer_present_failed", error=str(exc))

    async def _maybe_tear_down_video_tap(self) -> None:
        """Tear down the video page's local tap after the inactivity grace.

        The video page can't observe its own absence; the render loop
        drives the timer based on whether the active page is the
        ``video`` page. We call ``maybe_teardown_idle_tap`` on the
        registered VideoPage instance whenever the active route is
        elsewhere — the helper short-circuits when the inactivity
        threshold hasn't elapsed yet, so the cost is one method call
        per tick when the operator is on a different tab.
        """
        if (
            self._page_navigator is None
            or self._page_context is None
            or self._page_navigator.active_page_id == "video"
        ):
            return
        video_page = self._page_navigator.page("video")
        if video_page is None:
            return
        helper = getattr(video_page, "maybe_teardown_idle_tap", None)
        if helper is None:
            return
        try:
            await helper(self._page_context)
        except Exception as exc:  # noqa: BLE001
            log.debug("video_tap_idle_teardown_failed", error=str(exc))

    def _render_calibration(self) -> None:
        """Paint the active calibration wizard (or failure card)."""
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
        if self._page_navigator is None:
            return
        # Re-derive zones using the same layout the renderer uses.
        # We recompute rather than caching them so a different chrome
        # height in the future does not get a stale dispatch.
        from ados.services.ui.chrome.bottom_tab_bar import (
            HEIGHT as TAB_HEIGHT,
        )
        from ados.services.ui.chrome.bottom_tab_bar import (
            TAB_COUNT,
            TAB_WIDTH,
            page_id_for_zone,
        )
        if gesture.start_y < 320 - TAB_HEIGHT:
            return
        index = max(0, min(TAB_COUNT - 1, gesture.start_x // TAB_WIDTH))
        # The zone ids are stable; rebuild via the helper map. Must
        # match the tuple in chrome.bottom_tab_bar._TABS — the fourth
        # tab now routes to LinkStatsPage; the old MorePage class is
        # left in tree but unregistered.
        zone_ids = (
            "tab.dashboard",
            "tab.video",
            "tab.settings",
            "tab.link_stats",
        )
        zone_id = zone_ids[index]
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

    async def _maybe_apply_page_request(self) -> None:
        """Honor a remote ``POST /api/v1/display/page`` request.

        Reads ``/run/ados/lcd-page-request.json``, routes the
        navigator to the requested page, and unlinks the file so the
        same request can't reapply on the next tick. Best-effort:
        any malformed payload is silently dropped and the file is
        unlinked so the watcher doesn't loop on it.
        """
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

    def _set_contrast(self, value: int) -> None:
        if self._device is None:
            return
        try:
            self._device.contrast(value)
        except Exception as exc:
            log.debug("contrast_failed", error=str(exc))

    def _set_invert(self, on: bool) -> None:
        if self._device is None:
            return
        try:
            # luma.oled devices expose `.invert(bool)` on most drivers.
            invert_fn = getattr(self._device, "invert", None)
            if callable(invert_fn):
                invert_fn(on)
                self._inverted = on
        except Exception as exc:
            log.debug("invert_failed", error=str(exc))

    def _reload_ui_config(self) -> None:
        """Rebuild active screen list, brightness, and cycle period from config.

        Called once at construction and again whenever SIGHUP fires. Tolerates
        a missing `ground_station.ui` block by falling back to defaults. Never
        raises: if config load itself blows up we keep the prior state.
        """
        try:
            cfg = load_config()
            ui = getattr(getattr(cfg, "ground_station", None), "ui", None)
            screens_cfg = getattr(ui, "screens", {}) if ui is not None else {}
            oled_cfg = getattr(ui, "oled", {}) if ui is not None else {}
        except Exception as exc:
            log.warning("ui_config_reload_failed", error=str(exc))
            return

        order = screens_cfg.get("order") if isinstance(screens_cfg, dict) else None
        enabled = screens_cfg.get("enabled") if isinstance(screens_cfg, dict) else None
        if not isinstance(order, list) or not order:
            order = list(DEFAULT_SCREEN_ORDER)
        if not isinstance(enabled, list) or not enabled:
            enabled = list(DEFAULT_SCREEN_ENABLED)

        enabled_set = set(enabled)
        active: list[tuple[str, Any]] = []
        for sid in order:
            if not isinstance(sid, str):
                continue
            if sid not in enabled_set:
                continue
            renderer = SCREEN_RENDERERS.get(sid)
            if renderer is None:
                continue
            active.append((sid, renderer))
        if not active:
            # Empty active set is unusable. Fall back to defaults so the
            # operator always has something on screen.
            active = [(sid, SCREEN_RENDERERS[sid]) for sid in DEFAULT_SCREEN_ORDER]

        self._active_screens = active
        # Clamp screen_idx in case the list shrank under us.
        if self._screen_idx >= len(self._active_screens):
            self._screen_idx = 0

        if isinstance(oled_cfg, dict):
            cycle = oled_cfg.get("screen_cycle_seconds")
            if isinstance(cycle, (int, float)) and cycle > 0:
                self._cycle_seconds = float(cycle)
            auto_dim = oled_cfg.get("auto_dim_enabled")
            if isinstance(auto_dim, bool):
                self._auto_dim_enabled = auto_dim
            brightness = oled_cfg.get("brightness")
            if isinstance(brightness, int) and 0 <= brightness <= 255:
                self._brightness_active = brightness
                if not self._dimmed:
                    self._set_contrast(brightness)

        log.info(
            "oled_ui_config_reloaded",
            screens=[sid for sid, _ in self._active_screens],
            cycle_s=self._cycle_seconds,
            auto_dim=self._auto_dim_enabled,
            brightness=self._brightness_active,
        )

    def request_reload(self) -> None:
        """SIGHUP entry point. Set a flag the render loop will pick up."""
        self._reload_requested = True

    # ── Overlay lifecycle ───────────────────────────────────────

    def _enter_overlay(
        self,
        screen_id: str,
        initial_state: dict[str, Any] | None = None,
    ) -> None:
        """Switch the display to a mesh overlay screen.

        Fires the previous overlay's `on_exit` before swapping in the
        new module. Nested transitions (e.g. accept_window -> error on
        REST failure) stop their background tasks cleanly instead of
        leaking them.
        """
        module = OVERLAY_SCREENS.get(screen_id)
        if module is None:
            log.warning("overlay_unknown", screen_id=screen_id)
            return
        prev_id = self._overlay_id
        prev_module = self._overlay_module
        # Call the outgoing overlay's on_exit BEFORE we replace state,
        # so it still sees its own overlay_id/state while cleaning up.
        if prev_module is not None:
            on_exit = getattr(prev_module, "on_exit", None)
            if callable(on_exit):
                try:
                    asyncio.create_task(on_exit(self))
                except Exception as exc:
                    log.debug("overlay_on_exit_failed", screen=prev_id, error=str(exc))
        self._overlay_id = screen_id
        self._overlay_module = module
        # Module-provided initial state takes precedence over caller-
        # supplied overrides so screens can compute from live state.
        if hasattr(module, "initial_state"):
            try:
                base = module.initial_state(self)
            except Exception as exc:
                log.debug("overlay_initial_state_failed", screen=screen_id, error=str(exc))
                base = {}
        else:
            base = {}
        if initial_state:
            base.update(initial_state)
        self._overlay_state = base
        self._mode = "overlay"
        log.info("overlay_entered", screen_id=screen_id, previous=prev_id)
        # Screen-level on_enter hook (e.g. the accept_window overlay
        # opens the pairing window via REST when the operator enters).
        on_enter = getattr(module, "on_enter", None)
        if callable(on_enter):
            try:
                asyncio.create_task(on_enter(self))
            except Exception as exc:
                log.debug("overlay_on_enter_failed", screen=screen_id, error=str(exc))

    def _exit_overlay(self) -> None:
        prev_id = self._overlay_id
        prev_module = self._overlay_module
        self._overlay_id = None
        self._overlay_module = None
        self._overlay_state = {}
        self._mode = "status"
        log.info("overlay_exited", screen_id=prev_id)
        on_exit = getattr(prev_module, "on_exit", None) if prev_module else None
        if callable(on_exit):
            try:
                asyncio.create_task(on_exit(self))
            except Exception as exc:
                log.debug("overlay_on_exit_failed", screen=prev_id, error=str(exc))

    def _start_pairing_poll(self) -> None:
        if self._pairing_poll_task is not None and not self._pairing_poll_task.done():
            return
        self._pairing_poll_task = asyncio.create_task(
            self._poll_pairing_forever(), name="oled_pairing_poll"
        )

    def _stop_pairing_poll(self) -> None:
        task = self._pairing_poll_task
        if task is not None and not task.done():
            task.cancel()
        self._pairing_poll_task = None

    async def _poll_pairing_forever(self) -> None:
        """Refresh pairing snapshot at 2 Hz while the accept overlay is live."""
        if self._http is None:
            return
        while not self._stop.is_set() and self._overlay_id == "accept_window":
            try:
                r = await self._http.get(f"{self._api_base}/pair/pending", timeout=0.9)
                if r.status_code == 200:
                    data = r.json()
                    if isinstance(data, dict):
                        self._state.setdefault("pairing", {})["window"] = {
                            "open": data.get("open", False),
                            "opened_at_ms": data.get("opened_at_ms"),
                            "closes_at_ms": data.get("closes_at_ms"),
                        }
                        self._state["pairing"]["pending"] = data.get("pending") or []
            except Exception:
                pass
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=PAIRING_POLL_SECONDS)
            except TimeoutError:
                continue

    async def _poll_state_forever(self) -> None:
        """Refresh self._state at 1 Hz from the agent REST endpoint.

        Uses the shared HTTP client so overlay button handlers can
        reuse it without creating a second TCP connection pool.
        """
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

    async def _consume_buttons(self) -> None:
        """Drain the button bus and update UI state."""
        async for ev in self._bus.subscribe():
            if self._stop.is_set():
                return
            self._last_button_ts = _now()
            # Wake from dim on any press.
            if self._dimmed:
                self._set_contrast(self._brightness_active)
                self._dimmed = False
            # Pixel-invert burn-in protection. On any button press,
            # return the display to natural orientation and reset the
            # invert clock so the user always sees the non-inverted
            # view right after they interact.
            if self._inverted:
                self._set_invert(False)
            self._last_invert_ts = _now()
            if ev.kind != "short":
                # Long-press hooks reach the system-level handler
                # regardless of overlay. One example is factory reset
                # on long B4. Trace for bench visibility.
                if self._mode == "overlay":
                    log.info("oled_long_press_during_overlay", button=ev.button)
                else:
                    log.info("oled_long_press_passthrough", button=ev.button)
                continue
            if self._mode == "overlay":
                await self._handle_overlay_press(ev.button)
            elif self._mode == "unset":
                # Any press moves operator into the Mesh -> Set role menu.
                self._mode = "menu"
                self._menu_stack = []
                self._menu_items = _filter_visible(MENU_TREE, self._state)
                self._menu_sel = 0
                # Auto-enter Mesh submenu if present.
                for i, node in enumerate(self._menu_items):
                    if node.get("label") == "Mesh":
                        self._menu_stack.append((self._menu_items, i))
                        self._menu_items = _filter_visible(
                            node.get("children") or [], self._state
                        )
                        self._menu_sel = 0
                        # Position the cursor on "Set role" explicitly
                        # so the next B3 always drives the role picker
                        # even if a future change adds earlier items.
                        for j, child in enumerate(self._menu_items):
                            if child.get("label") == "Set role":
                                self._menu_sel = j
                                break
                        break
            elif self._mode == "status":
                self._handle_status_press(ev.button)
            else:
                self._handle_menu_press(ev.button)

    async def _handle_overlay_press(self, button: int) -> None:
        module = self._overlay_module
        if module is None:
            self._exit_overlay()
            return
        actions = getattr(module, "BUTTON_ACTIONS", None) or {}
        handler = actions.get(button)
        if handler is None:
            # Unmapped button in overlay: B4 always exits as a safe default.
            if button == B4:
                self._exit_overlay()
            return
        try:
            await handler(self)
        except Exception as exc:
            log.warning(
                "overlay_action_failed",
                screen_id=self._overlay_id,
                button=button,
                error=str(exc),
            )

    def _handle_status_press(self, button: int) -> None:
        n = max(1, len(self._active_screens))
        if button == B1:
            self._screen_idx = (self._screen_idx - 1) % n
        elif button == B2:
            self._screen_idx = (self._screen_idx + 1) % n
        elif button == B3:
            self._mode = "menu"
            self._menu_stack = []
            self._menu_items = _filter_visible(MENU_TREE, self._state)
            self._menu_sel = 0
        elif button == B4:
            # No-op on status auto-cycle. Stay put.
            pass

    def _handle_menu_press(self, button: int) -> None:
        if not self._menu_items:
            # Empty after filtering; back out.
            if self._menu_stack:
                parent_items, parent_sel = self._menu_stack.pop()
                self._menu_items = _filter_visible(parent_items, self._state)
                self._menu_sel = min(parent_sel, max(0, len(self._menu_items) - 1))
            else:
                self._mode = "status"
            return
        if button == B1:
            self._menu_sel = (self._menu_sel - 1) % len(self._menu_items)
        elif button == B2:
            self._menu_sel = (self._menu_sel + 1) % len(self._menu_items)
        elif button == B3:
            current = self._menu_items[self._menu_sel]
            screen_id = current.get("screen")
            children = current.get("children") or []
            if current.get("label") == "Back to status":
                self._mode = "status"
                return
            if screen_id:
                # Menu leaf drives an overlay screen.
                self._enter_overlay(screen_id)
                return
            if children:
                self._menu_stack.append((self._menu_items, self._menu_sel))
                self._menu_items = _filter_visible(children, self._state)
                self._menu_sel = 0
            else:
                path = [
                    (items[idx].get("label", "") if idx < len(items) else "")
                    for (items, idx) in self._menu_stack
                ]
                path.append(current.get("label", ""))
                log.info(
                    "menu_action_stub",
                    label=current.get("label"),
                    path=path,
                )
        elif button == B4:
            if self._menu_stack:
                parent_items, parent_sel = self._menu_stack.pop()
                self._menu_items = _filter_visible(parent_items, self._state)
                self._menu_sel = min(parent_sel, max(0, len(self._menu_items) - 1))
            else:
                self._mode = "status"

    def _render_role_badge(self, draw: Any) -> None:
        """Draw a compact role indicator at the top-right of the status cycle.

        Kept tight (3-char role tag, optional 3-char mesh_id suffix) so
        it fits in the ~30 px strip beyond where status screens like
        `link.py` draw channel text at x=88. The badge renders at or
        past x=94 to avoid overlap.
        """
        role_block = self._state.get("role") or {}
        role = role_block.get("current")
        mesh_capable = role_block.get("mesh_capable", False)
        if not mesh_capable:
            return
        mesh_block = self._state.get("mesh") or {}
        if role == "receiver":
            mesh_id = str(mesh_block.get("mesh_id") or "")[:3]
            label = f"Rx{mesh_id}" if mesh_id else "Rx"
        elif role == "relay":
            label = "Rly"
        elif role == "direct":
            label = "Dir"
        else:
            label = "?"
        label = label[:5]
        # Right-anchor at WIDTH with a 6px per glyph approximation and
        # a minimum left bound of 94 to stay clear of channel text.
        approx_px = len(label) * 6
        x = max(94, WIDTH - approx_px - 2)
        draw.text((x, 0), label, fill="white")

    def _first_boot_unset(self) -> bool:
        role_block = self._state.get("role") or {}
        if not role_block.get("mesh_capable", False):
            return False
        current = role_block.get("current")
        return current in (None, "", "unset")

    async def _render_forever(self) -> None:
        """Main draw loop. Advances status screens every AUTO_CYCLE_SECONDS."""
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

            # Refresh cadence: pages declare a preferred Hz; carousel
            # stays at the historical 5 Hz.
            tick_period = 0.2
            if self._mode == "lcd_page" and self._page_navigator is not None:
                page = self._page_navigator.current_page()
                hz = float(getattr(page, "refresh_hz", 5.0) or 5.0)
                if hz > 0:
                    tick_period = max(0.02, 1.0 / hz)
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=tick_period)
            except TimeoutError:
                continue

    async def run(self) -> int:
        oled_present = self._probe_device()
        fb_present = self._probe_framebuffer()
        if not oled_present and not fb_present:
            log.warning(
                "no_display_detected",
                msg=(
                    "no SSD1306 / SH1106 OLED on i2c-1 and no SPI LCD "
                    "framebuffer at /dev/fb1, exiting cleanly"
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
        tasks = [
            asyncio.create_task(self._render_forever(), name="oled_render"),
            asyncio.create_task(self._consume_buttons(), name="oled_buttons"),
            asyncio.create_task(self._poll_state_forever(), name="oled_poll"),
        ]
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


async def _amain() -> int:
    cfg = load_config()
    configure_logging(cfg.logging.level)
    api_cfg = cfg.scripting.rest_api
    bus = ButtonEventBus()
    service = OledService(
        bus=bus,
        api_host="127.0.0.1",
        api_port=api_cfg.port,
    )

    loop = asyncio.get_event_loop()

    def _on_signal(*_a: Any) -> None:
        log.info("oled_service_signal_stop")
        service.request_stop()

    for sig in (signal.SIGTERM, signal.SIGINT):
        try:
            loop.add_signal_handler(sig, _on_signal)
        except NotImplementedError:
            signal.signal(sig, _on_signal)

    def _on_sighup(*_a: Any) -> None:
        log.info("oled_service_signal_reload")
        service.request_reload()

    try:
        loop.add_signal_handler(signal.SIGHUP, _on_sighup)
    except (NotImplementedError, AttributeError):
        try:
            signal.signal(signal.SIGHUP, _on_sighup)
        except (AttributeError, ValueError):
            pass

    rc = await service.run()
    await bus.close()
    return rc


def main() -> None:
    try:
        rc = asyncio.run(_amain())
    except KeyboardInterrupt:
        rc = 0
    sys.exit(rc)


if __name__ == "__main__":
    main()
