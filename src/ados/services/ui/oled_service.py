"""OLED status display service (MSN-025 Wave B).

Drives the 128x64 SSD1306 or SH1106 I2C OLED on the ground-station
front panel. Auto-cycles five status screens (Link, Drone, GCS, Net,
System) at 5-second intervals. Reacts to front-panel button events
from `ButtonEventBus` for manual screen advance and menu entry.

Design choices:
- luma.oled probes the I2C bus at 0x3C (most common) then 0x3D. Both
  SSD1306 and SH1106 are tried on each address. If none bind, the
  service logs a clear warning and exits 0. The systemd unit (wired
  in Wave D) is expected to use `Restart=no` for graceful absence on
  hardware without an OLED. Rule 26: no manual recovery expected.
- Status state is refreshed at 1 Hz by polling the agent REST API
  loopback endpoint `/api/v1/ground-station/status`. Wave C owns the
  endpoint body shape. Until it exists the poll returns empty dict
  and screens render the "--" placeholder.
- Button mapping on status cycle:
    B1 short: previous screen
    B2 short: next screen
    B3 short: enter menu
    B4 short: back to auto-cycle (no-op on status)
- Button mapping on menu:
    B1 short: selection up
    B2 short: selection down
    B3 short: enter or confirm
    B4 short: back out one level (top level returns to status)
- Burn-in protection: contrast auto-dim to 40 after 60 seconds idle,
  pixel-invert toggled every 10 minutes. Phase 4 Wave 2 confirms the
  invert clock resets to "just normal-started" on every button press
  so the operator always sees the natural orientation right after
  interacting.

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

Not in Wave B scope:
- systemd unit files (Wave D).
- REST endpoint `/api/v1/ground-station/status` is read-only; the
  writer lives in Wave C.
- Actual wiring of menu items to agent actions (pair, reboot, etc.)
  stubs through `log.info("menu_action_stub", ...)`.
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

# Screen registry. Phase 4 Wave 2: now a dict keyed by screen id so the
# active list can be rebuilt from `ground_station.ui.screens` config.
# REST schema uses the keys `home`, `link`, `drone`, `network`,
# `system`, `qr`. We map the historical Wave B ids onto the current
# REST schema (`net` -> `network`, no separate `home` or `qr` renderer
# yet) so an unknown screen id is silently skipped instead of crashing.
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

# Menu tree from spec 05-physical-ui-oled-buttons.md.
# Each node is (label, children). Leaf nodes have empty children and
# are logged as menu_action_stub when selected.
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
        self._api_url = f"http://{api_host}:{api_port}/api/v1/ground-station/status"
        self._device = None
        self._driver_name = ""
        self._mode: str = "status"  # "status" or "menu"
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
        # Phase 4 Wave 2: dynamic screen list and OLED prefs are
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

    async def _poll_state_forever(self) -> None:
        """Refresh self._state at 1 Hz from the agent REST endpoint."""
        async with httpx.AsyncClient(timeout=0.9) as client:
            while not self._stop.is_set():
                try:
                    r = await client.get(self._api_url)
                    if r.status_code == 200:
                        data = r.json()
                        if isinstance(data, dict):
                            self._state = data
                except Exception:
                    # Endpoint may not exist yet (Wave C owns it). Stay
                    # quiet and keep the last known state.
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
            # Phase 4 Wave 2: pixel-invert burn-in protection. On any
            # button press, return the display to natural orientation
            # and reset the invert clock so the user always sees the
            # non-inverted view right after they interact. The 10-min
            # invert / 10-min normal cycle restarts from "normal" now.
            if self._inverted:
                self._set_invert(False)
            self._last_invert_ts = _now()
            if ev.kind != "short":
                # Long press hooks live in Wave C (factory reset, pair).
                log.info("oled_long_press_passthrough", button=ev.button)
                continue
            if self._mode == "status":
                self._handle_status_press(ev.button)
            else:
                self._handle_menu_press(ev.button)

    def _handle_status_press(self, button: int) -> None:
        n = max(1, len(self._active_screens))
        if button == B1:
            self._screen_idx = (self._screen_idx - 1) % n
        elif button == B2:
            self._screen_idx = (self._screen_idx + 1) % n
        elif button == B3:
            self._mode = "menu"
            self._menu_stack = []
            self._menu_items = MENU_TREE
            self._menu_sel = 0
        elif button == B4:
            # No-op on status auto-cycle. Stay put.
            pass

    def _handle_menu_press(self, button: int) -> None:
        if button == B1:
            self._menu_sel = (self._menu_sel - 1) % len(self._menu_items)
        elif button == B2:
            self._menu_sel = (self._menu_sel + 1) % len(self._menu_items)
        elif button == B3:
            current = self._menu_items[self._menu_sel]
            children = current.get("children") or []
            if current.get("label") == "Back to status":
                self._mode = "status"
                return
            if children:
                self._menu_stack.append((self._menu_items, self._menu_sel))
                self._menu_items = children
                self._menu_sel = 0
            else:
                path = [items[idx].get("label", "") for (items, idx) in self._menu_stack]
                path.append(current.get("label", ""))
                log.info(
                    "menu_action_stub",
                    label=current.get("label"),
                    path=path,
                )
        elif button == B4:
            if self._menu_stack:
                self._menu_items, self._menu_sel = self._menu_stack.pop()
            else:
                self._mode = "status"

    async def _render_forever(self) -> None:
        """Main draw loop. Advances status screens every AUTO_CYCLE_SECONDS."""
        if self._device is None:
            return
        from luma.core.render import canvas

        last_advance = _now()
        while not self._stop.is_set():
            now = _now()

            # Phase 4 Wave 2: pick up SIGHUP-driven config reloads.
            if self._reload_requested:
                self._reload_requested = False
                self._reload_ui_config()
                last_advance = now

            # Idle auto-dim. Honors the auto_dim_enabled flag from config.
            idle = now - self._last_button_ts
            if (
                self._auto_dim_enabled
                and not self._dimmed
                and idle >= IDLE_DIM_SECONDS
            ):
                self._set_contrast(CONTRAST_DIM)
                self._dimmed = True

            # Periodic pixel invert for burn-in mitigation. The 10-min
            # cycle clock is reset on every button press (see
            # `_consume_buttons`) so the user never sees an inverted
            # screen immediately after interacting.
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
                    if self._mode == "status" and n_screens > 0:
                        _, module = self._active_screens[self._screen_idx]
                        module.render(draw, WIDTH, HEIGHT, self._state)
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
            await asyncio.gather(*tasks, return_exceptions=True)
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
