"""Touch-input bridge for the SPI LCD's resistive touchscreen.

The kernel ``ads7846`` driver exposes an evdev device once the LCD
overlay binds the touch chip. This module:

1. Auto-discovers that evdev node by scanning ``/dev/input/event*`` for
   one whose ``EV_KEY`` capabilities include ``BTN_TOUCH``.
2. Reads pen-down events (``BTN_TOUCH`` going from 0 to 1).
3. Republishes each pen-down as a synthetic ``ButtonEvent`` on the
   shared :class:`ButtonEventBus` so the OLED service's existing button
   consumer advances screens or enters menus without any new dispatch
   logic.

Phase-1 mapping is the simplest useful one: any single tap acts like
the ``B2 short`` event (``next_screen`` action in status mode, ``down``
in menu mode). That's enough to give an operator on a button-less SBC
(Cubie A7Z, Rock 5C) a way to walk the screen cycle without touching
SSH. Multi-zone tap regions and long-press handling are deferred to a
follow-up plan that reuses this bridge as the wire-level read path.

The bridge tolerates the touch device disappearing (e.g., after a USB
re-enumeration of an attached UVC camera that bumps the input device
numbering) by re-running the probe loop with backoff.
"""

from __future__ import annotations

import asyncio
import time
from pathlib import Path
from typing import TYPE_CHECKING

from ados.core.logging import get_logger

if TYPE_CHECKING:  # pragma: no cover - typing-only import
    from .events import ButtonEventBus


log = get_logger("ui.touch")


# Synthetic button identifiers. These don't have to match a physical
# GPIO; ButtonEvent.button is just an int label that downstream
# consumers route through their action map. The OLED service treats
# button 6 as "B2", which maps to "next_screen" / "down" in
# DEFAULT_BUTTON_ACTIONS.
SYNTHETIC_TAP_BUTTON = 6

# How long to wait between probe retries when the evdev node is absent
# at startup or disappears mid-run.
PROBE_BACKOFF_SECONDS = 2.0

# Suppress accidental double-taps tighter than this. The ADS7846
# driver fires multiple events per single physical contact when the
# pen is dragged; we only want one per pen-down.
DEBOUNCE_SECONDS = 0.25


class TouchInputBridge:
    """Listen on the ADS7846 evdev node and republish taps to the bus."""

    def __init__(self, bus: "ButtonEventBus", device_name_hint: str = "ADS7846") -> None:
        self._bus = bus
        self._hint = device_name_hint.lower()
        self._stop = asyncio.Event()
        self._last_tap_ts: float = 0.0

    async def run(self) -> None:
        """Run forever. Cancel via :meth:`request_stop`."""
        try:
            from evdev import InputDevice, categorize, ecodes, list_devices  # noqa: F401
        except ImportError as exc:  # pragma: no cover - dep is in pyproject
            log.warning("evdev_import_failed", error=str(exc))
            return

        from evdev import InputDevice, ecodes, list_devices

        while not self._stop.is_set():
            device = self._find_device(InputDevice, list_devices, ecodes)
            if device is None:
                try:
                    await asyncio.wait_for(
                        self._stop.wait(), timeout=PROBE_BACKOFF_SECONDS
                    )
                except asyncio.TimeoutError:
                    continue
                else:
                    return
            log.info("touch_bound", path=device.path, name=device.name)
            try:
                await self._consume(device, ecodes)
            except OSError as exc:
                # Device went away. Loop back to re-probe.
                log.warning("touch_device_lost", path=device.path, error=str(exc))
                continue

    def request_stop(self) -> None:
        self._stop.set()

    def _find_device(self, InputDevice, list_devices, ecodes):  # noqa: ANN001
        for path in list_devices():
            try:
                dev = InputDevice(path)
            except OSError:
                continue
            name = (dev.name or "").lower()
            caps = dev.capabilities().get(ecodes.EV_KEY, [])
            has_btn_touch = ecodes.BTN_TOUCH in caps
            matches_hint = self._hint in name
            if has_btn_touch and (matches_hint or self._hint == ""):
                return dev
            try:
                dev.close()
            except Exception:  # noqa: BLE001
                pass
        return None

    async def _consume(self, device, ecodes):  # noqa: ANN001
        """Read events from the device until cancelled or it errors."""
        from .events import ButtonEvent

        async for event in device.async_read_loop():
            if self._stop.is_set():
                return
            # We care about EV_KEY BTN_TOUCH transitions to 1 (down).
            if event.type != ecodes.EV_KEY:
                continue
            if event.code != ecodes.BTN_TOUCH:
                continue
            if event.value != 1:
                continue
            now = time.monotonic()
            if now - self._last_tap_ts < DEBOUNCE_SECONDS:
                continue
            self._last_tap_ts = now
            try:
                await self._bus.publish(
                    ButtonEvent(
                        button=SYNTHETIC_TAP_BUTTON,
                        kind="short",
                        action="next_screen",
                        timestamp_ms=int(now * 1000),
                    )
                )
            except Exception as exc:  # noqa: BLE001
                log.debug("touch_publish_failed", error=str(exc))


def _evdev_path_for_name(hint: str) -> Path | None:
    """Best-effort sync helper used by hardware_check probes.

    Returns the first ``/dev/input/event*`` whose driver name contains
    ``hint`` (case-insensitive). Returns None when nothing matches.
    """
    try:
        from evdev import InputDevice, list_devices
    except ImportError:
        return None
    needle = hint.lower()
    for path in list_devices():
        try:
            dev = InputDevice(path)
        except OSError:
            continue
        try:
            if needle in (dev.name or "").lower():
                return Path(path)
        finally:
            try:
                dev.close()
            except Exception:  # noqa: BLE001
                pass
    return None
