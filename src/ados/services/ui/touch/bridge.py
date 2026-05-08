"""Touch-input bridge: evdev -> rotated/calibrated -> gesture bus.

The bridge replaces the older ``ui.touch_input`` shim. It runs three
event paths in one async loop:

1. Discover the ADS7846 evdev node on ``/dev/input/event*``.
2. Read raw ABS_X / ABS_Y / BTN_TOUCH events while the pen is down.
3. On pen-up, classify the stroke (tap, long_press, swipe, drag) and
   emit a :class:`TouchGesture` on :class:`TouchEventBus`. While the
   pen is down, every accepted move is also published on
   :class:`TouchMoveBus` so a page that wants to draw a live drag
   indicator can subscribe.

For backwards compatibility with the OLED carousel mode, the bridge
also offers ``legacy_button_subscribe()``: when the bridge is in
``oled_compat`` mode it drops the gesture/move buses and instead
republishes a synthetic ``ButtonEvent(button=6, kind="short")`` on
the original :class:`ButtonEventBus` for every tap. The OLED service
flips this mode based on whether it has constructed a PageNavigator.

The classifier thresholds match the field-tuned numbers from the spec:

* ``tap`` — duration < 400 ms AND total displacement < 12 px.
* ``long_press`` — duration >= 400 ms AND total displacement < 12 px.
* ``swipe`` — duration < 250 ms AND displacement >= 24 px.
* ``drag`` — anything else with displacement >= 12 px.

Direction is the cardinal axis (one of up/down/left/right) of the
larger displacement component.
"""

from __future__ import annotations

import asyncio
import time
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal

from ados.core.logging import get_logger
from ados.core.paths import TOUCH_CALIB_PATH

from .events import (
    Direction,
    GestureKind,
    TouchEventBus,
    TouchGesture,
    TouchMove,
    TouchMoveBus,
)
from .transform import Affine, identity_for
from .transform import load as load_calib

if TYPE_CHECKING:  # pragma: no cover
    from ados.services.ui.events import ButtonEventBus


log = get_logger("ui.touch.bridge")

# Synthetic button id republished to the legacy bus when bridge mode
# is "oled_compat". 6 maps to B2 in the OLED button-action table.
_SYNTHETIC_TAP_BUTTON = 6

# How long to back off between probe attempts when no evdev node is
# bound. The driver may bind a few seconds after the framebuffer.
_PROBE_BACKOFF_SECONDS = 2.0

# Movement filter: only emit a TouchMove event when the latest sample
# differs from the previous accepted one by at least this many pixels
# along either axis. Stops the bridge from flooding the bus when the
# pen is held stationary.
_MOVE_DELTA_PX = 2

# Gesture classifier thresholds (LCD pixels and ms).
_TAP_DUR_MS = 400
_TAP_DISPLACEMENT_PX = 12
_SWIPE_DUR_MS = 250
_SWIPE_DISPLACEMENT_PX = 24

BridgeMode = Literal["lcd_page", "oled_compat"]


def _now_ms() -> int:
    return int(time.monotonic() * 1000)


class TouchInputBridge:
    """Run the touch event loop, publish gestures, and own bridge mode."""

    def __init__(
        self,
        legacy_button_bus: ButtonEventBus | None = None,
        *,
        rotation: int = 0,
        lcd_size: tuple[int, int] = (480, 320),
        calib_path: Path | None = None,
        device_name_hint: str = "ADS7846",
        gesture_bus: TouchEventBus | None = None,
        move_bus: TouchMoveBus | None = None,
    ) -> None:
        self._legacy_button_bus = legacy_button_bus
        self._rotation = rotation
        self._lcd_size = lcd_size
        self._calib_path = calib_path or TOUCH_CALIB_PATH
        self._hint = device_name_hint.lower()
        self._stop = asyncio.Event()
        self._affine: Affine = self._initial_affine()
        self._gesture_bus = gesture_bus or TouchEventBus()
        self._move_bus = move_bus or TouchMoveBus()
        self._mode: BridgeMode = "lcd_page"
        # Per-stroke state machine.
        self._down_at_ms: int = 0
        self._down_x_lcd: int = 0
        self._down_y_lcd: int = 0
        self._last_x_raw: int = 0
        self._last_y_raw: int = 0
        self._last_x_lcd: int = 0
        self._last_y_lcd: int = 0
        self._samples: list[tuple[int, int, int]] = []

    # ── public API ───────────────────────────────────────────────

    @property
    def gesture_bus(self) -> TouchEventBus:
        return self._gesture_bus

    @property
    def move_bus(self) -> TouchMoveBus:
        return self._move_bus

    @property
    def mode(self) -> BridgeMode:
        return self._mode

    @mode.setter
    def mode(self, value: BridgeMode) -> None:
        if value not in ("lcd_page", "oled_compat"):
            raise ValueError(f"unknown bridge mode: {value}")
        self._mode = value
        log.info("touch_bridge_mode_set", mode=value)

    def reload_calibration(self) -> None:
        """Re-read the calibration file from disk.

        Call this after the wizard persists a new file so live taps
        switch to the freshly-fit transform without a service
        restart.
        """
        self._affine = self._initial_affine()
        log.info(
            "touch_bridge_calibration_reloaded",
            calibrated=load_calib(self._calib_path) is not None,
        )

    def request_stop(self) -> None:
        self._stop.set()

    async def run(self) -> None:
        """Probe the evdev node and consume events until cancelled."""
        try:
            from evdev import InputDevice, ecodes, list_devices  # noqa: F401
        except ImportError as exc:  # pragma: no cover - evdev in extras
            log.warning("touch_evdev_import_failed", error=str(exc))
            return

        from evdev import InputDevice, ecodes, list_devices

        while not self._stop.is_set():
            device = self._find_device(InputDevice, list_devices, ecodes)
            if device is None:
                try:
                    await asyncio.wait_for(
                        self._stop.wait(), timeout=_PROBE_BACKOFF_SECONDS
                    )
                except TimeoutError:
                    continue
                else:
                    return
            log.info(
                "touch_bridge_bound",
                path=device.path,
                name=device.name,
                rotation=self._rotation,
                lcd_size=self._lcd_size,
                calibrated=load_calib(self._calib_path) is not None,
            )
            try:
                await self._consume(device, ecodes)
            except OSError as exc:
                log.warning(
                    "touch_bridge_device_lost",
                    path=device.path,
                    error=str(exc),
                )
                continue

    # ── core loop ────────────────────────────────────────────────

    async def _consume(self, device: Any, ecodes: Any) -> None:
        """Drain one bound device and publish gestures until it errors."""
        # Track active raw coordinates between EV_SYN markers.
        pending_x: int | None = None
        pending_y: int | None = None
        pen_down = False
        async for event in device.async_read_loop():
            if self._stop.is_set():
                return
            if event.type == ecodes.EV_KEY and event.code == ecodes.BTN_TOUCH:
                if event.value == 1 and not pen_down:
                    pen_down = True
                    self._open_stroke(_now_ms())
                elif event.value == 0 and pen_down:
                    pen_down = False
                    await self._close_stroke(_now_ms())
                    pending_x = None
                    pending_y = None
                continue
            if event.type == ecodes.EV_ABS:
                if event.code == ecodes.ABS_X:
                    pending_x = int(event.value)
                elif event.code == ecodes.ABS_Y:
                    pending_y = int(event.value)
                continue
            if event.type == ecodes.EV_SYN:
                if pen_down and pending_x is not None and pending_y is not None:
                    await self._record_move(pending_x, pending_y, _now_ms())
                continue

    # ── stroke lifecycle ────────────────────────────────────────

    def _open_stroke(self, now_ms: int) -> None:
        self._down_at_ms = now_ms
        self._samples = []
        # Defer recording until the first ABS pair lands; only then do
        # we have a coordinate.
        self._down_x_lcd = self._last_x_lcd = 0
        self._down_y_lcd = self._last_y_lcd = 0
        self._last_x_raw = 0
        self._last_y_raw = 0

    async def _record_move(
        self, x_raw: int, y_raw: int, now_ms: int,
    ) -> None:
        """Apply transform, emit TouchMove if the delta is significant."""
        x_lcd, y_lcd = self._affine.apply(x_raw, y_raw)
        # First sample of the stroke seeds the down-position.
        if not self._samples:
            self._down_x_lcd = x_lcd
            self._down_y_lcd = y_lcd
            self._last_x_lcd = x_lcd
            self._last_y_lcd = y_lcd
            self._last_x_raw = x_raw
            self._last_y_raw = y_raw
            self._samples.append((x_lcd, y_lcd, now_ms))
            if self._mode == "lcd_page":
                await self._move_bus.publish(
                    TouchMove(x_lcd=x_lcd, y_lcd=y_lcd, timestamp_ms=now_ms)
                )
            return
        dx = abs(x_lcd - self._last_x_lcd)
        dy = abs(y_lcd - self._last_y_lcd)
        if dx < _MOVE_DELTA_PX and dy < _MOVE_DELTA_PX:
            return
        self._last_x_lcd = x_lcd
        self._last_y_lcd = y_lcd
        self._last_x_raw = x_raw
        self._last_y_raw = y_raw
        self._samples.append((x_lcd, y_lcd, now_ms))
        if self._mode == "lcd_page":
            await self._move_bus.publish(
                TouchMove(x_lcd=x_lcd, y_lcd=y_lcd, timestamp_ms=now_ms)
            )

    async def _close_stroke(self, now_ms: int) -> None:
        """Classify and emit the gesture, then reset the per-stroke state."""
        if not self._samples:
            # Pen-up arrived before any ABS sample landed (driver
            # quirk). Nothing to emit.
            return
        end_x_lcd, end_y_lcd, _ = self._samples[-1]
        duration_ms = max(0, now_ms - self._down_at_ms)
        dx_total = end_x_lcd - self._down_x_lcd
        dy_total = end_y_lcd - self._down_y_lcd
        displacement = (dx_total * dx_total + dy_total * dy_total) ** 0.5
        velocity = 0.0
        if duration_ms > 0:
            velocity = displacement * 1000.0 / duration_ms

        kind = self._classify(duration_ms, displacement)
        direction = self._direction_for(dx_total, dy_total) if kind in (
            "swipe", "drag"
        ) else None

        gesture = TouchGesture(
            kind=kind,
            start_x=self._down_x_lcd,
            start_y=self._down_y_lcd,
            end_x=end_x_lcd,
            end_y=end_y_lcd,
            start_t_ms=self._down_at_ms,
            end_t_ms=now_ms,
            duration_ms=duration_ms,
            direction=direction,
            velocity_px_per_s=velocity,
            samples=tuple(self._samples),
        )

        if self._mode == "lcd_page":
            await self._gesture_bus.publish(gesture)

        # Backwards-compat: republish taps as ButtonEvent on the
        # legacy bus so the OLED carousel keeps advancing on
        # touch-only boards even before the page system takes over.
        if (
            kind == "tap"
            and self._legacy_button_bus is not None
            and self._mode == "oled_compat"
        ):
            try:
                from ados.services.ui.events import ButtonEvent

                await self._legacy_button_bus.publish(
                    ButtonEvent(
                        button=_SYNTHETIC_TAP_BUTTON,
                        kind="short",
                        timestamp_ms=now_ms,
                        action="next_screen",
                    )
                )
            except Exception as exc:  # noqa: BLE001
                log.debug("touch_bridge_legacy_publish_failed", error=str(exc))

    # ── classification ───────────────────────────────────────────

    def _classify(self, duration_ms: int, displacement: float) -> GestureKind:
        if displacement < _TAP_DISPLACEMENT_PX:
            return "long_press" if duration_ms >= _TAP_DUR_MS else "tap"
        if (
            duration_ms < _SWIPE_DUR_MS
            and displacement >= _SWIPE_DISPLACEMENT_PX
        ):
            return "swipe"
        return "drag"

    def _direction_for(self, dx: int, dy: int) -> Direction:
        if abs(dx) >= abs(dy):
            return "right" if dx >= 0 else "left"
        return "down" if dy >= 0 else "up"

    # ── helpers ──────────────────────────────────────────────────

    def _initial_affine(self) -> Affine:
        loaded = load_calib(self._calib_path)
        if loaded is not None:
            return loaded
        return identity_for(self._rotation, self._lcd_size)

    def _find_device(self, input_device: Any, list_devices: Any, ecodes: Any) -> Any:
        # ``input_device`` mirrors the evdev ``InputDevice`` class. We
        # accept it as an argument so unit tests can substitute a fake.
        for path in list_devices():
            try:
                dev = input_device(path)
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
