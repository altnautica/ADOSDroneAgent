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
import signal
import sys
import time
from typing import Any

import httpx

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.services.ui.events import ButtonEventBus
from ados.services.ui.screens import (
    drone as screen_drone,
    gcs as screen_gcs,
    link as screen_link,
    menu as screen_menu,
    net as screen_net,
    system as screen_system,
)
from ados.services.ui.screens.mesh import (
    accept_window as screen_mesh_accept_window,
    error_states as screen_mesh_error_states,
    hub_unreachable as screen_mesh_hub_unreachable,
    join_request_inflight as screen_mesh_join_request_inflight,
    join_scan as screen_mesh_join_scan,
    joined_status as screen_mesh_joined_status,
    leave_confirm as screen_mesh_leave_confirm,
    neighbors as screen_mesh_neighbors,
    role_picker as screen_mesh_role_picker,
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


class OledService:
    """Owns the OLED device, the render loop, and menu state."""

    def __init__(
        self,
        bus: ButtonEventBus,
        api_host: str = "127.0.0.1",
        api_port: int = 8080,
    ) -> None:
        self._bus = bus
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
            except asyncio.TimeoutError:
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
                        self._state = data
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
            except asyncio.TimeoutError:
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
        if self._device is None:
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

            try:
                with canvas(self._device) as draw:
                    if self._mode == "overlay" and self._overlay_module is not None:
                        overlay_state = {
                            **self._state,
                            "_overlay_state": self._overlay_state,
                        }
                        self._overlay_module.render(draw, WIDTH, HEIGHT, overlay_state)
                    elif self._mode == "unset":
                        screen_mesh_unset_boot.render(draw, WIDTH, HEIGHT, self._state)
                    elif self._mode == "status" and n_screens > 0:
                        _, module = self._active_screens[self._screen_idx]
                        module.render(draw, WIDTH, HEIGHT, self._state)
                        self._render_role_badge(draw)
                    elif self._mode == "menu":
                        screen_menu.render(
                            draw,
                            WIDTH,
                            HEIGHT,
                            {
                                "items": [n.get("label", "") for n in self._menu_items],
                                "selected": self._menu_sel,
                                "depth": len(self._menu_stack),
                            },
                        )
            except Exception as exc:
                log.warning("render_failed", error=str(exc))

            try:
                await asyncio.wait_for(self._stop.wait(), timeout=0.2)
            except asyncio.TimeoutError:
                continue

    async def run(self) -> int:
        if not self._probe_device():
            log.warning(
                "oled_not_detected",
                msg="no SSD1306 or SH1106 found at 0x3C or 0x3D, exiting cleanly",
            )
            return 0
        log.info("oled_service_running", driver=self._driver_name)
        self._http = httpx.AsyncClient(timeout=0.9)
        tasks = [
            asyncio.create_task(self._render_forever(), name="oled_render"),
            asyncio.create_task(self._consume_buttons(), name="oled_buttons"),
            asyncio.create_task(self._poll_state_forever(), name="oled_poll"),
        ]
        try:
            await self._stop.wait()
        finally:
            for t in tasks:
                t.cancel()
            self._stop_pairing_poll()
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
