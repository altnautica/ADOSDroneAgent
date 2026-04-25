"""Front-panel button service.

Reads the four ground-station front-panel buttons on GPIO 5, 6, 13,
and 19 (BCM). Publishes short and long press events on a shared
`ButtonEventBus`. The OLED service consumes the bus.

Hardware contract:
- Pins 5, 6, 13, 19 per `hal/boards/rpi4b.yaml` `gpio_buttons`.
  Hardcoded here as a fallback. Future revisions load from the HAL
  profile.
- Internal pull-up, active-low (button press pulls pin to ground).
- 20 ms debounce via gpiozero `bounce_time=0.02` plus a recent-edge
  guard inside the callback.

Press classification:
- Short press: release within 2 s of press. Fires on release.
- Long press: held for 2 s or more, fires on release (not at the 2 s
  mark). This lets a user change their mind by continuing to hold.
- Cancel: held for 6 s or more. The event is dropped and a log line
  records the cancel so operators can see they held too long.

Failure behavior:
- If gpiozero fails to import or the button factory fails to init,
  log a clear error and exit non-zero. systemd restart-on-failure
  surfaces the problem.
"""

from __future__ import annotations

import asyncio
import signal
import sys
import threading
import time
from typing import Any

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.services.ui.events import ButtonEvent, ButtonEventBus

log = get_logger("ui.button_service")

# Fallback pin list. Future revisions will pull this from the HAL
# profile (`hal/boards/rpi4b.yaml` `gpio_buttons`).
BUTTON_PINS: list[int] = [5, 6, 13, 19]

# Press classification thresholds (seconds).
LONG_PRESS_SECONDS: float = 2.0
CANCEL_HOLD_SECONDS: float = 6.0

# Debounce window passed to gpiozero.
BOUNCE_TIME: float = 0.02

# BCM pin -> friendly button id used by the REST schema.
# `B1`, `B2`, `B3`, `B4` correspond to the four front-panel buttons in
# the order documented at `hal/boards/rpi4b.yaml`. The REST mapping
# uses keys like `B1_short`, `B2_long`. Anything outside this table is
# resolved as `BX_short` / `BX_long` where X is the BCM pin number, so
# extra GPIO buttons added later still get a stable mapping key.
PIN_TO_LABEL: dict[int, str] = {5: "B1", 6: "B2", 13: "B3", 19: "B4"}

# Default mapping. Used when `ground_station.ui.buttons.mapping` is empty
# or missing. Mirrors the REST defaults at
# `api/routes/ground_station.py::_DEFAULT_BUTTONS`.
DEFAULT_BUTTON_MAPPING: dict[str, str] = {
    "B1_short": "cycle_screen",
    "B1_long": "toggle_backlight",
    "B2_short": "show_network",
    "B2_long": "show_qr",
    "B3_short": "confirm",
    "B3_long": "pair_drone",
    "B4_short": "back",
    "B4_long": "menu",
}


def _now_ms() -> int:
    """Monotonic wall time in milliseconds."""
    return int(time.monotonic() * 1000)


class ButtonService:
    """Wires gpiozero.Button objects to a `ButtonEventBus`.

    Press timing is tracked per pin. Callbacks run in gpiozero's
    worker thread; they schedule the publish onto the asyncio loop
    via `loop.call_soon_threadsafe`.
    """

    def __init__(
        self,
        bus: ButtonEventBus,
        pins: list[int] | None = None,
        loop: asyncio.AbstractEventLoop | None = None,
    ) -> None:
        self._bus = bus
        self._pins = pins if pins is not None else list(BUTTON_PINS)
        self._loop = loop or asyncio.get_event_loop()
        self._press_ms: dict[int, int] = {}
        self._buttons: list = []  # holds gpiozero.Button refs
        # Live action mapping rebuilt on SIGHUP.
        # The mapping is read from the gpiozero worker thread in
        # `_resolve_action` and written from the asyncio SIGHUP handler
        # in `reload_mapping`. A threading.Lock guards the swap so a
        # release callback never sees a half-built dict.
        self._mapping_lock = threading.Lock()
        self._mapping: dict[str, str] = dict(DEFAULT_BUTTON_MAPPING)
        self.reload_mapping()

    def _label_for_pin(self, pin: int) -> str:
        """Return `B1`..`B4` for known pins, else `BX<pin>` for extras."""
        return PIN_TO_LABEL.get(pin, f"BX{pin}")

    def reload_mapping(self) -> None:
        """Rebuild `self._mapping` from `ground_station.ui.buttons.mapping`.

        Tolerates a missing ground_station block, a missing ui block, or
        an empty mapping by falling back to `DEFAULT_BUTTON_MAPPING`.
        Keys in the loaded mapping override the defaults one by one so
        a partial remap (e.g. user only changed `B1_short`) keeps the
        rest of the defaults intact.
        """
        merged = dict(DEFAULT_BUTTON_MAPPING)
        try:
            cfg = load_config()
            ui = getattr(getattr(cfg, "ground_station", None), "ui", None)
            buttons_cfg: Any = getattr(ui, "buttons", {}) if ui is not None else {}
            if isinstance(buttons_cfg, dict):
                raw_map = buttons_cfg.get("mapping", {})
                if isinstance(raw_map, dict):
                    for k, v in raw_map.items():
                        if isinstance(k, str) and isinstance(v, str):
                            merged[k] = v
        except Exception as exc:
            log.warning("button_mapping_reload_failed", error=str(exc))
        with self._mapping_lock:
            self._mapping = merged
        log.info("button_mapping_loaded", entries=len(merged))

    def _resolve_action(self, pin: int, kind: str) -> str | None:
        """Look up the action name for a (pin, kind) pair. None when unmapped."""
        key = f"{self._label_for_pin(pin)}_{kind}"
        with self._mapping_lock:
            return self._mapping.get(key)

    def start(self) -> None:
        """Attach gpiozero callbacks to every pin.

        Raises on import or init failure so `main()` can exit non-zero.
        """
        try:
            from gpiozero import Button
        except Exception as exc:
            log.error("gpiozero import failed", error=str(exc))
            raise

        for pin in self._pins:
            try:
                btn = Button(
                    pin,
                    pull_up=True,
                    bounce_time=BOUNCE_TIME,
                )
            except Exception as exc:
                log.error("button init failed", pin=pin, error=str(exc))
                raise

            btn.when_pressed = self._make_press_handler(pin)
            btn.when_released = self._make_release_handler(pin)
            self._buttons.append(btn)
            log.info("button attached", pin=pin)

    def _make_press_handler(self, pin: int):
        def _on_press() -> None:
            now = _now_ms()
            # Recent-edge guard. Ignore a new press edge if the last
            # press on this pin was inside the debounce window.
            last = self._press_ms.get(pin, 0)
            if now - last < int(BOUNCE_TIME * 1000):
                return
            self._press_ms[pin] = now
            log.debug("button press edge", pin=pin, ts_ms=now)
        return _on_press

    def _make_release_handler(self, pin: int):
        def _on_release() -> None:
            now = _now_ms()
            press_ms = self._press_ms.pop(pin, None)
            if press_ms is None:
                # Release without a recorded press. Likely a spurious
                # edge. Drop it.
                return
            held_ms = now - press_ms
            held_s = held_ms / 1000.0

            if held_s >= CANCEL_HOLD_SECONDS:
                log.info(
                    "button cancel (held too long)",
                    pin=pin,
                    held_seconds=round(held_s, 2),
                )
                return

            kind: str = "long" if held_s >= LONG_PRESS_SECONDS else "short"
            action = self._resolve_action(pin, kind)
            event = ButtonEvent(  # type: ignore[arg-type]
                button=pin,
                kind=kind,
                timestamp_ms=now,
                action=action,
            )
            log.info(
                "button event",
                pin=pin,
                label=self._label_for_pin(pin),
                kind=kind,
                action=action,
                held_seconds=round(held_s, 2),
            )
            # Schedule the publish onto the asyncio loop from the
            # gpiozero worker thread.
            asyncio.run_coroutine_threadsafe(
                self._bus.publish(event),
                self._loop,
            )
        return _on_release

    def stop(self) -> None:
        for btn in self._buttons:
            try:
                btn.close()
            except Exception:
                pass
        self._buttons.clear()


async def _run() -> int:
    bus = ButtonEventBus()
    service = ButtonService(bus=bus, pins=list(BUTTON_PINS), loop=asyncio.get_event_loop())

    try:
        service.start()
    except Exception as exc:
        log.error("button service failed to start", error=str(exc))
        return 1

    log.info(
        "button service running",
        pins=BUTTON_PINS,
        long_press_seconds=LONG_PRESS_SECONDS,
        cancel_hold_seconds=CANCEL_HOLD_SECONDS,
    )

    stop_event = asyncio.Event()

    def _signal_stop(*_args) -> None:
        log.info("button service stopping on signal")
        stop_event.set()

    loop = asyncio.get_event_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, _signal_stop)
        except NotImplementedError:
            # Windows or restricted environments. Fall through.
            signal.signal(sig, _signal_stop)

    def _signal_reload(*_args) -> None:
        log.info("button service reloading mapping on SIGHUP")
        service.reload_mapping()

    try:
        loop.add_signal_handler(signal.SIGHUP, _signal_reload)
    except (NotImplementedError, AttributeError):
        try:
            signal.signal(signal.SIGHUP, _signal_reload)
        except (AttributeError, ValueError):
            pass

    try:
        await stop_event.wait()
    finally:
        service.stop()
        await bus.close()

    return 0


def main() -> None:
    """Entry point for `python -m ados.services.ui.button_service`."""
    configure_logging()
    try:
        rc = asyncio.run(_run())
    except KeyboardInterrupt:
        rc = 0
    sys.exit(rc)


if __name__ == "__main__":
    main()
