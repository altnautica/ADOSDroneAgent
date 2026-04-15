"""Front-panel button service (MSN-025 Wave A).

Reads the four ground-station front-panel buttons on GPIO 5, 6, 13,
and 19 (BCM). Publishes short and long press events on a shared
`ButtonEventBus`. Wave B OLED service consumes the bus.

Hardware contract:
- Pins 5, 6, 13, 19 per `hal/boards/rpi4b.yaml` `gpio_buttons`.
  Hardcoded here as a Phase 1 fallback. Future phases load from HAL
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
import time

from ados.core.logging import configure_logging, get_logger
from ados.services.ui.events import ButtonEvent, ButtonEventBus

log = get_logger("ui.button_service")

# Phase 1 fallback pin list. Wave D or later will pull this from the
# HAL profile (`hal/boards/rpi4b.yaml` `gpio_buttons`).
BUTTON_PINS: list[int] = [5, 6, 13, 19]

# Press classification thresholds (seconds).
LONG_PRESS_SECONDS: float = 2.0
CANCEL_HOLD_SECONDS: float = 6.0

# Debounce window passed to gpiozero.
BOUNCE_TIME: float = 0.02


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
            event = ButtonEvent(button=pin, kind=kind, timestamp_ms=now)  # type: ignore[arg-type]
            log.info(
                "button event",
                pin=pin,
                kind=kind,
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
