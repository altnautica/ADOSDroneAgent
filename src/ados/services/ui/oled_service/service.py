"""OLED service shim — public surface for the package.

The real implementation is split across sibling modules within this
package:

* :mod:`.framebuffer` — :class:`_FramebufferMixin` (device probe,
  framebuffer probe, render loop, SPI LCD paint).
* :mod:`.touch` — :class:`_TouchMixin` (calibration wizard, gesture
  dispatch, evdev touch probe).
* :mod:`.buttons` — :class:`_ButtonsMixin` (button bus consumer,
  overlay lifecycle, pairing-window secondary poll).
* :mod:`.display` — :class:`_DisplayMixin` (screen-paint dispatcher,
  OLED knobs, SIGHUP config reload, role badge).
* :mod:`.lifecycle` — :class:`OledService` itself, composing the four
  mixins plus the orchestrator ``run()`` and page-system bootstrap.

This module exposes :class:`OledService`, :func:`main`, :func:`_amain`,
and the package logger :data:`log` so the entry points in
``__main__.py`` and the existing test suite (which imports
``from ados.services.ui.oled_service import service as oled_service``
and monkeypatches ``load_config`` / ``configure_logging`` /
``ButtonEventBus`` / ``OledService`` on this module) keep working
without changes.
"""

from __future__ import annotations

import asyncio
import signal
import sys
from typing import Any

from ados.core.config import load_config
from ados.core.logging import configure_logging, get_logger
from ados.services.ui.events import ButtonEventBus

log = get_logger("ui.oled_service")

# Lifecycle imports `log` from this module at call time, so the logger
# binding above must be in place before the lifecycle module is loaded.
from .lifecycle import OledService  # noqa: E402

__all__ = [
    "ButtonEventBus",
    "OledService",
    "_amain",
    "configure_logging",
    "load_config",
    "log",
    "main",
]


def _display_conf_disabled() -> bool:
    """True when /etc/ados/display.conf explicitly disables the display.

    Defense-in-depth alongside the systemd ConditionPathExists marker gate:
    if the overlay installer (or the boot-time probe's auto-revert) wrote
    display_id=none, exit 0 cleanly rather than probing for a panel that the
    installer already decided is absent. Read failures are non-fatal — the
    later probe-and-exit-clean path still handles a genuinely empty board.
    """
    from ados.core.paths import DISPLAY_CONF_PATH

    try:
        text = DISPLAY_CONF_PATH.read_text()
    except OSError:
        return False
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("display_id="):
            return line.partition("=")[2].strip() == "none"
    return False


async def _amain() -> int:
    cfg = load_config()
    configure_logging(cfg.logging.level)

    # Honour the overlay installer's verdict. When display.conf says
    # display_id=none (no panel detected, or the boot probe auto-reverted an
    # unconfirmed overlay), exit 0 so the systemd unit goes inactive cleanly
    # instead of running a render loop against absent hardware.
    if _display_conf_disabled():
        log.info("oled_skipped_display_disabled", reason="display_id=none")
        return 0

    # Honour the operator-set local-display primary path. On boards
    # that boot with both HDMI and the SPI LCD wired (or that ship
    # headless), letting the OLED / framebuffer renderers grab the
    # panel anyway fights the kiosk service for the display. Gate
    # early so the systemd unit exits cleanly and stays inactive.
    display_type = getattr(
        getattr(cfg.ground_station, "display", None), "type", "auto"
    )
    if display_type in ("hdmi", "none"):
        log.info(
            "oled_skipped_due_to_display_config",
            display_type=display_type,
        )
        return 0

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
